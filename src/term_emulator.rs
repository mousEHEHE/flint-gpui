use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point as AlacPoint, Side};
use alacritty_terminal::selection::{
    Selection, SelectionRange as AlacSelectionRange, SelectionType as AlacSelectionType,
};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{ClearMode, Color, CursorShape, Handler, Processor};

use crate::shell::{
    GridPoint, GridSelectionRange, MouseModeFlags, ScrollTarget, SelectionSide, SelectionType,
    SnapshotCell, SnapshotColor, TermSnapshot,
};

/// Rows of scrollback kept per session.
const SCROLLBACK_LINES: usize = 10_000;

/// Collects what the emulator wants written back to the server: its answers
/// to terminal queries (cursor position, device attributes). Programs such as
/// vim and tmux ask at startup and wait for the answer. The other events
/// (title, bell, clipboard) are not used.
#[derive(Clone, Default)]
struct ReplyCollector(Arc<Mutex<Vec<u8>>>);

impl EventListener for ReplyCollector {
    fn send_event(&self, event: Event) {
        let Event::PtyWrite(text) = event else { return };
        if let Ok(mut replies) = self.0.lock() {
            replies.extend_from_slice(text.as_bytes());
        }
    }
}

pub struct TermEmulator {
    term: Term<ReplyCollector>,
    processor: Processor,
    replies: ReplyCollector,
    /// Query of the terminal search bar; empty means no search is running.
    search: String,
    /// The match the user last stepped to, in absolute buffer coordinates. Kept
    /// so it can be painted apart from the others and so next/prev know where to
    /// continue from.
    current_match: Option<GridSelectionRange>,
}

struct SimpleDims {
    rows: u16,
    cols: u16,
}

impl Dimensions for SimpleDims {
    fn total_lines(&self) -> usize {
        self.rows as usize
    }

    fn screen_lines(&self) -> usize {
        self.rows as usize
    }

    fn columns(&self) -> usize {
        self.cols as usize
    }
}

impl TermEmulator {
    pub fn new(rows: u16, cols: u16) -> Self {
        let dims = SimpleDims { rows, cols };
        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            ..Config::default()
        };
        let replies = ReplyCollector::default();
        let term = Term::new(config, &dims, replies.clone());
        let processor = Processor::new();
        Self {
            term,
            processor,
            replies,
            search: String::new(),
            current_match: None,
        }
    }

    pub fn advance(&mut self, bytes: &[u8]) {
        let history_before = self.term.history_size();
        self.processor.advance(&mut self.term, bytes);
        self.follow_scrolled_lines(history_before);
    }

    /// The bytes the emulator answered with since the last call. The caller
    /// sends them to the server after each [`Self::advance`].
    pub fn take_replies(&mut self) -> Vec<u8> {
        self.replies
            .0
            .lock()
            .map(|mut replies| std::mem::take(&mut *replies))
            .unwrap_or_default()
    }

    /// Keep the stepped-to match on its own text after output scrolled the
    /// buffer. Absolute lines do not move with their content: every row pushed
    /// into history shifts the match one line further up. Two cases can't be
    /// followed and drop the match instead: a full scrollback, where the buffer
    /// still scrolls but history stops growing, and the alternate screen, which
    /// swaps the whole grid.
    fn follow_scrolled_lines(&mut self, history_before: usize) {
        let Some(current) = self.current_match else {
            return;
        };

        let history_now = self.term.history_size();
        let Some(scrolled) = history_now.checked_sub(history_before) else {
            self.current_match = None;
            return;
        };
        if scrolled == 0 {
            return;
        }
        if history_now >= SCROLLBACK_LINES {
            self.current_match = None;
            return;
        }

        let scrolled = scrolled as i32;
        self.current_match = Some(GridSelectionRange {
            start: GridPoint {
                line: current.start.line - scrolled,
                ..current.start
            },
            end: GridPoint {
                line: current.end.line - scrolled,
                ..current.end
            },
        });
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.term.resize(SimpleDims { rows, cols });
        // Reflow moves text between lines, so the match would point at other text.
        self.current_match = None;
    }

    pub fn snapshot(&self) -> TermSnapshot {
        let content = self.term.renderable_content();
        let display_offset = content.display_offset;
        // alacritty has already rotated this range to track its content as the
        // buffer scrolled, so the UI can render it as-is against the current
        // display_offset and it stays pinned to the selected text.
        let selection = content.selection.map(to_grid_range);
        let cursor_visible = content.cursor.shape != CursorShape::Hidden;
        let cursor_point = content.cursor.point;
        let mode = content.mode;

        let mouse_mode = MouseModeFlags {
            report_click: mode.contains(TermMode::MOUSE_REPORT_CLICK),
            report_drag: mode.contains(TermMode::MOUSE_DRAG),
            report_motion: mode.contains(TermMode::MOUSE_MOTION),
            sgr: mode.contains(TermMode::SGR_MOUSE),
        };

        let rows = self.term.screen_lines();
        let cols = self.term.columns();
        let history_size = self.term.total_lines().saturating_sub(rows);

        // Only the visible rows are searched here: off-screen matches would be
        // clipped by the renderer anyway, and this runs on every chunk of output.
        let first_visible = -(display_offset as i32);
        let search_matches = self.find_between(first_visible, first_visible + rows as i32 - 1);

        let mut cells = Vec::with_capacity(rows);
        for _ in 0..rows {
            let mut row = Vec::with_capacity(cols);
            row.resize_with(cols, SnapshotCell::default);
            cells.push(row);
        }
        let mut is_row_wrapped = vec![false; rows];

        for indexed in content.display_iter {
            let viewport_row = indexed.point.line.0 + display_offset as i32;
            let col = indexed.point.column.0;
            if viewport_row < 0 || viewport_row as usize >= rows || col >= cols {
                continue;
            }
            cells[viewport_row as usize][col] = to_snapshot_cell(indexed.cell);
            // alacritty marks a soft wrap on the last cell of the row.
            if indexed.cell.flags.contains(Flags::WRAPLINE) {
                is_row_wrapped[viewport_row as usize] = true;
            }
        }

        TermSnapshot {
            rows: rows as u16,
            cols: cols as u16,
            cells,
            is_row_wrapped,
            cursor: (
                cursor_point.line.0.max(0) as u16,
                cursor_point.column.0 as u16,
            ),
            cursor_visible,
            alt_screen: mode.contains(TermMode::ALT_SCREEN),
            bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
            is_app_cursor: mode.contains(TermMode::APP_CURSOR),
            is_alternate_scroll: mode.contains(TermMode::ALTERNATE_SCROLL),
            mouse_mode,
            display_offset,
            history_size,
            selection,
            search_matches,
            current_match: self.valid_current_match(),
        }
    }

    pub fn scroll_delta(&mut self, lines: i32) {
        self.term.scroll_display(Scroll::Delta(lines));
    }

    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    pub fn scroll_to(&mut self, target: ScrollTarget) {
        self.term.scroll_display(match target {
            ScrollTarget::PageUp => Scroll::PageUp,
            ScrollTarget::PageDown => Scroll::PageDown,
            ScrollTarget::Top => Scroll::Top,
            ScrollTarget::Bottom => Scroll::Bottom,
        });
    }

    /// Wipe the scrollback and the screen, leaving the cursor at the top left.
    ///
    /// The screen goes too because the shell answers the Ctrl-L that follows
    /// with a clear of its own, and that one pushes whatever is on screen into
    /// the scrollback. On the alternate screen nothing happens: the scrollback
    /// belongs to the other grid, and the app repaints its own screen.
    pub fn clear_scrollback(&mut self) {
        if self.term.mode().contains(TermMode::ALT_SCREEN) {
            return;
        }
        // Below, not All: All would move the screen into the scrollback, and
        // Saved does nothing when the scrollback is empty.
        self.term.goto(0, 0);
        self.term.clear_screen(ClearMode::Below);
        self.term.clear_screen(ClearMode::Saved);
        self.term.selection = None;
        self.current_match = None;
    }

    /// Jump the viewport to an absolute history offset (0 = bottom). The delta is
    /// taken against the live display offset, so callers can issue absolute targets
    /// without tracking the current position themselves.
    pub fn scroll_to_offset(&mut self, offset: usize) {
        let current = self.term.renderable_content().display_offset;
        let delta = offset as i32 - current as i32;
        if delta != 0 {
            self.term.scroll_display(Scroll::Delta(delta));
        }
    }

    pub fn set_selection(
        &mut self,
        ty: SelectionType,
        anchor: GridPoint,
        anchor_side: SelectionSide,
        head: GridPoint,
        head_side: SelectionSide,
    ) -> Option<GridSelectionRange> {
        let mut sel = Selection::new(
            to_alac_ty(ty),
            to_alac_point(anchor),
            to_alac_side(anchor_side),
        );
        sel.update(to_alac_point(head), to_alac_side(head_side));
        self.term.selection = Some(sel);
        self.term
            .selection
            .as_ref()
            .and_then(|s| s.to_range(&self.term))
            .map(to_grid_range)
    }

    /// Extend the head of the current selection without resetting its anchor.
    /// The anchor stays where alacritty has rotated it to, so the selection keeps
    /// tracking its content even if the buffer scrolls mid-drag.
    pub fn update_selection_head(
        &mut self,
        head: GridPoint,
        head_side: SelectionSide,
    ) -> Option<GridSelectionRange> {
        if let Some(sel) = self.term.selection.as_mut() {
            sel.update(to_alac_point(head), to_alac_side(head_side));
        }
        self.term
            .selection
            .as_ref()
            .and_then(|s| s.to_range(&self.term))
            .map(to_grid_range)
    }

    pub fn selection_to_string(&self) -> Option<String> {
        self.term.selection_to_string()
    }

    pub fn select_all(&mut self) -> Option<GridSelectionRange> {
        let (start, end) = {
            let grid = self.term.grid();
            let cols = grid.columns();
            // The topmost line is the oldest row of the scrollback, not the
            // first row on screen.
            (
                AlacPoint::new(grid.topmost_line(), Column(0)),
                AlacPoint::new(grid.bottommost_line(), Column(cols - 1)),
            )
        };
        let mut sel = Selection::new(AlacSelectionType::Simple, start, Side::Left);
        sel.update(end, Side::Right);
        self.term.selection = Some(sel);
        self.term
            .selection
            .as_ref()
            .and_then(|s| s.to_range(&self.term))
            .map(to_grid_range)
    }
}

impl TermEmulator {
    /// Set the search query and report how many matches the whole buffer holds.
    /// An empty query turns the search off.
    pub fn set_search(&mut self, query: String) -> usize {
        self.search = query;
        self.current_match = None;
        self.find_all().len()
    }

    /// Move to the next (or previous) match, scrolling it into view. Returns the
    /// 1-based position of that match and the total; `(0, 0)` = nothing matches.
    pub fn step_search(&mut self, forward: bool) -> (usize, usize) {
        let matches = self.find_all();
        if matches.is_empty() {
            self.current_match = None;
            return (0, 0);
        }

        let index = self.next_match_index(&matches, forward);
        let hit = matches[index];
        self.current_match = Some(hit);
        self.reveal_line(hit.start.line);
        (index + 1, matches.len())
    }

    /// Position of the match to step to. Continues from the current match when it
    /// is still there, otherwise starts from whatever the user is looking at.
    fn next_match_index(&self, matches: &[GridSelectionRange], forward: bool) -> usize {
        let last = matches.len() - 1;

        if let Some(current) = self.current_match {
            if let Some(i) = matches.iter().position(|m| *m == current) {
                return match (forward, i) {
                    (true, i) if i == last => 0,
                    (true, i) => i + 1,
                    (false, 0) => last,
                    (false, i) => i - 1,
                };
            }
        }

        let first_visible = -(self.term.grid().display_offset() as i32);
        let last_visible = first_visible + self.term.screen_lines() as i32 - 1;
        if forward {
            matches
                .iter()
                .position(|m| m.start.line >= first_visible)
                .unwrap_or(0)
        } else {
            matches
                .iter()
                .rposition(|m| m.start.line <= last_visible)
                .unwrap_or(last)
        }
    }

    /// Scroll `line` into view, centred, unless it is already visible.
    fn reveal_line(&mut self, line: i32) {
        let screen_lines = self.term.screen_lines() as i32;
        let display_offset = self.term.grid().display_offset() as i32;
        let first_visible = -display_offset;
        if line >= first_visible && line < first_visible + screen_lines {
            return;
        }

        let history_size = self
            .term
            .total_lines()
            .saturating_sub(screen_lines as usize) as i32;
        let target = (-line + screen_lines / 2).clamp(0, history_size);
        let delta = target - display_offset;
        if delta != 0 {
            self.term.scroll_display(Scroll::Delta(delta));
        }
    }

    fn find_all(&self) -> Vec<GridSelectionRange> {
        self.find_between(self.term.topmost_line().0, self.term.bottommost_line().0)
    }

    /// Matches on the absolute lines `first ..= last`, ordered top to bottom.
    ///
    /// Every row is searched on its own, so a match split across a soft wrap is
    /// not found. Finding it would mean joining a wrapped row (`Flags::WRAPLINE`
    /// on its last cell) with the rows that follow before searching.
    fn find_between(&self, first: i32, last: i32) -> Vec<GridSelectionRange> {
        if self.search.is_empty() {
            return Vec::new();
        }

        let needle: Vec<char> = self.search.chars().collect();
        let is_case_sensitive = self.search.chars().any(char::is_uppercase);
        let first = first.max(self.term.topmost_line().0);
        let last = last.min(self.term.bottommost_line().0);

        // Scanning the whole scrollback touches 10 000 rows, so the two row
        // buffers are allocated once here and reused for every line.
        let mut chars: Vec<char> = Vec::with_capacity(self.term.columns());
        let mut columns: Vec<usize> = Vec::with_capacity(self.term.columns());

        let mut matches = Vec::new();
        for line in first..=last {
            self.read_row(line, &mut chars, &mut columns);
            self.find_in_row(
                line,
                &chars,
                &columns,
                &needle,
                is_case_sensitive,
                &mut matches,
            );
        }
        matches
    }

    /// Read a row's characters and the column each one starts at.
    ///
    /// Wide characters (CJK, emoji) own two cells; the second one is a spacer
    /// holding no character of its own, so it must not break a match.
    fn read_row(&self, line: i32, chars: &mut Vec<char>, columns: &mut Vec<usize>) {
        chars.clear();
        columns.clear();

        let row = &self.term.grid()[Line(line)];
        for column in 0..self.term.columns() {
            let cell = &row[Column(column)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            chars.push(cell.c);
            columns.push(column);
        }
    }

    fn find_in_row(
        &self,
        line: i32,
        chars: &[char],
        columns: &[usize],
        needle: &[char],
        is_case_sensitive: bool,
        matches: &mut Vec<GridSelectionRange>,
    ) {
        if needle.len() > chars.len() {
            return;
        }
        let row = &self.term.grid()[Line(line)];

        let mut start = 0;
        while start + needle.len() <= chars.len() {
            let is_hit = needle
                .iter()
                .enumerate()
                .all(|(i, n)| chars_match(chars[start + i], *n, is_case_sensitive));
            if !is_hit {
                start += 1;
                continue;
            }

            let first_column = columns[start];
            let last_column = columns[start + needle.len() - 1];
            let is_wide = row[Column(last_column)].flags.contains(Flags::WIDE_CHAR);
            matches.push(GridSelectionRange {
                start: GridPoint {
                    line,
                    column: first_column,
                },
                end: GridPoint {
                    line,
                    column: last_column + usize::from(is_wide),
                },
            });
            start += needle.len();
        }
    }

    /// The current match, checked against the grid before it is handed out, so
    /// the renderer can never mark text that no longer holds the query. It also
    /// catches a match that followed its text past the top of the scrollback,
    /// see [`Self::follow_scrolled_lines`].
    fn valid_current_match(&self) -> Option<GridSelectionRange> {
        let current = self.current_match?;
        let line = current.start.line;
        if line < self.term.topmost_line().0 || line > self.term.bottommost_line().0 {
            return None;
        }
        self.find_between(line, line)
            .into_iter()
            .find(|m| *m == current)
    }
}

/// Compare two characters under the search's smart case rule: an all-lowercase
/// query matches any case, a query with an uppercase character matches exactly.
fn chars_match(a: char, b: char, is_case_sensitive: bool) -> bool {
    if is_case_sensitive {
        return a == b;
    }
    if a.is_ascii() && b.is_ascii() {
        return a.eq_ignore_ascii_case(&b);
    }
    a.to_lowercase().eq(b.to_lowercase())
}

fn to_alac_ty(ty: SelectionType) -> AlacSelectionType {
    match ty {
        SelectionType::Simple => AlacSelectionType::Simple,
        SelectionType::Semantic => AlacSelectionType::Semantic,
        SelectionType::Lines => AlacSelectionType::Lines,
    }
}

fn to_alac_point(p: GridPoint) -> AlacPoint {
    AlacPoint::new(Line(p.line), Column(p.column))
}

fn to_alac_side(s: SelectionSide) -> Side {
    match s {
        SelectionSide::Left => Side::Left,
        SelectionSide::Right => Side::Right,
    }
}

fn to_grid_range(r: AlacSelectionRange) -> GridSelectionRange {
    GridSelectionRange {
        start: GridPoint {
            line: r.start.line.0,
            column: r.start.column.0,
        },
        end: GridPoint {
            line: r.end.line.0,
            column: r.end.column.0,
        },
    }
}

fn to_snapshot_cell(cell: &alacritty_terminal::term::cell::Cell) -> SnapshotCell {
    SnapshotCell {
        c: cell.c,
        fg: to_snapshot_color(cell.fg),
        bg: to_snapshot_color(cell.bg),
        bold: cell.flags.contains(Flags::BOLD),
        italic: cell.flags.contains(Flags::ITALIC),
        underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
        strikethrough: cell.flags.contains(Flags::STRIKEOUT),
        dim: cell.flags.contains(Flags::DIM),
        inverse: cell.flags.contains(Flags::INVERSE),
        wide_char: cell.flags.contains(Flags::WIDE_CHAR),
        wide_char_spacer: cell.flags.contains(Flags::WIDE_CHAR_SPACER),
    }
}

fn to_snapshot_color(color: Color) -> SnapshotColor {
    match color {
        Color::Named(nc) => {
            // NamedColor::Foreground = 256 and above are semantic, not palette indices.
            if (nc as u16) < 256 {
                SnapshotColor::Indexed(nc as u8)
            } else {
                SnapshotColor::Default
            }
        }
        Color::Indexed(i) => SnapshotColor::Indexed(i),
        Color::Spec(rgb) => SnapshotColor::Rgb(rgb.r, rgb.g, rgb.b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 6x20 terminal holding `lines`, one per row.
    fn emulator_with(lines: &[&str]) -> TermEmulator {
        let mut emulator = TermEmulator::new(6, 20);
        for line in lines {
            emulator.advance(line.as_bytes());
            emulator.advance(b"\r\n");
        }
        emulator
    }

    #[test]
    fn snapshot_tracks_cursor_key_and_alternate_scroll_modes() {
        let mut emulator = TermEmulator::new(6, 20);
        let snapshot = emulator.snapshot();
        assert!(!snapshot.is_app_cursor);
        assert!(snapshot.is_alternate_scroll);

        emulator.advance(b"\x1b[?1h\x1b[?1007l");
        let snapshot = emulator.snapshot();
        assert!(snapshot.is_app_cursor);
        assert!(!snapshot.is_alternate_scroll);

        emulator.advance(b"\x1b[?1l\x1b[?1007h");
        let snapshot = emulator.snapshot();
        assert!(!snapshot.is_app_cursor);
        assert!(snapshot.is_alternate_scroll);
    }

    #[test]
    fn terminal_queries_are_answered_once() {
        let mut emulator = TermEmulator::new(6, 20);
        emulator.advance(b"ab\x1b[6n\x1b[c");
        assert_eq!(emulator.take_replies(), b"\x1b[1;3R\x1b[?6c".to_vec());
        assert!(emulator.take_replies().is_empty());
    }

    #[test]
    fn snapshot_marks_only_the_rows_that_wrap() {
        let emulator = emulator_with(&["short", "a line longer than twenty cells", "end"]);
        assert_eq!(
            emulator.snapshot().is_row_wrapped,
            vec![false, true, false, false, false, false]
        );
    }

    #[test]
    fn select_all_takes_the_scrollback_too() {
        // 8 rows into a 6-row terminal: the first rows are pushed into history.
        let mut emulator = emulator_with(&["first", "b", "c", "d", "e", "f", "g", "last"]);
        let range = emulator.select_all().expect("buffer is not empty");
        assert!(
            range.start.line < 0,
            "selection starts on screen, not in history"
        );

        let text = emulator.selection_to_string().expect("selection has text");
        assert!(text.starts_with("first\n"), "got {text:?}");
        assert!(text.contains("last"));
    }

    #[test]
    fn clearing_the_scrollback_empties_history_and_screen() {
        let mut emulator = emulator_with(&["old", "b", "c", "d", "e", "f", "g", "tail"]);
        emulator.scroll_to(ScrollTarget::Top);
        emulator.select_all();
        emulator.set_search("old".to_string());
        emulator.step_search(true);

        emulator.clear_scrollback();

        let snapshot = emulator.snapshot();
        assert_eq!(snapshot.history_size, 0);
        assert_eq!(snapshot.display_offset, 0);
        assert_eq!(snapshot.cursor, (0, 0));
        assert!(snapshot.selection.is_none());
        assert!(snapshot.current_match.is_none());
        assert!(snapshot.cells.iter().flatten().all(|cell| cell.c == ' '));
        assert_eq!(emulator.set_search("tail".to_string()), 0);
    }

    #[test]
    fn clearing_works_with_an_empty_scrollback() {
        let mut emulator = emulator_with(&["only line"]);
        emulator.clear_scrollback();
        assert!(emulator
            .snapshot()
            .cells
            .iter()
            .flatten()
            .all(|cell| cell.c == ' '));
    }

    #[test]
    fn the_shell_redraw_after_a_clear_leaves_no_old_text_behind() {
        let mut emulator = emulator_with(&["secret", "b", "c", "d", "e", "f", "g", "$ "]);
        emulator.clear_scrollback();
        // What readline and zsh send back for Ctrl-L.
        emulator.advance(b"\x1b[H\x1b[2J$ ");

        assert_eq!(emulator.set_search("secret".to_string()), 0);
        // alacritty scrolls one row out even when the screen is already blank.
        assert!(emulator.snapshot().history_size <= 1);
    }

    #[test]
    fn clearing_leaves_the_alternate_screen_alone() {
        let mut emulator = emulator_with(&["old", "b", "c", "d", "e", "f", "g"]);
        emulator.advance(b"\x1b[?1049h\x1b[Hvim");
        emulator.clear_scrollback();
        assert_eq!(emulator.snapshot().cells[0][0].c, 'v');

        emulator.advance(b"\x1b[?1049l");
        assert!(emulator.snapshot().history_size > 0);
    }

    #[test]
    fn page_keys_scroll_one_screen_and_stop_at_the_ends() {
        let mut emulator = TermEmulator::new(6, 20);
        for _ in 0..20 {
            emulator.advance(b"line\r\n");
        }
        let history_size = emulator.snapshot().history_size;

        emulator.scroll_to(ScrollTarget::PageUp);
        assert_eq!(emulator.snapshot().display_offset, 6);
        emulator.scroll_to(ScrollTarget::PageDown);
        assert_eq!(emulator.snapshot().display_offset, 0);
        emulator.scroll_to(ScrollTarget::PageDown);
        assert_eq!(emulator.snapshot().display_offset, 0);

        emulator.scroll_to(ScrollTarget::Top);
        assert_eq!(emulator.snapshot().display_offset, history_size);
        emulator.scroll_to(ScrollTarget::PageUp);
        assert_eq!(emulator.snapshot().display_offset, history_size);
        emulator.scroll_to(ScrollTarget::Bottom);
        assert_eq!(emulator.snapshot().display_offset, 0);
    }

    #[test]
    fn finds_every_match_in_the_buffer() {
        let mut emulator = emulator_with(&["error one", "fine", "error two"]);
        assert_eq!(emulator.set_search("error".to_string()), 2);
    }

    #[test]
    fn repeated_match_on_one_row_is_found_once_per_occurrence() {
        let mut emulator = emulator_with(&["ab ab ab"]);
        assert_eq!(emulator.set_search("ab".to_string()), 3);
    }

    #[test]
    fn overlapping_occurrences_do_not_double_count() {
        let mut emulator = emulator_with(&["aaaa"]);
        assert_eq!(emulator.set_search("aa".to_string()), 2);
    }

    #[test]
    fn lowercase_query_matches_any_case() {
        let mut emulator = emulator_with(&["Error", "error"]);
        assert_eq!(emulator.set_search("error".to_string()), 2);
    }

    #[test]
    fn query_with_uppercase_matches_exactly() {
        let mut emulator = emulator_with(&["Error", "error"]);
        assert_eq!(emulator.set_search("Error".to_string()), 1);
    }

    #[test]
    fn empty_query_turns_the_search_off() {
        let mut emulator = emulator_with(&["error"]);
        emulator.set_search("error".to_string());
        assert_eq!(emulator.set_search(String::new()), 0);
        assert!(emulator.snapshot().search_matches.is_empty());
    }

    #[test]
    fn match_range_covers_the_query() {
        let mut emulator = emulator_with(&["ab error cd"]);
        emulator.set_search("error".to_string());
        let matches = emulator.snapshot().search_matches;
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].start.column, 3);
        assert_eq!(matches[0].end.column, 7);
    }

    #[test]
    fn stepping_walks_the_matches_and_wraps_around() {
        let mut emulator = emulator_with(&["hit", "hit"]);
        emulator.set_search("hit".to_string());
        assert_eq!(emulator.step_search(true), (1, 2));
        assert_eq!(emulator.step_search(true), (2, 2));
        assert_eq!(emulator.step_search(true), (1, 2));
        assert_eq!(emulator.step_search(false), (2, 2));
    }

    #[test]
    fn stepping_without_matches_reports_nothing() {
        let mut emulator = emulator_with(&["nothing here"]);
        emulator.set_search("missing".to_string());
        assert_eq!(emulator.step_search(true), (0, 0));
        assert!(emulator.snapshot().current_match.is_none());
    }

    #[test]
    fn snapshot_carries_only_the_visible_matches() {
        // 8 rows into a 6-row terminal: the first rows are pushed into history.
        let mut emulator = emulator_with(&["old hit", "b", "c", "d", "e", "f", "g", "new hit"]);
        assert_eq!(emulator.set_search("hit".to_string()), 2);
        assert_eq!(emulator.snapshot().search_matches.len(), 1);
    }

    #[test]
    fn stepping_to_an_off_screen_match_scrolls_it_into_view() {
        let mut emulator = emulator_with(&["old hit", "b", "c", "d", "e", "f", "g", "tail"]);
        emulator.set_search("hit".to_string());
        emulator.step_search(true);

        let snapshot = emulator.snapshot();
        assert!(snapshot.display_offset > 0, "viewport did not scroll up");
        assert_eq!(snapshot.search_matches.len(), 1);
        assert!(snapshot.current_match.is_some());
    }

    #[test]
    fn output_that_does_not_scroll_keeps_the_current_match() {
        let mut emulator = emulator_with(&["hit"]);
        emulator.set_search("hit".to_string());
        emulator.step_search(true);
        let before = emulator.snapshot().current_match;
        assert!(before.is_some());

        // A progress bar rewriting its own line moves nothing.
        emulator.advance(b"\rworking 10%");
        assert_eq!(emulator.snapshot().current_match, before);
    }

    #[test]
    fn scrolled_output_moves_the_current_match_with_its_text() {
        let mut emulator = emulator_with(&["hit"]);
        emulator.set_search("hit".to_string());
        emulator.step_search(true);
        let before = emulator
            .snapshot()
            .current_match
            .expect("stepped to a match");

        // Push the row with the hit up into the scrollback.
        for _ in 0..8 {
            emulator.advance(b"filler\r\n");
        }

        let after = emulator
            .snapshot()
            .current_match
            .expect("match followed its text");
        assert!(
            after.start.line < before.start.line,
            "match did not move up"
        );
        assert_eq!(
            emulator.snapshot().search_matches.len(),
            0,
            "hit scrolled off screen"
        );
        assert_eq!(emulator.step_search(true), (1, 1));
    }

    #[test]
    fn stepping_keeps_advancing_while_output_arrives() {
        let mut emulator = emulator_with(&["hit a", "hit b", "hit c"]);
        emulator.set_search("hit".to_string());
        assert_eq!(emulator.step_search(true), (1, 3));

        emulator.advance(b"noise\r\n");
        assert_eq!(emulator.step_search(true), (2, 3));
    }

    #[test]
    fn resize_drops_the_current_match() {
        let mut emulator = emulator_with(&["hit"]);
        emulator.set_search("hit".to_string());
        emulator.step_search(true);
        assert!(emulator.snapshot().current_match.is_some());

        emulator.resize(8, 20);
        assert!(emulator.snapshot().current_match.is_none());
    }

    #[test]
    fn switching_to_the_alternate_screen_drops_the_current_match() {
        let mut emulator = emulator_with(&["hit", "b", "c", "d", "e", "f", "g"]);
        emulator.set_search("hit".to_string());
        emulator.step_search(true);
        assert!(emulator.snapshot().current_match.is_some());

        emulator.advance(b"\x1b[?1049h");
        assert!(emulator.snapshot().current_match.is_none());
    }

    #[test]
    fn wide_characters_do_not_break_a_match() {
        let mut emulator = emulator_with(&["x 日本語 y"]);
        emulator.set_search("日本".to_string());
        let matches = emulator.snapshot().search_matches;
        assert_eq!(matches.len(), 1);
        // Two wide chars take four cells, starting after "x ".
        assert_eq!(matches[0].start.column, 2);
        assert_eq!(matches[0].end.column, 5);
    }
}
