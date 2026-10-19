use gpui::{
    fill, font, outline, px, relative, size, App, BorderStyle, Bounds, CursorStyle, DispatchPhase,
    Element, ElementId, Entity, Font, FontStyle, FontWeight, GlobalElementId, Hitbox,
    HitboxBehavior, Hsla, InputHandler, InspectorElementId, IntoElement, LayoutId, Modifiers,
    MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, Point, ShapedLine, SharedString,
    StrikethroughStyle, Style, TextAlign, TextRun, UTF16Selection, UnderlineStyle, WeakEntity,
    Window,
};
use gpui_component::ActiveTheme as _;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;

use crate::preferences::{Preferences, TerminalFont};
use crate::shell::{GridPoint, GridSelectionRange, SelectionSide, TermSnapshot};
use crate::theme as flint_theme;
use crate::ui::terminal_link::TerminalLink;
use crate::ui::terminal_palette::{self, ColorKey, TerminalColors};
use crate::ui::terminal_view::{ScrollbarGeom, TerminalView, SIDE_PADDING};

/// Height of a terminal row, as a multiple of the font size.
pub(crate) const LINE_HEIGHT_FACTOR: f32 = 1.3;

pub struct TerminalElement {
    view: Entity<TerminalView>,
}

impl TerminalElement {
    pub fn new(view: Entity<TerminalView>) -> Self {
        Self { view }
    }
}

pub fn pixel_to_grid(
    pos: Point<Pixels>,
    bounds: Bounds<Pixels>,
    cell_width: Pixels,
    line_height: Pixels,
    display_offset: usize,
) -> (GridPoint, SelectionSide) {
    let rel_x = (pos.x - bounds.origin.x).as_f32().max(0.0_f32);
    let rel_y = (pos.y - bounds.origin.y).as_f32().max(0.0_f32);

    let viewport_line = (rel_y / line_height.as_f32()).floor() as i32;
    let absolute_line = viewport_line - display_offset as i32;
    let col_f = rel_x / cell_width.as_f32();
    let column = col_f.floor() as usize;

    let side = if col_f - col_f.floor() < 0.5 {
        SelectionSide::Left
    } else {
        SelectionSide::Right
    };

    (
        GridPoint {
            line: absolute_line,
            column,
        },
        side,
    )
}

/// Pull a viewport point back onto a grid of `rows` by `cols` cells.
pub(crate) fn clamp_to_grid(point: GridPoint, rows: u16, cols: u16) -> GridPoint {
    GridPoint {
        line: point.line.clamp(0, (rows as i32 - 1).max(0)),
        column: point.column.min((cols as usize).saturating_sub(1)),
    }
}

/// Byte offset in `text` of a UTF-16 offset, which is what the platform's
/// input method counts in. An offset past the end gives the end; one inside a
/// surrogate pair gives the start of that character.
fn utf16_to_byte_offset(text: &str, offset_utf16: usize) -> usize {
    let mut units = 0;
    for (byte_offset, character) in text.char_indices() {
        if units + character.len_utf16() > offset_utf16 {
            return byte_offset;
        }
        units += character.len_utf16();
    }
    text.len()
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

#[derive(Clone)]
pub struct LayoutRect {
    line: i32,
    start_col: i32,
    num_cells: usize,
    color: Hsla,
}

#[derive(Clone)]
pub struct BatchedTextRun {
    line: i32,
    start_col: i32,
    text: SharedString,
    font_size: Pixels,
    style: TextRun,
    is_wide: bool,
}

#[derive(Clone)]
pub struct CursorLayout {
    line: i32,
    col: i32,
    fg_under: Hsla,
    bg_under: Hsla,
    char_under: char,
}

/// Cell-grid geometry derived from a `TermSnapshot`. Building this walks every
/// cell. It is an intermediate that gets shaped into a [`CachedGrid`].
pub struct GridLayout {
    pub bg_rects: Vec<LayoutRect>,
    pub text_runs: Vec<BatchedTextRun>,
    pub cursor: Option<CursorLayout>,
}

/// Shaped lines by their text. The same text in another style is a separate
/// entry, so the list per text is almost always one long.
#[derive(Default)]
pub struct ShapedLines(HashMap<SharedString, Vec<ShapedEntry>>);

struct ShapedEntry {
    style: TextRun,
    font_size: Pixels,
    is_wide: bool,
    shaped: ShapedLine,
}

impl ShapedLines {
    fn get(&self, run: &BatchedTextRun) -> Option<ShapedLine> {
        self.0.get(&run.text)?.iter().find_map(|entry| {
            let is_same = entry.style == run.style
                && entry.font_size == run.font_size
                && entry.is_wide == run.is_wide;
            is_same.then(|| entry.shaped.clone())
        })
    }

    fn insert(&mut self, run: &BatchedTextRun, shaped: ShapedLine) {
        self.0
            .entry(run.text.clone())
            .or_default()
            .push(ShapedEntry {
                style: run.style.clone(),
                font_size: run.font_size,
                is_wide: run.is_wide,
                shaped,
            });
    }
}

/// A text run already shaped into glyphs, positioned by grid coordinates.
struct ShapedRun {
    line: i32,
    start_col: i32,
    shaped: ShapedLine,
}

/// Cell-grid geometry plus shaped text lines for a `TermSnapshot`.
///
/// Building this walks every cell and shapes every run, so it is cached on the
/// `TerminalView`, keyed by `snapshot_version`, in an `Rc`. Repaints that leave
/// the terminal contents alone (resize drags, hover, focus changes, sibling
/// re-renders while a session connects) reuse it.
pub struct CachedGrid {
    bg_rects: Vec<LayoutRect>,
    shaped_runs: Vec<ShapedRun>,
    cursor: Option<CursorLayout>,
}

pub struct LayoutState {
    cell_width: Pixels,
    line_height: Pixels,
    font_size: Pixels,
    term_font: Font,
    colors: TerminalColors,
    /// Covers the whole grid. Tells whether the pointer is really on the
    /// terminal and not on something drawn over it, and carries the pointer
    /// cursor shown over a link.
    hitbox: Hitbox,
    cached: Rc<CachedGrid>,
    selection_range: Option<GridSelectionRange>,
    /// Search hits on the visible rows. Kept out of `CachedGrid` on purpose: they
    /// change on every keystroke in the search bar while the grid does not, and
    /// the cache is what keeps repaints from re-walking and re-shaping the screen.
    search_matches: Vec<GridSelectionRange>,
    current_match: Option<GridSelectionRange>,
    total_cols: usize,
    display_offset: usize,
    history_size: usize,
    rows: usize,
}

/// The rectangle a range covers on each row. Rows outside the viewport are
/// skipped, so off-screen ranges cost nothing.
fn range_row_bounds(
    range: &GridSelectionRange,
    origin: Point<Pixels>,
    layout: &LayoutState,
) -> impl Iterator<Item = Bounds<Pixels>> {
    let range = *range;
    let total_cols = layout.total_cols;
    let display_offset = layout.display_offset as i32;
    let rows = layout.rows as i32;
    let cell_width = layout.cell_width;
    let line_height = layout.line_height;

    (range.start.line..=range.end.line).filter_map(move |abs_line| {
        let viewport_row = abs_line + display_offset;
        if viewport_row < 0 || viewport_row >= rows {
            return None;
        }

        let (start_col, end_col) = if abs_line == range.start.line && abs_line == range.end.line {
            (range.start.column, range.end.column.saturating_add(1))
        } else if abs_line == range.start.line {
            (range.start.column, total_cols)
        } else if abs_line == range.end.line {
            (0usize, range.end.column.saturating_add(1))
        } else {
            (0usize, total_cols)
        };

        let start_col = start_col.min(total_cols);
        let end_col = end_col.min(total_cols);
        if start_col >= end_col {
            return None;
        }

        let x = origin.x + cell_width * start_col as f32;
        let y = origin.y + line_height * viewport_row as f32;
        let w = cell_width * (end_col - start_col) as f32;
        Some(Bounds::new(Point::new(x, y), size(w, line_height)))
    })
}

/// Paint one highlight band per row a range covers.
fn paint_range(
    range: &GridSelectionRange,
    color: Hsla,
    origin: Point<Pixels>,
    layout: &LayoutState,
    window: &mut Window,
) {
    for row_bounds in range_row_bounds(range, origin, layout) {
        window.paint_quad(fill(row_bounds, color));
    }
}

/// Underline a link the pointer is on, one line per row it covers.
fn paint_link_underline(
    range: &GridSelectionRange,
    origin: Point<Pixels>,
    layout: &LayoutState,
    window: &mut Window,
) {
    let thickness = px(1.0);
    for row_bounds in range_row_bounds(range, origin, layout) {
        // Two pixels up from the cell's bottom edge, where text underlines sit.
        let y = row_bounds.origin.y + row_bounds.size.height - px(2.0);
        window.paint_quad(fill(
            Bounds::new(
                Point::new(row_bounds.origin.x, y),
                size(row_bounds.size.width, thickness),
            ),
            layout.colors.fg,
        ));
    }
}

/// The link to draw as clickable this frame. `None` while something drawn over
/// the terminal (a dialog, the search bar) has the pointer.
fn hovered_link(
    view: &TerminalView,
    hitbox: &Hitbox,
    position: Point<Pixels>,
    modifiers: Modifiers,
    window: &Window,
) -> Option<TerminalLink> {
    if !hitbox.is_hovered(window) {
        return None;
    }
    view.hovered_link(position, modifiers)
}

/// Ask for a repaint when the link under the pointer is not the one on screen.
/// Idle mouse movement without the modifier compares `None` to `None` and
/// costs nothing.
fn repaint_if_link_changed(
    view: &Entity<TerminalView>,
    hitbox: &Hitbox,
    painted: Option<GridSelectionRange>,
    position: Point<Pixels>,
    modifiers: Modifiers,
    window: &Window,
    cx: &mut App,
) {
    let hovered = hovered_link(view.read(cx), hitbox, position, modifiers, window);
    if hovered.map(|link| link.range) != painted {
        cx.notify(view.entity_id());
    }
}

struct TerminalInputHandler {
    view: WeakEntity<TerminalView>,
    cursor_bounds: Option<Bounds<Pixels>>,
}

impl InputHandler for TerminalInputHandler {
    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<UTF16Selection> {
        None
    }

    fn marked_text_range(&mut self, _window: &mut Window, cx: &mut App) -> Option<Range<usize>> {
        let view = self.view.upgrade()?;
        let marked = view.read(cx).marked_text.as_ref()?;
        Some(0..utf16_len(marked))
    }

    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Option<String> {
        let view = self.view.upgrade()?;
        let marked = view.read(cx).marked_text.clone()?;
        let start = utf16_to_byte_offset(&marked, range_utf16.start);
        let end = utf16_to_byte_offset(&marked, range_utf16.end).max(start);
        *adjusted_range = Some(utf16_len(&marked[..start])..utf16_len(&marked[..end]));
        Some(marked[start..end].to_string())
    }

    fn replace_text_in_range(
        &mut self,
        _replacement_range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut App,
    ) {
        let _ = self.view.update(cx, |view, cx| {
            view.clear_marked_text(cx);
            view.commit_text(text, cx);
        });
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range_utf16: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut App,
    ) {
        let _ = self.view.update(cx, |view, cx| {
            // An input method ends a composition by marking an empty text.
            if new_text.is_empty() {
                view.clear_marked_text(cx);
                return;
            }
            view.set_marked_text(new_text.to_string(), cx);
        });
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut App) {
        if let Some(view) = self.view.upgrade() {
            view.update(cx, |view, cx| view.clear_marked_text(cx));
        }
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        self.cursor_bounds
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<usize> {
        None
    }

    fn accepts_text_input(&mut self, _window: &mut Window, _cx: &mut App) -> bool {
        true
    }

    fn prefers_ime_for_printable_keys(&mut self, _window: &mut Window, _cx: &mut App) -> bool {
        false
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum BgKey {
    Normal(ColorKey),
    Inverted(ColorKey),
}

#[derive(PartialEq, Eq, Clone, Copy, Default)]
struct TextStyleKey {
    // ColorKey of the effective fg after inverse swap.
    fg_source: ColorKey,
    // Reverse video with default colours has the same `fg_source` as plain
    // text next to it, and would be drawn in that text's colour: light on
    // light. Text pasted into bash or zsh is highlighted exactly this way.
    inverse: bool,
    bold: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
    dim: bool,
}

/// The default face is the one the rest of the app uses. A system monospace
/// such as Menlo has a larger x-height and a wider advance, so it reads as a
/// size up from the file panes right above it at the same point size. The
/// bundled file is a variable font with a 200-700 weight axis, so bold output
/// gets a real weight, not a synthesised one.
///
/// A saved font can be missing: it was removed, or the preferences file came
/// from another machine. gpui would then fall back to a proportional font,
/// so the bundled face is used instead.
pub(crate) fn terminal_font_family(cx: &App) -> &'static str {
    let family = Preferences::terminal_font(cx).family();
    let text_system = cx.text_system();
    let resolved = text_system.get_font_for_id(text_system.resolve_font(&font(family)));
    if resolved.is_some_and(|resolved| resolved.family == family) {
        return family;
    }
    TerminalFont::default().family()
}

fn layout_grid(
    snapshot: &TermSnapshot,
    term_font: &Font,
    font_size: Pixels,
    colors: &TerminalColors,
) -> GridLayout {
    let mut bg_rects: Vec<LayoutRect> = Vec::new();
    let mut text_runs: Vec<BatchedTextRun> = Vec::new();
    let mut cursor: Option<CursorLayout> = None;

    // BG batch state
    let mut bg_active = false;
    let mut bg_line = 0i32;
    let mut bg_start = 0i32;
    let mut bg_count = 0usize;
    let mut bg_key = BgKey::Normal(ColorKey::Default);
    let mut bg_color = colors.bg;

    // Text batch state
    let mut txt_active = false;
    let mut txt_line = 0i32;
    let mut txt_start = 0i32;
    let mut txt_cells = 0i32;
    let mut txt_buf = String::new();
    let mut txt_key = TextStyleKey::default();
    let mut txt_color = colors.fg;
    let mut txt_font = term_font.clone();
    let mut txt_underline: Option<UnderlineStyle> = None;
    let mut txt_strikethrough: Option<StrikethroughStyle> = None;

    let cursor_row = snapshot.cursor.0 as i32;
    let cursor_col = snapshot.cursor.1 as i32;

    for (row_idx, row) in snapshot.cells.iter().enumerate() {
        let line = row_idx as i32;

        // Flush batches at row boundary.
        if bg_active {
            bg_rects.push(LayoutRect {
                line: bg_line,
                start_col: bg_start,
                num_cells: bg_count,
                color: bg_color,
            });
            bg_active = false;
        }
        if txt_active && !txt_buf.is_empty() {
            text_runs.push(BatchedTextRun {
                line: txt_line,
                start_col: txt_start,
                text: SharedString::from(txt_buf.clone()),
                font_size,
                style: TextRun {
                    len: txt_buf.len(),
                    font: txt_font.clone(),
                    color: txt_color,
                    background_color: None,
                    underline: txt_underline,
                    strikethrough: txt_strikethrough,
                },
                is_wide: false,
            });
            txt_buf.clear();
            txt_active = false;
        }

        let mut col_idx = 0;
        while col_idx < row.len() {
            let cell = &row[col_idx];
            let col = col_idx as i32;

            if cell.wide_char_spacer {
                col_idx += 1;
                continue;
            }

            // Inverse swaps the two colours first, then dim fades the foreground.
            let mut fg = colors.resolve(&cell.fg, false);
            let mut bg = colors.resolve(&cell.bg, true);
            if cell.inverse {
                std::mem::swap(&mut fg, &mut bg);
            }
            if cell.dim {
                fg.a *= 0.7;
            }

            let cell_bg_key = if cell.inverse {
                BgKey::Inverted(terminal_palette::color_key(&cell.fg))
            } else {
                BgKey::Normal(terminal_palette::color_key(&cell.bg))
            };
            let cell_txt_key = TextStyleKey {
                fg_source: if cell.inverse {
                    terminal_palette::color_key(&cell.bg)
                } else {
                    terminal_palette::color_key(&cell.fg)
                },
                inverse: cell.inverse,
                bold: cell.bold,
                italic: cell.italic,
                underline: cell.underline,
                strikethrough: cell.strikethrough,
                dim: cell.dim,
            };

            let mut cell_font = term_font.clone();
            if cell.bold {
                cell_font.weight = FontWeight::BOLD;
            }
            if cell.italic {
                cell_font.style = FontStyle::Italic;
            }
            let cell_underline = if cell.underline {
                Some(UnderlineStyle {
                    thickness: px(1.0),
                    color: Some(fg),
                    wavy: false,
                })
            } else {
                None
            };
            let cell_strikethrough = if cell.strikethrough {
                Some(StrikethroughStyle {
                    thickness: px(1.0),
                    color: Some(fg),
                })
            } else {
                None
            };

            // Record cursor position when visible and viewport is at bottom.
            if snapshot.cursor_visible
                && snapshot.display_offset == 0
                && line == cursor_row
                && col == cursor_col
            {
                cursor = Some(CursorLayout {
                    line,
                    col,
                    fg_under: fg,
                    bg_under: bg,
                    char_under: cell.c,
                });
            }

            // Wide char: flush current batches, emit separate runs spanning 2 cells.
            if cell.wide_char {
                if bg_active {
                    bg_rects.push(LayoutRect {
                        line: bg_line,
                        start_col: bg_start,
                        num_cells: bg_count,
                        color: bg_color,
                    });
                    bg_active = false;
                }
                if txt_active && !txt_buf.is_empty() {
                    text_runs.push(BatchedTextRun {
                        line: txt_line,
                        start_col: txt_start,
                        text: SharedString::from(txt_buf.clone()),
                        font_size,
                        style: TextRun {
                            len: txt_buf.len(),
                            font: txt_font.clone(),
                            color: txt_color,
                            background_color: None,
                            underline: txt_underline,
                            strikethrough: txt_strikethrough,
                        },
                        is_wide: false,
                    });
                    txt_buf.clear();
                    txt_active = false;
                }

                let char_str = cell.c.to_string();
                let char_len = char_str.len();
                text_runs.push(BatchedTextRun {
                    line,
                    start_col: col,
                    text: SharedString::from(char_str),
                    font_size,
                    style: TextRun {
                        len: char_len,
                        font: cell_font,
                        color: fg,
                        background_color: None,
                        underline: cell_underline,
                        strikethrough: cell_strikethrough,
                    },
                    is_wide: true,
                });
                bg_rects.push(LayoutRect {
                    line,
                    start_col: col,
                    num_cells: 2,
                    color: bg,
                });

                col_idx += 2;
                continue;
            }

            // Normal cell: background batch.
            if bg_active && bg_key == cell_bg_key && bg_start + bg_count as i32 == col {
                bg_count += 1;
            } else {
                if bg_active {
                    bg_rects.push(LayoutRect {
                        line: bg_line,
                        start_col: bg_start,
                        num_cells: bg_count,
                        color: bg_color,
                    });
                }
                bg_active = true;
                bg_line = line;
                bg_start = col;
                bg_count = 1;
                bg_key = cell_bg_key;
                bg_color = bg;
            }

            // Normal cell: text batch.
            if txt_active && txt_key == cell_txt_key && txt_start + txt_cells == col {
                txt_buf.push(cell.c);
                txt_cells += 1;
            } else {
                if txt_active && !txt_buf.is_empty() {
                    text_runs.push(BatchedTextRun {
                        line: txt_line,
                        start_col: txt_start,
                        text: SharedString::from(txt_buf.clone()),
                        font_size,
                        style: TextRun {
                            len: txt_buf.len(),
                            font: txt_font.clone(),
                            color: txt_color,
                            background_color: None,
                            underline: txt_underline,
                            strikethrough: txt_strikethrough,
                        },
                        is_wide: false,
                    });
                    txt_buf.clear();
                }
                txt_active = true;
                txt_line = line;
                txt_start = col;
                txt_cells = 1;
                txt_buf.push(cell.c);
                txt_key = cell_txt_key;
                txt_color = fg;
                txt_font = cell_font;
                txt_underline = cell_underline;
                txt_strikethrough = cell_strikethrough;
            }

            col_idx += 1;
        }
    }

    // Final flush after the last row.
    if bg_active {
        bg_rects.push(LayoutRect {
            line: bg_line,
            start_col: bg_start,
            num_cells: bg_count,
            color: bg_color,
        });
    }
    if txt_active && !txt_buf.is_empty() {
        let txt_len = txt_buf.len();
        text_runs.push(BatchedTextRun {
            line: txt_line,
            start_col: txt_start,
            text: SharedString::from(txt_buf),
            font_size,
            style: TextRun {
                len: txt_len,
                font: txt_font,
                color: txt_color,
                background_color: None,
                underline: txt_underline,
                strikethrough: txt_strikethrough,
            },
            is_wide: false,
        });
    }

    GridLayout {
        bg_rects,
        text_runs,
        cursor,
    }
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = LayoutState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> LayoutState {
        let term_font = font(terminal_font_family(cx));
        let font_size = px(Preferences::terminal_font_size(cx));
        let colors = TerminalColors::current(cx);
        let font_id = window.text_system().resolve_font(&term_font);
        let cell_width = window
            .text_system()
            .advance(font_id, font_size, 'm')
            .map(|s| s.width)
            .unwrap_or(px(8.0));
        let line_height = font_size * LINE_HEIGHT_FACTOR;
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);

        if bounds.size.width.as_f32() > 0.0 && bounds.size.height.as_f32() > 0.0 {
            let cols = (bounds.size.width / cell_width).floor() as u16;
            let rows = (bounds.size.height / line_height).floor() as u16;
            let cols = cols.max(1);
            let rows = rows.max(1);

            self.view.update(cx, |view, cx| {
                view.last_element_bounds = Some(bounds);
                view.last_cell_size = Some((cell_width, line_height));

                let new_size = (rows, cols);
                if view.last_size != Some(new_size) {
                    view.last_size = Some(new_size);
                    let shell = view.shell.clone();
                    cx.spawn(async move |_, _| {
                        shell.resize(rows, cols).await;
                    })
                    .detach();
                }
            });
        }

        // The cache lookup needs `&mut view` and the shaping needs `&mut Window`,
        // so on a miss the grid is built inside `update`, shaped out here and
        // stored back. See [`CachedGrid`].
        enum GridSource {
            Cached(Rc<CachedGrid>),
            Fresh(GridLayout),
        }

        struct Prepared {
            source: GridSource,
            version: u64,
            selection_range: Option<GridSelectionRange>,
            search_matches: Vec<GridSelectionRange>,
            current_match: Option<GridSelectionRange>,
            total_cols: usize,
            display_offset: usize,
            history_size: usize,
            rows: usize,
        }

        let generation = crate::theme::generation();
        let prepared = self.view.update(cx, |view, _cx| {
            let snapshot = view.last_snapshot.as_ref()?;
            let version = view.snapshot_version;

            // The cache bakes resolved colours, so a theme switch must drop it.
            // A change of font, size or scheme drops it from the view, which
            // watches the preferences.
            let source = match &view.cached_grid {
                Some((v, g, cached)) if *v == version && *g == generation => {
                    GridSource::Cached(cached.clone())
                }
                _ => GridSource::Fresh(layout_grid(snapshot, &term_font, font_size, &colors)),
            };

            Some(Prepared {
                source,
                version,
                selection_range: view.selection.as_ref().and_then(|sel| sel.range),
                search_matches: snapshot.search_matches.clone(),
                current_match: snapshot.current_match,
                total_cols: snapshot.cols as usize,
                display_offset: snapshot.display_offset,
                history_size: snapshot.history_size,
                rows: snapshot.rows as usize,
            })
        });

        let Some(prepared) = prepared else {
            return LayoutState {
                cell_width,
                line_height,
                font_size,
                term_font,
                colors,
                hitbox,
                cached: Rc::new(CachedGrid {
                    bg_rects: vec![],
                    shaped_runs: vec![],
                    cursor: None,
                }),
                selection_range: None,
                search_matches: Vec::new(),
                current_match: None,
                total_cols: 80,
                display_offset: 0,
                history_size: 0,
                rows: 0,
            };
        };
        let Prepared {
            source,
            version,
            selection_range,
            search_matches,
            current_match,
            total_cols,
            display_offset,
            history_size,
            rows,
        } = prepared;

        let cached = match source {
            GridSource::Cached(cached) => cached,
            GridSource::Fresh(grid) => {
                let previous = self
                    .view
                    .update(cx, |view, _| std::mem::take(&mut view.shaped_lines));
                let mut current = ShapedLines::default();
                // Runs of plain spaces with no underline/strikethrough draw nothing,
                // so skip shaping and emitting them. Terminals are mostly blank, so
                // this drops the bulk of the screen. Positions are absolute via
                // `start_col`, so dropping blank runs shifts nothing.
                let shaped_runs = grid
                    .text_runs
                    .iter()
                    .filter(|run| {
                        !(run.style.underline.is_none()
                            && run.style.strikethrough.is_none()
                            && run.text.bytes().all(|b| b == b' '))
                    })
                    .map(|run| {
                        let force_w = if run.is_wide {
                            Some(cell_width * 2.0)
                        } else {
                            Some(cell_width)
                        };
                        // A run repeated on screen is shaped once.
                        let shaped = match current.get(run) {
                            Some(shaped) => shaped,
                            None => {
                                let shaped = previous.get(run).unwrap_or_else(|| {
                                    window.text_system().shape_line(
                                        run.text.clone(),
                                        run.font_size,
                                        std::slice::from_ref(&run.style),
                                        force_w,
                                    )
                                });
                                current.insert(run, shaped.clone());
                                shaped
                            }
                        };
                        ShapedRun {
                            line: run.line,
                            start_col: run.start_col,
                            shaped,
                        }
                    })
                    .collect();
                // The whole element is already filled with the default background
                // before painting (see `paint`), so those runs would be redundant quads.
                let bg_rects: Vec<LayoutRect> = grid
                    .bg_rects
                    .into_iter()
                    .filter(|r| r.color != colors.bg)
                    .collect();
                let cached = Rc::new(CachedGrid {
                    bg_rects,
                    shaped_runs,
                    cursor: grid.cursor,
                });
                self.view.update(cx, |view, _cx| {
                    view.cached_grid = Some((version, generation, cached.clone()));
                    view.shaped_lines = current;
                });
                cached
            }
        };

        LayoutState {
            cell_width,
            line_height,
            font_size,
            term_font,
            colors,
            hitbox,
            cached,
            selection_range,
            search_matches,
            current_match,
            total_cols,
            display_offset,
            history_size,
            rows,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        layout: &mut LayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        // Nothing is stored for the hovered link: it follows from the pointer,
        // the held modifiers and the snapshot, so it is worked out per frame.
        // The listeners below repaint when their own answer differs from it.
        let hovered_link = hovered_link(
            self.view.read(cx),
            &layout.hitbox,
            window.mouse_position(),
            window.modifiers(),
            window,
        );
        let painted_link = hovered_link.as_ref().map(|link| link.range);

        // Register global drag handlers so selection extends outside the element bounds.
        let view_for_move = self.view.downgrade();
        let view_for_up = self.view.downgrade();
        let view_for_modifiers = self.view.downgrade();
        let hitbox_for_move = layout.hitbox.clone();
        let hitbox_for_modifiers = layout.hitbox.clone();

        // Pressing or letting go of Cmd/Ctrl with the pointer at rest.
        window.on_modifiers_changed(move |event, window, cx| {
            let Some(view) = view_for_modifiers.upgrade() else {
                return;
            };
            repaint_if_link_changed(
                &view,
                &hitbox_for_modifiers,
                painted_link,
                window.mouse_position(),
                event.modifiers,
                window,
                cx,
            );
        });

        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            let Some(view) = view_for_move.upgrade() else {
                return;
            };
            if event.pressed_button.is_none() {
                repaint_if_link_changed(
                    &view,
                    &hitbox_for_move,
                    painted_link,
                    event.position,
                    event.modifiers,
                    window,
                    cx,
                );
                return;
            }
            // A scrollbar drag takes priority and keeps tracking even when the
            // pointer leaves the element bounds; it works regardless of focus.
            if view.read(cx).is_scrollbar_dragging() {
                if event.pressed_button == Some(MouseButton::Left) {
                    view.update(cx, |view, cx| view.scrollbar_drag_move(event.position, cx));
                }
                return;
            }
            if event.pressed_button != Some(MouseButton::Left) {
                return;
            }
            if !view.read(cx).focus_handle.is_focused(window) {
                return;
            }
            view.update(cx, |view, cx| {
                if view.is_link_click {
                    return;
                }
                let Some(bounds) = view.last_element_bounds else {
                    return;
                };
                let Some((cell_width, line_height)) = view.last_cell_size else {
                    return;
                };
                if view.report_mouse_drag(event, cx) {
                    return;
                }
                let display_offset = view
                    .last_snapshot
                    .as_ref()
                    .map(|s| s.display_offset)
                    .unwrap_or(0);
                let (abs_point, side) = pixel_to_grid(
                    event.position,
                    bounds,
                    cell_width,
                    line_height,
                    display_offset,
                );

                if view.is_selecting {
                    // Track the cursor so the autoscroll timer can keep scrolling
                    // even when the mouse is held still past an edge.
                    view.set_drag_position(event.position);
                    view.update_selection_head(abs_point, side, cx);
                }
            });
        });

        window.on_mouse_event(move |event: &MouseUpEvent, phase, _window, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            let Some(view) = view_for_up.upgrade() else {
                return;
            };
            // This listener sees every release in the window. Only the one that
            // ends a press the remote app was told about is reported to it.
            if view.update(cx, |view, cx| view.report_mouse_release(event, cx)) {
                return;
            }
            if event.button != MouseButton::Left {
                return;
            }
            // Finish a scrollbar drag before falling through to selection handling.
            if view.update(cx, |view, _| view.end_scrollbar_drag()) {
                return;
            }
            view.update(cx, |view, _| {
                if std::mem::take(&mut view.is_link_click) {
                    return;
                }
                view.end_selection();
            });
        });

        window.paint_quad(fill(bounds, layout.colors.bg));

        let origin = bounds.origin;
        let cell_width = layout.cell_width;
        let line_height = layout.line_height;

        for rect in &layout.cached.bg_rects {
            let x = origin.x + cell_width * rect.start_col as f32;
            let y = origin.y + line_height * rect.line as f32;
            let w = cell_width * rect.num_cells as f32;
            window.paint_quad(fill(
                Bounds::new(Point::new(x, y), size(w, line_height)),
                rect.color,
            ));
        }

        // Highlights are painted over the cell backgrounds and under the text, so
        // the text stays readable. The selection goes first: a scheme's own
        // selection colour is opaque and would hide the search hits under it.
        // The hits are see-through, so they still show on top of a selection.
        if let Some(range) = &layout.selection_range {
            let selection = layout.colors.selection.unwrap_or(cx.theme().selection);
            paint_range(range, selection, origin, layout, window);
        }

        for range in layout.search_matches.iter() {
            // The stepped-to hit is in this list too; painting it twice would
            // stack the two alphas into a colour neither of them describes.
            if Some(*range) == layout.current_match {
                continue;
            }
            paint_range(range, flint_theme::search_hit(), origin, layout, window);
        }

        if let Some(range) = &layout.current_match {
            paint_range(
                range,
                flint_theme::search_hit_active(),
                origin,
                layout,
                window,
            );
        }

        // Text runs were shaped in prepaint and are only positioned here.
        for run in &layout.cached.shaped_runs {
            let x = origin.x + cell_width * run.start_col as f32;
            let y = origin.y + line_height * run.line as f32;
            let _ = run.shaped.paint(
                Point::new(x, y),
                line_height,
                TextAlign::Left,
                None,
                window,
                cx,
            );
        }

        if let Some(link) = &hovered_link {
            paint_link_underline(&link.range, origin, layout, window);
            window.set_cursor_style(CursorStyle::PointingHand, &layout.hitbox);
        }

        let cursor_bounds = layout.cached.cursor.as_ref().map(|cur| {
            let x = origin.x + cell_width * cur.col as f32;
            let y = origin.y + line_height * cur.line as f32;
            Bounds::new(Point::new(x, y), size(cell_width, line_height))
        });

        let is_focused = self.view.read(cx).focus_handle.is_focused(window);
        let cursor = layout.cached.cursor.as_ref().zip(cursor_bounds);
        if let Some((cur, cur_bounds)) = cursor.filter(|_| !is_focused) {
            // Keys do not go to this terminal: a hollow cursor, as other
            // terminals draw it. The character under it is already painted.
            window.paint_quad(outline(cur_bounds, cur.fg_under, BorderStyle::Solid));
        }

        if let Some((cur, cur_bounds)) = cursor.filter(|_| is_focused) {
            // Block cursor: fill in fg color, draw character in bg color (swap).
            window.paint_quad(fill(cur_bounds, cur.fg_under));

            let char_str = cur.char_under.to_string();
            let char_len = char_str.len();
            let cursor_style = TextRun {
                len: char_len,
                font: layout.term_font.clone(),
                color: cur.bg_under,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let shaped = window.text_system().shape_line(
                SharedString::from(char_str),
                layout.font_size,
                std::slice::from_ref(&cursor_style),
                Some(cell_width),
            );
            let _ = shaped.paint(
                cur_bounds.origin,
                line_height,
                TextAlign::Left,
                None,
                window,
                cx,
            );
        }

        // IME composition overlay: the marked text at the cursor, underlined.
        let marked = self.view.read(cx).marked_text.clone();
        if let Some(marked) = marked {
            let cursor_pos = layout
                .cached
                .cursor
                .as_ref()
                .map(|c| (c.line, c.col))
                .unwrap_or((0, 0));
            let pos = Point::new(
                origin.x + cell_width * cursor_pos.1 as f32,
                origin.y + line_height * cursor_pos.0 as f32,
            );

            let marked_style = TextRun {
                len: marked.len(),
                font: layout.term_font.clone(),
                color: layout.colors.fg,
                background_color: None,
                underline: Some(UnderlineStyle {
                    thickness: px(1.0),
                    color: Some(layout.colors.fg),
                    wavy: false,
                }),
                strikethrough: None,
            };
            // No forced cell width: a wide (CJK) character keeps its own
            // advance, and the background is as wide as the shaped text.
            let shaped = window.text_system().shape_line(
                SharedString::from(marked),
                layout.font_size,
                std::slice::from_ref(&marked_style),
                None,
            );
            window.paint_quad(fill(
                Bounds::new(pos, size(shaped.width, line_height)),
                layout.colors.bg,
            ));
            let _ = shaped.paint(pos, line_height, TextAlign::Left, None, window, cx);
        }

        // Scrollbar: an overlay on the right edge, shown whenever there is
        // scrollback. Hit-testing for the drag lives in TerminalView's mouse
        // handlers.
        if layout.history_size > 0 {
            let track_color = cx.theme().scrollbar;
            let thumb_color = cx.theme().scrollbar_thumb;

            let margin = px(2.0); // inset from the edges
            let width = px(6.0);
            let radius = width * 0.5; // pill ends

            let track_x = bounds.origin.x + bounds.size.width - width - margin;
            let track_y = bounds.origin.y + margin;
            let track_h = (bounds.size.height - margin * 2.0).max(px(0.0));

            window.paint_quad(
                fill(
                    Bounds::new(Point::new(track_x, track_y), size(width, track_h)),
                    track_color,
                )
                .corner_radii(radius),
            );

            // Thumb sized to the visible fraction, with a floor so it stays grabbable
            // on long scrollback. The top is placed along the remaining travel.
            let total = (layout.history_size + layout.rows) as f32;
            let viewport = layout.rows as f32;
            let thumb_frac = (viewport / total).clamp(0.05, 1.0);
            let offset_frac = layout.display_offset as f32 / total;
            let top_frac = (1.0 - offset_frac - thumb_frac).clamp(0.0, 1.0 - thumb_frac);

            let min_thumb = px(24.0).min(track_h);
            let thumb_h = (track_h * thumb_frac).max(min_thumb);
            let travel = (track_h - thumb_h).max(px(0.0));
            let denom = (1.0 - thumb_frac).max(f32::EPSILON);
            let thumb_y = track_y + travel * (top_frac / denom).clamp(0.0, 1.0);

            window.paint_quad(
                fill(
                    Bounds::new(Point::new(track_x, thumb_y), size(width, thumb_h)),
                    thumb_color,
                )
                .corner_radii(radius),
            );

            // The clickable band is wider than the painted bar: a few pixels to
            // its left, the right margin and the view's side padding out to the
            // window edge, and the full height, so the thin bar is easy to grab.
            let grab_pad = px(4.0);
            self.view.update(cx, |view, _| {
                view.scrollbar_geom = Some(ScrollbarGeom {
                    hit_x0: track_x - grab_pad,
                    hit_x1: bounds.origin.x + bounds.size.width + px(SIDE_PADDING),
                    hit_y0: bounds.origin.y,
                    hit_y1: bounds.origin.y + bounds.size.height,
                    track_y,
                    track_h,
                    thumb_y,
                    thumb_h,
                    history_size: layout.history_size,
                });
            });
        } else {
            self.view.update(cx, |view, _| view.scrollbar_geom = None);
        }

        let focus_handle = self.view.read(cx).focus_handle.clone();
        let input_handler = TerminalInputHandler {
            view: self.view.downgrade(),
            cursor_bounds,
        };
        window.handle_input(&focus_handle, input_handler, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preferences::TerminalScheme;
    use crate::shell::SnapshotCell;

    fn snapshot_of(row: Vec<SnapshotCell>) -> TermSnapshot {
        TermSnapshot {
            rows: 1,
            cols: row.len() as u16,
            cells: vec![row],
            is_row_wrapped: vec![false],
            cursor: (0, 0),
            cursor_visible: false,
            alt_screen: false,
            bracketed_paste: false,
            is_app_cursor: false,
            is_alternate_scroll: true,
            mouse_mode: Default::default(),
            display_offset: 0,
            history_size: 0,
            selection: None,
            search_matches: Vec::new(),
            current_match: None,
        }
    }

    #[test]
    fn a_point_outside_the_grid_is_pulled_onto_its_edge() {
        let inside = GridPoint { line: 3, column: 7 };
        assert_eq!(clamp_to_grid(inside, 24, 80), inside);
        assert_eq!(
            clamp_to_grid(
                GridPoint {
                    line: 30,
                    column: 200
                },
                24,
                80
            ),
            GridPoint {
                line: 23,
                column: 79
            }
        );
        assert_eq!(
            clamp_to_grid(
                GridPoint {
                    line: -2,
                    column: 0
                },
                24,
                80
            ),
            GridPoint { line: 0, column: 0 }
        );
        assert_eq!(
            clamp_to_grid(GridPoint { line: 1, column: 1 }, 0, 0),
            GridPoint { line: 0, column: 0 }
        );
    }

    #[test]
    fn utf16_offsets_are_turned_into_byte_offsets() {
        // One UTF-16 unit and three bytes per character.
        assert_eq!(utf16_to_byte_offset("日本語", 0), 0);
        assert_eq!(utf16_to_byte_offset("日本語", 1), 3);
        assert_eq!(utf16_to_byte_offset("日本語", 3), 9);
        assert_eq!(utf16_to_byte_offset("日本語", 10), 9);
        // An emoji is two UTF-16 units and four bytes.
        assert_eq!(utf16_to_byte_offset("a😀b", 1), 1);
        assert_eq!(utf16_to_byte_offset("a😀b", 2), 1);
        assert_eq!(utf16_to_byte_offset("a😀b", 3), 5);
        assert_eq!(utf16_len("a😀b"), 4);
    }

    /// bash and zsh show pasted text in reverse video. With default colours
    /// it must not join the run of the prompt before it and take its colour.
    #[test]
    fn reverse_video_text_is_not_drawn_in_the_colour_of_the_text_before_it() {
        let plain = |c| SnapshotCell {
            c,
            ..Default::default()
        };
        let pasted = |c| SnapshotCell {
            c,
            inverse: true,
            ..Default::default()
        };
        let snapshot = snapshot_of(vec![plain('$'), plain(' '), pasted('l'), pasted('s')]);
        let colors = TerminalColors::for_scheme(TerminalScheme::Andromeda);

        let grid = layout_grid(&snapshot, &gpui::font("Menlo"), px(13.), &colors);

        let runs: Vec<(&str, Hsla)> = grid
            .text_runs
            .iter()
            .map(|run| (run.text.as_ref(), run.style.color))
            .collect();
        assert_eq!(runs, [("$ ", colors.fg), ("ls", colors.bg)]);
        // The highlight behind the pasted text is the text colour.
        let highlight = grid
            .bg_rects
            .iter()
            .find(|rect| rect.start_col == 2)
            .unwrap();
        assert_eq!((highlight.num_cells, highlight.color), (2, colors.fg));
    }
}
