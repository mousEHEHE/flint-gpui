use std::rc::Rc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    actions, div, px, App, Bounds, ClipboardItem, Context, EventEmitter, FocusHandle, Focusable,
    KeyBinding, KeyDownEvent, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, ScrollDelta, ScrollWheelEvent, Subscription, Task, Window,
};

use crate::preferences::{Preferences, DEFAULT_TERMINAL_FONT_SIZE};
use crate::shell::{
    GridPoint, GridSelectionRange, MouseModeFlags, ScrollTarget, SelectionSide, SelectionType,
    ShellEvent, ShellHandle, TermSnapshot,
};
use crate::theme as flint_theme;
use crate::ui::context_menu::{ContextMenu, ContextMenuItem};
use crate::ui::keyboard_layout::bind_chord;
use crate::ui::mouse_report::{encode_mouse_event, MouseButton as ReportButton, MouseEventKind};
use crate::ui::terminal_element::{
    clamp_to_grid, pixel_to_grid, CachedGrid, TerminalElement, LINE_HEIGHT_FACTOR,
};
use crate::ui::terminal_keymap;
use crate::ui::terminal_link::{self, TerminalLink};
use crate::ui::terminal_search::{self, SearchState};
use crate::ui::terminal_suggestions::{NavAction, SuggestionState};

actions!(
    terminal_view,
    [
        Tab,
        ShiftTab,
        Paste,
        CopySelection,
        SelectAll,
        IncreaseFontSize,
        DecreaseFontSize,
        ResetFontSize,
        ClearScreen,
        ScrollPageUp,
        ScrollPageDown,
        ScrollToTop,
        ScrollToBottom,
    ]
);

/// How long the "Copied" note stays up after a copy.
const COPIED_HINT_DURATION: Duration = Duration::from_millis(1500);

pub fn init(cx: &mut App) {
    terminal_search::init(cx);
    cx.bind_keys([
        KeyBinding::new("tab", Tab, Some("Terminal")),
        KeyBinding::new("shift-tab", ShiftTab, Some("Terminal")),
        KeyBinding::new("shift-pageup", ScrollPageUp, Some("Terminal")),
        KeyBinding::new("shift-pagedown", ScrollPageDown, Some("Terminal")),
        KeyBinding::new("shift-home", ScrollToTop, Some("Terminal")),
        KeyBinding::new("shift-end", ScrollToBottom, Some("Terminal")),
    ]);

    // macOS uses Cmd for clipboard. Windows/Linux terminals use Ctrl+Shift,
    // since plain Ctrl+C/V/A are reserved for SIGINT / literal-next / start-of-line.
    #[cfg(target_os = "macos")]
    {
        bind_chord(cx, "cmd-v", Paste, Some("Terminal"));
        bind_chord(cx, "cmd-c", CopySelection, Some("Terminal"));
        bind_chord(cx, "cmd-a", SelectAll, Some("Terminal"));
        bind_chord(cx, "cmd-=", IncreaseFontSize, Some("Terminal"));
        bind_chord(cx, "cmd-shift-=", IncreaseFontSize, Some("Terminal"));
        bind_chord(cx, "cmd--", DecreaseFontSize, Some("Terminal"));
        bind_chord(cx, "cmd-0", ResetFontSize, Some("Terminal"));
        bind_chord(cx, "cmd-k", ClearScreen, Some("Terminal"));
        cx.bind_keys([
            KeyBinding::new("cmd-home", ScrollToTop, Some("Terminal")),
            KeyBinding::new("cmd-end", ScrollToBottom, Some("Terminal")),
        ]);
    }

    // Ctrl+K is readline's kill-line, so clear takes Ctrl+Shift+K here.
    #[cfg(not(target_os = "macos"))]
    {
        bind_chord(cx, "ctrl-shift-v", Paste, Some("Terminal"));
        bind_chord(cx, "ctrl-shift-c", CopySelection, Some("Terminal"));
        bind_chord(cx, "ctrl-shift-a", SelectAll, Some("Terminal"));
        bind_chord(cx, "ctrl-=", IncreaseFontSize, Some("Terminal"));
        bind_chord(cx, "ctrl-shift-=", IncreaseFontSize, Some("Terminal"));
        bind_chord(cx, "ctrl--", DecreaseFontSize, Some("Terminal"));
        bind_chord(cx, "ctrl-0", ResetFontSize, Some("Terminal"));
        bind_chord(cx, "ctrl-shift-k", ClearScreen, Some("Terminal"));
    }
}

#[derive(Clone, Copy)]
pub struct SelectionState {
    /// The selection range as reported by the emulator. alacritty rotates it to
    /// stay on its content as the buffer scrolls, and every snapshot refreshes
    /// it, so the highlight does not drift when new output arrives.
    pub range: Option<GridSelectionRange>,
}

/// Geometry of the painted scrollbar, stashed each frame so mouse handlers can
/// hit-test the thumb/track for dragging. All values are window-space pixels.
#[derive(Clone, Copy)]
pub(crate) struct ScrollbarGeom {
    /// Clickable band, wider than the painted track so the thin bar and its right
    /// margin are easy to grab. Used only for hit-testing.
    pub hit_x0: Pixels,
    pub hit_x1: Pixels,
    pub hit_y0: Pixels,
    pub hit_y1: Pixels,
    pub track_y: Pixels,
    pub track_h: Pixels,
    pub thumb_y: Pixels,
    pub thumb_h: Pixels,
    pub history_size: usize,
}

pub struct TerminalView {
    _event_task: Task<()>,
    pub(crate) shell: ShellHandle,
    pub last_snapshot: Option<TermSnapshot>,
    /// Bumped each time `last_snapshot` is replaced; used to invalidate the
    /// cached grid layout in the terminal element.
    pub(crate) snapshot_version: u64,
    /// Cached cell-grid geometry and shaped text, keyed by `snapshot_version` and
    /// the palette generation: a repaint that changes neither reuses it. Wrapped
    /// in `Rc` so reusing it is a refcount bump.
    pub(crate) cached_grid: Option<(u64, u32, Rc<CachedGrid>)>,
    pub(crate) focus_handle: FocusHandle,
    has_auto_focused: bool,
    pub(crate) last_size: Option<(u16, u16)>,
    /// Text the input method is still composing, drawn at the cursor.
    pub(crate) marked_text: Option<String>,
    pub(crate) selection: Option<SelectionState>,
    /// True only while a selection drag started on this terminal is in progress
    /// (mouse-down to mouse-up). The global mouse-move handler keys off this rather
    /// than `selection.is_some()`, so a left-button drag elsewhere (e.g. dragging
    /// the split divider) can't mutate an existing selection.
    pub(crate) is_selecting: bool,
    pub(crate) last_element_bounds: Option<Bounds<Pixels>>,
    pub(crate) last_cell_size: Option<(Pixels, Pixels)>,
    scroll_accumulator: f32,
    /// Last mouse position during an active selection drag, used by the
    /// autoscroll timer to keep scrolling while the cursor sits past an edge.
    drag_position: Option<Point<Pixels>>,
    /// Ticks while a selection drag is in progress; scrolls the view when the
    /// cursor is past the top/bottom edge. Dropped (cancelled) when the drag ends.
    autoscroll_task: Option<Task<()>>,
    /// Last painted scrollbar geometry, used to hit-test mouse-downs on the thumb.
    pub(crate) scrollbar_geom: Option<ScrollbarGeom>,
    /// While dragging the scrollbar thumb: the grab offset (pointer.y - thumb top).
    scrollbar_drag: Option<Pixels>,
    /// Command suggestions popup, see [`crate::ui::terminal_suggestions`].
    pub(crate) suggestions: SuggestionState,
    /// Search bar state, see [`crate::ui::terminal_search`].
    pub(crate) search: SearchState,
    /// Font, size and colour scheme live in the preferences and are baked
    /// into `cached_grid`, so a change there must drop it.
    _preferences_subscription: Subscription,
    /// When set, a right-click context menu is open, anchored at this point.
    context_menu: Option<Point<Pixels>>,
    /// Line count of the last copy, while its note is on screen.
    copied_line_count: Option<usize>,
    /// Takes the note down again. Replaced (and so cancelled) by the next copy.
    copied_hint_task: Option<Task<()>>,
    /// True from a Cmd/Ctrl+click that opened a link until the button is let
    /// go. The drag and the release of that click belong to the link, so they
    /// are not reported to an app that has mouse mode on.
    pub(crate) is_link_click: bool,
    /// The button whose press was reported to an app with mouse mode on. Its
    /// drag and its release go to the same app. A release of anything else
    /// (a click on the sidebar, a press the app never saw) is not reported.
    reported_press: Option<MouseButton>,
}

pub enum TerminalEvent {
    /// The remote shell ended while the connection stayed up.
    Exited,
}

impl EventEmitter<TerminalEvent> for TerminalView {}

/// How often the autoscroll timer fires while the cursor is past an edge.
const AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(40);

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TerminalView {
    pub fn new(shell: ShellHandle, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let event_rx = shell.subscribe();

        let event_task = cx.spawn(async move |this, cx| {
            loop {
                let Ok(event) = event_rx.recv().await else {
                    break;
                };

                // Fold a burst of events into one update. Bursty output (cat,
                // progress bars, top) brings a snapshot per SSH data chunk, and an
                // update per snapshot floods the main thread. A newer snapshot
                // replaces the one before it; other events keep their order.
                let mut batch = vec![event];
                while let Ok(next) = event_rx.try_recv() {
                    if matches!(next, ShellEvent::Snapshot(_))
                        && matches!(batch.last(), Some(ShellEvent::Snapshot(_)))
                    {
                        *batch.last_mut().expect("batch starts with one event") = next;
                    } else {
                        batch.push(next);
                    }
                }

                let res = this.update(cx, |view, cx| {
                    for event in batch {
                        view.handle_event(event, cx);
                    }
                });
                if res.is_err() {
                    break;
                }
            }
        });

        let preferences_subscription = cx.observe_global::<Preferences>(|view, cx| {
            view.cached_grid = None;
            cx.notify();
        });

        Self {
            _event_task: event_task,
            shell,
            last_snapshot: None,
            snapshot_version: 0,
            cached_grid: None,
            focus_handle,
            has_auto_focused: false,
            last_size: None,
            marked_text: None,
            selection: None,
            is_selecting: false,
            last_element_bounds: None,
            last_cell_size: None,
            scroll_accumulator: 0.0,
            drag_position: None,
            autoscroll_task: None,
            scrollbar_geom: None,
            scrollbar_drag: None,
            suggestions: SuggestionState::default(),
            search: SearchState::default(),
            _preferences_subscription: preferences_subscription,
            context_menu: None,
            copied_line_count: None,
            copied_hint_task: None,
            is_link_click: false,
            reported_press: None,
        }
    }

    fn on_increase_font_size(
        &mut self,
        _: &IncreaseFontSize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        Preferences::set_terminal_font_size(Preferences::terminal_font_size(cx) + 1.0, cx);
    }

    fn on_decrease_font_size(
        &mut self,
        _: &DecreaseFontSize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        Preferences::set_terminal_font_size(Preferences::terminal_font_size(cx) - 1.0, cx);
    }

    fn on_reset_font_size(
        &mut self,
        _: &ResetFontSize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        Preferences::set_terminal_font_size(DEFAULT_TERMINAL_FONT_SIZE, cx);
    }

    /// Wipe the scrollback and the screen, then send Ctrl-L (form feed) so the
    /// shell draws its prompt again. Readline and fish both do that without
    /// polluting shell history.
    fn on_clear_screen(&mut self, _: &ClearScreen, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_terminal_focused(window) {
            return;
        }
        self.selection = None;
        cx.notify();

        let shell = self.shell.clone();
        cx.spawn(async move |this, cx| {
            // One task, so the wipe reaches the worker before the Ctrl-L.
            shell.clear_scrollback().await;
            shell.send_input(vec![0x0c]).await;
            // The hits the search bar counted are gone with the scrollback.
            this.update(cx, |view, cx| view.recount_search(cx)).ok();
        })
        .detach();
    }

    fn on_scroll_page_up(
        &mut self,
        _: &ScrollPageUp,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.scroll_with_key(ScrollTarget::PageUp, cx);
    }

    fn on_scroll_page_down(
        &mut self,
        _: &ScrollPageDown,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.scroll_with_key(ScrollTarget::PageDown, cx);
    }

    fn on_scroll_to_top(&mut self, _: &ScrollToTop, _window: &mut Window, cx: &mut Context<Self>) {
        self.scroll_with_key(ScrollTarget::Top, cx);
    }

    fn on_scroll_to_bottom(
        &mut self,
        _: &ScrollToBottom,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.scroll_with_key(ScrollTarget::Bottom, cx);
    }

    fn scroll_with_key(&mut self, target: ScrollTarget, cx: &mut Context<Self>) {
        // The alternate screen has no scrollback, and vim, less and tmux want
        // these keys themselves. Passing the action on lets `on_key_down` send
        // the key to the app.
        if self
            .last_snapshot
            .as_ref()
            .is_some_and(|snap| snap.alt_screen)
        {
            cx.propagate();
            return;
        }
        let shell = self.shell.clone();
        cx.background_executor()
            .spawn(async move { shell.scroll_to(target).await })
            .detach();
    }

    /// Show the "Copied" note for a moment.
    fn show_copied_hint(&mut self, line_count: usize, cx: &mut Context<Self>) {
        self.copied_line_count = Some(line_count);
        self.copied_hint_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPIED_HINT_DURATION).await;
            this.update(cx, |view, cx| {
                view.copied_line_count = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn render_copied_hint(&self) -> Option<impl IntoElement + use<>> {
        let line_count = self.copied_line_count?;
        Some(
            div()
                .absolute()
                .bottom(px(8.))
                // Clear of the scrollbar, like the search bar.
                .right(px(20.))
                .px(px(8.))
                .py(px(3.))
                .bg(flint_theme::pop_surface())
                .border_1()
                .border_color(flint_theme::element_active())
                .rounded(px(4.))
                .text_size(px(11.))
                .text_color(flint_theme::muted_gray())
                .child(copied_hint_text(line_count)),
        )
    }

    /// Items for the right-click menu. Copy/Paste appear only when there's
    /// something to act on; Select All and Clear are always available.
    fn terminal_menu_items(&self, cx: &mut Context<Self>) -> Vec<ContextMenuItem> {
        let has_selection = self.selection.is_some();
        let has_clipboard = cx.read_from_clipboard().and_then(|i| i.text()).is_some();
        let mut items: Vec<ContextMenuItem> = Vec::new();

        if has_selection {
            items.push(ContextMenuItem::new(
                "term-copy",
                "Copy",
                cx.listener(|this, _, window, cx| {
                    this.context_menu = None;
                    this.on_copy_selection(&CopySelection, window, cx);
                }),
            ));
        }
        if has_clipboard {
            items.push(ContextMenuItem::new(
                "term-paste",
                "Paste",
                cx.listener(|this, _, window, cx| {
                    this.context_menu = None;
                    this.on_paste(&Paste, window, cx);
                }),
            ));
        }
        items.push(ContextMenuItem::new(
            "term-select-all",
            "Select All",
            cx.listener(|this, _, window, cx| {
                this.context_menu = None;
                this.on_select_all(&SelectAll, window, cx);
            }),
        ));
        items.push(ContextMenuItem::new(
            "term-clear",
            "Clear",
            cx.listener(|this, _, window, cx| {
                this.context_menu = None;
                this.on_clear_screen(&ClearScreen, window, cx);
            }),
        ));
        items
    }

    /// The grid size last sent to the remote PTY, as (rows, columns).
    pub fn grid_size(&self) -> Option<(u16, u16)> {
        self.last_size
    }

    /// Add the server's shell history to the suggestions popup. Called by the
    /// session once the history has been fetched.
    pub fn set_history(&mut self, commands: Vec<String>, cx: &mut Context<Self>) {
        self.suggestions.set_history(commands);
        cx.notify();
    }

    /// Write raw bytes to the shell on a detached task. Empty input is a no-op.
    pub(crate) fn send_input(&self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        if bytes.is_empty() {
            return;
        }
        let shell = self.shell.clone();
        cx.spawn(async move |_, _| {
            shell.send_input(bytes).await;
        })
        .detach();
    }

    /// The http/https link under a window position, if there is one.
    pub(crate) fn link_at_position(&self, position: Point<Pixels>) -> Option<TerminalLink> {
        let bounds = self.last_element_bounds?;
        if !bounds.contains(&position) || self.is_over_scrollbar(position) {
            return None;
        }
        let (cell_width, line_height) = self.last_cell_size?;
        let snapshot = self.last_snapshot.as_ref()?;
        // Offset 0 keeps the line relative to the viewport, like the snapshot rows.
        let (point, _) = pixel_to_grid(position, bounds, cell_width, line_height, 0);
        let row = usize::try_from(point.line).ok()?;
        terminal_link::link_at(snapshot, row, point.column)
    }

    /// The link to show as clickable: the one under the pointer while the
    /// platform modifier (Cmd on macOS, Ctrl elsewhere) is held.
    pub(crate) fn hovered_link(
        &self,
        position: Point<Pixels>,
        modifiers: Modifiers,
    ) -> Option<TerminalLink> {
        if !modifiers.secondary() || self.is_selecting || self.is_scrollbar_dragging() {
            return None;
        }
        self.link_at_position(position)
    }

    /// Open the link under a Cmd/Ctrl+click in the browser. Returns false when
    /// the click is not on a link, so it stays a normal click: the remote app
    /// still gets its Ctrl+click when mouse mode is on.
    fn try_open_link(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) -> bool {
        if event.button != MouseButton::Left || !event.modifiers.secondary() {
            return false;
        }
        let Some(link) = self.link_at_position(event.position) else {
            return false;
        };
        self.is_link_click = true;
        // The second press of a double click is still taken, but one page is enough.
        if event.click_count == 1 {
            cx.open_url(&link.url);
        }
        true
    }

    /// True when `pos` is on the scrollbar's clickable band.
    fn is_over_scrollbar(&self, pos: Point<Pixels>) -> bool {
        let Some(g) = self.scrollbar_geom else {
            return false;
        };
        g.history_size > 0
            && pos.x >= g.hit_x0
            && pos.x <= g.hit_x1
            && pos.y >= g.hit_y0
            && pos.y <= g.hit_y1
    }

    pub(crate) fn is_scrollbar_dragging(&self) -> bool {
        self.scrollbar_drag.is_some()
    }

    /// End an in-progress thumb drag. Returns whether one was actually active (so
    /// the mouse-up handler knows to swallow the event instead of ending a
    /// selection).
    pub(crate) fn end_scrollbar_drag(&mut self) -> bool {
        self.scrollbar_drag.take().is_some()
    }

    /// If `pos` lands on the scrollbar, begin a drag and return true so the caller
    /// skips starting a text selection. A click on the thumb grabs it where held;
    /// a click on the bare track jumps the thumb to center under the cursor.
    fn try_begin_scrollbar_drag(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        if !self.is_over_scrollbar(pos) {
            return false;
        }
        let Some(g) = self.scrollbar_geom else {
            return false;
        };
        let is_on_thumb = pos.y >= g.thumb_y && pos.y <= g.thumb_y + g.thumb_h;
        let grab = if is_on_thumb {
            pos.y - g.thumb_y
        } else {
            g.thumb_h * 0.5
        };
        self.scrollbar_drag = Some(grab);
        self.scrollbar_drag_move(pos, cx);
        true
    }

    /// Map the dragged thumb position to an absolute history offset and scroll there.
    pub(crate) fn scrollbar_drag_move(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(grab) = self.scrollbar_drag else {
            return;
        };
        let Some(g) = self.scrollbar_geom else { return };

        let track_y = g.track_y.as_f32();
        let travel = (g.track_h.as_f32() - g.thumb_h.as_f32()).max(0.0);
        let thumb_top = (pos.y.as_f32() - grab.as_f32()).clamp(track_y, track_y + travel);
        let norm = if travel > 0.0 {
            ((thumb_top - track_y) / travel).clamp(0.0, 1.0)
        } else {
            0.0
        };
        // Top of the track (norm 0) = fully scrolled into history; bottom = live tail.
        let offset = ((1.0 - norm) * g.history_size as f32).round() as usize;

        let shell = self.shell.clone();
        cx.background_executor()
            .spawn(async move { shell.scroll_to_offset(offset).await })
            .detach();
    }

    fn handle_event(&mut self, event: ShellEvent, cx: &mut Context<Self>) {
        match event {
            ShellEvent::Snapshot(snap) => {
                // Take the range alacritty has rotated with the scrolled buffer.
                // A `None` is ignored: a snapshot emitted before a selection
                // command was applied would wipe a selection the user just
                // started. Keyboard input clears `self.selection` on its own path.
                if let (Some(sel), Some(range)) = (self.selection.as_mut(), snap.selection) {
                    sel.range = Some(range);
                }
                if Preferences::is_suggestions_enabled(cx) {
                    self.suggestions.on_snapshot(&snap);
                } else {
                    self.suggestions.reset();
                }
                self.last_snapshot = Some(snap);
                self.snapshot_version = self.snapshot_version.wrapping_add(1);
                cx.notify();
            }
            ShellEvent::Exited => {
                tracing::debug!("remote shell exited");
                cx.emit(TerminalEvent::Exited);
            }
            ShellEvent::Disconnected => {
                tracing::debug!("terminal disconnected");
            }
            ShellEvent::Error(e) => {
                tracing::warn!(%e, "terminal error");
            }
        }
    }

    /// Window-space rect of the cursor cell in the current snapshot, used to
    /// anchor the suggestions popup. `None` before the first paint. The popup is
    /// only open on the live tail with a visible cursor, so the snapshot's
    /// cursor row and column are viewport cells.
    fn cursor_anchor_bounds(&self) -> Option<Bounds<Pixels>> {
        let snap = self.last_snapshot.as_ref()?;
        let bounds = self.last_element_bounds?;
        let (cell_w, line_h) = self.last_cell_size?;
        let x = bounds.origin.x + cell_w * snap.cursor.1 as f32;
        let y = bounds.origin.y + line_h * snap.cursor.0 as f32;
        Some(Bounds::new(Point::new(x, y), gpui::size(cell_w, line_h)))
    }

    pub fn set_marked_text(&mut self, text: String, cx: &mut Context<Self>) {
        self.marked_text = Some(text);
        cx.notify();
    }

    pub fn clear_marked_text(&mut self, cx: &mut Context<Self>) {
        if self.marked_text.is_some() {
            self.marked_text = None;
            cx.notify();
        }
    }

    pub fn commit_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.selection = None;
        // Typed text, spaces included, is an edit: drop a fill in flight and
        // let the next snapshot filter again. The popup stays open as long as
        // the new line (e.g. "git ") still has matches.
        self.suggestions.note_user_edit();
        cx.notify();
        self.send_input(text.as_bytes().to_vec(), cx);
    }

    fn start_selection(
        &mut self,
        point: GridPoint,
        side: SelectionSide,
        click_count: usize,
        cx: &mut Context<Self>,
    ) {
        let ty = match click_count {
            2 => SelectionType::Semantic,
            n if n >= 3 => SelectionType::Lines,
            _ => SelectionType::Simple,
        };

        self.selection = Some(SelectionState { range: None });

        // The emulator owns the selection, so it can rotate it to follow its
        // content as the buffer scrolls.
        let shell = self.shell.clone();
        cx.spawn(async move |this, cx| {
            let range = shell.set_selection(ty, point, side, point, side).await;
            this.update(cx, |view, cx| {
                if let Some(sel) = view.selection.as_mut() {
                    sel.range = range;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();

        cx.notify();
    }

    pub(crate) fn update_selection_head(
        &mut self,
        point: GridPoint,
        side: SelectionSide,
        cx: &mut Context<Self>,
    ) {
        if self.selection.is_none() {
            return;
        }

        // Move only the head; the emulator keeps (and rotates) the anchor.
        let shell = self.shell.clone();
        cx.spawn(async move |this, cx| {
            let range = shell.update_selection_head(point, side).await;
            this.update(cx, |view, cx| {
                if let Some(sel) = view.selection.as_mut() {
                    sel.range = range;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();

        cx.notify();
    }

    pub(crate) fn end_selection(&mut self) {
        self.is_selecting = false;
        self.drag_position = None;
        self.autoscroll_task = None;
    }

    pub(crate) fn set_drag_position(&mut self, position: Point<Pixels>) {
        self.drag_position = Some(position);
    }

    /// Start (or restart) the autoscroll timer for the current drag. Runs for the
    /// whole drag, idling while the cursor is inside the viewport and scrolling
    /// while it is past the top/bottom edge.
    fn start_autoscroll(&mut self, cx: &mut Context<Self>) {
        let task = cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(AUTOSCROLL_INTERVAL).await;
            let keep = this
                .update(cx, |view, cx| view.autoscroll_tick(cx))
                .unwrap_or(false);
            if !keep {
                break;
            }
        });
        self.autoscroll_task = Some(task);
    }

    /// One autoscroll step. Returns `false` once the drag is over so the timer
    /// loop exits.
    fn autoscroll_tick(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(pos) = self.drag_position else {
            return false;
        };
        if self.selection.is_none() {
            return false;
        }
        let Some(bounds) = self.last_element_bounds else {
            return true;
        };
        let Some((cell_width, line_height)) = self.last_cell_size else {
            return true;
        };
        let Some(snap) = self.last_snapshot.as_ref() else {
            return true;
        };
        let rows = snap.rows as i32;
        let cols = snap.cols as usize;
        let display_offset = snap.display_offset as i32;
        let history = snap.history_size as i32;
        let raw_offset = snap.display_offset;

        let top = bounds.origin.y;
        let bottom = bounds.origin.y + bounds.size.height;
        let (dir, past) = if pos.y < top {
            (1i32, top - pos.y)
        } else if pos.y > bottom {
            (-1i32, pos.y - bottom)
        } else {
            return true; // pointer inside the viewport: nothing to scroll
        };

        // Scroll faster the further the cursor is past the edge.
        let step = ((past.as_f32() / line_height.as_f32()).floor() as i32 + 1).clamp(1, 8);
        let delta = dir * step;

        let shell = self.shell.clone();
        cx.background_executor()
            .spawn(async move { shell.scroll_delta(delta).await })
            .detach();

        // Pin the head to the edge row, anticipating the post-scroll display
        // offset so the line being revealed is included rather than trailing.
        let anticipated = (display_offset + delta).clamp(0, history);
        let edge_row = if dir > 0 { 0 } else { rows - 1 };
        let (mp, side) = pixel_to_grid(pos, bounds, cell_width, line_height, raw_offset);
        let column = mp.column.min(cols.saturating_sub(1));
        self.update_selection_head(
            GridPoint {
                line: edge_row - anticipated,
                column,
            },
            side,
            cx,
        );
        true
    }

    fn on_copy_selection(
        &mut self,
        _: &CopySelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selection.is_none() || !self.is_terminal_focused(window) {
            return;
        }
        // The emulator holds the rotated selection, so the text is read from
        // it. Sending a cached anchor back would overwrite it.
        let shell = self.shell.clone();

        cx.spawn(async move |this, cx| {
            if let Some(text) = shell.extract_selection_text().await {
                this.update(cx, |view, cx| {
                    let line_count = text.lines().count();
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                    view.show_copied_hint(line_count, cx);
                })
                .ok();
            }
        })
        .detach();
    }

    fn on_select_all(&mut self, _: &SelectAll, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_terminal_focused(window) {
            return;
        }
        let shell = self.shell.clone();
        cx.spawn(async move |this, cx| {
            if let Some(range) = shell.select_all().await {
                this.update(cx, |view, cx| {
                    view.selection = Some(SelectionState { range: Some(range) });
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// True while the grid itself holds focus. The search bar's field is a
    /// descendant of this view, and the "Terminal" key bindings stay live while
    /// it has focus, so every handler that writes to the shell has to ask first.
    fn is_terminal_focused(&self, window: &Window) -> bool {
        self.focus_handle.is_focused(window)
    }

    fn on_tab(&mut self, _: &Tab, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_terminal_focused(window) {
            return;
        }
        // Tab belongs to the shell's completion. Hide the popup for this line
        // so it does not sit on top of the shell's completion list.
        self.suggestions.dismiss_for_line();
        cx.notify();
        self.send_input(b"\t".to_vec(), cx);
    }

    /// On a plain Enter, hand the current line to the suggestions so the
    /// command is learned for the rest of the session.
    fn note_command_run(&mut self, keystroke: &Keystroke, cx: &App) {
        let m = &keystroke.modifiers;
        let is_plain_enter = matches!(keystroke.key.as_str(), "enter" | "return")
            && !m.control
            && !m.alt
            && !m.platform
            && !m.shift;
        if !is_plain_enter || !Preferences::is_suggestions_enabled(cx) {
            return;
        }
        if let Some(snap) = self.last_snapshot.as_ref() {
            self.suggestions.note_enter(snap);
        }
    }

    fn on_shift_tab(&mut self, _: &ShiftTab, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_terminal_focused(window) {
            return;
        }
        self.suggestions.dismiss_for_line();
        cx.notify();
        self.send_input(b"\x1b[Z".to_vec(), cx);
    }

    fn on_paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        // Ctrl+Shift+V is a terminal chord the search field does not claim, so
        // without this the clipboard would land on the remote prompt while the
        // user is typing a query.
        if !self.is_terminal_focused(window) {
            return;
        }
        let is_bracketed = self
            .last_snapshot
            .as_ref()
            .is_some_and(|snap| snap.bracketed_paste);
        let shell = self.shell.clone();
        // On X11/Wayland the clipboard lives in the app that owns it, so a
        // synchronous read stalls the frame until that app answers.
        let clipboard = cx.read_from_clipboard_async();
        cx.spawn(async move |_, _| {
            let item = match clipboard.await {
                Ok(item) => item,
                Err(e) => {
                    tracing::warn!(?e, "clipboard read failed, nothing pasted");
                    return;
                }
            };
            let Some(text) = item.and_then(|item| item.text()) else {
                return;
            };
            shell.send_input(paste_bytes(&text, is_bracketed)).await;
        })
        .detach();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // The search bar renders inside this div, so keys typed in its field
        // bubble through here. Without this guard they would also reach the
        // remote shell: Ctrl-D would end the session, Up would recall a command
        // at the live prompt, F-keys and Alt chords would leak escape sequences.
        if !self.is_terminal_focused(window) {
            return;
        }

        // Suggestions popup key handling takes priority over raw forwarding.
        if self.suggestions.is_open {
            let m = &event.keystroke.modifiers;
            let is_plain = !m.control && !m.alt && !m.platform;
            match event.keystroke.key.as_str() {
                // Right accepts fish-style: the top row, or the highlighted one.
                // The popup is only open with the cursor at the end of the line,
                // so Right has no cursor movement to give up.
                "right" if is_plain && !m.shift => {
                    if let Some(bytes) = self.suggestions.accept() {
                        self.send_input(bytes, cx);
                        cx.stop_propagation();
                        cx.notify();
                        return;
                    }
                }
                // Down enters the list and steps down it; past the bottom row it
                // leaves the popup and Down falls through to the shell so the user
                // walks forward through real terminal history.
                "down" if is_plain && !m.shift => {
                    match self.suggestions.navigate_down() {
                        Some(NavAction::Moved(bytes)) => {
                            self.send_input(bytes, cx);
                            cx.stop_propagation();
                            cx.notify();
                            return;
                        }
                        Some(NavAction::Exit(mut bytes)) => {
                            // One write, so the shell gets the restored query
                            // before the Down escape.
                            if let Some(down) = self.key_bytes(&event.keystroke, cx) {
                                bytes.extend_from_slice(&down);
                            }
                            self.send_input(bytes, cx);
                            cx.stop_propagation();
                            cx.notify();
                            return;
                        }
                        None => {}
                    }
                }
                // Up only steps within the list once Down has entered it. From
                // the unentered or top row it leaves the popup and Up falls
                // through to the shell so the user browses real terminal history.
                "up" if is_plain && !m.shift => {
                    match self.suggestions.navigate_up() {
                        Some(NavAction::Moved(bytes)) => {
                            self.send_input(bytes, cx);
                            cx.stop_propagation();
                            cx.notify();
                            return;
                        }
                        Some(NavAction::Exit(mut bytes)) => {
                            // One write, so the shell gets the restored query
                            // before the Up escape.
                            if let Some(up) = self.key_bytes(&event.keystroke, cx) {
                                bytes.extend_from_slice(&up);
                            }
                            self.send_input(bytes, cx);
                            cx.stop_propagation();
                            cx.notify();
                            return;
                        }
                        None => {}
                    }
                }
                // Esc hides the popup and keeps it hidden for this line.
                "escape" if is_plain => {
                    self.suggestions.dismiss_for_line();
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                // Enter runs the current line and closes the popup. Backspace is
                // an edit: drop any fill and let the shorter line filter again,
                // so the popup stays while matches remain. Both fall through to
                // the normal byte path.
                "enter" | "return" => self.suggestions.close(),
                "backspace" => self.suggestions.note_user_edit(),
                // Option+Space on macOS is sent from here as a plain space and
                // never reaches commit_text, so it is noted as an edit here.
                "space" if m.alt => self.suggestions.note_user_edit(),
                _ => {}
            }
            cx.notify();
        }

        self.note_command_run(&event.keystroke, cx);

        if let Some(bytes) = self.key_bytes(&event.keystroke, cx) {
            // Typing drops the highlight. The emulator keeps its own selection
            // until the next one replaces it.
            self.selection = None;
            cx.stop_propagation();
            self.send_input(bytes, cx);
        }
        // A key with no bytes is left to the OS text input, which calls
        // TerminalInputHandler::replace_text_in_range.
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        // A release that never arrived (button let go outside the window) must
        // not make this click look like part of a link click.
        self.is_link_click = false;
        self.reported_press = None;

        // A click on the scrollbar starts a thumb drag instead of a text selection.
        if event.button == MouseButton::Left && self.try_begin_scrollbar_drag(event.position, cx) {
            cx.stop_propagation();
            return;
        }

        // Before mouse reporting and selection: a link click is neither.
        if self.try_open_link(event, cx) {
            cx.stop_propagation();
            return;
        }

        let Some(bounds) = self.last_element_bounds else {
            return;
        };
        let Some((cell_width, line_height)) = self.last_cell_size else {
            return;
        };
        let display_offset = self
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

        let mouse_mode = self
            .last_snapshot
            .as_ref()
            .map(|s| s.mouse_mode)
            .unwrap_or_default();

        // Shift keeps the click for a selection while an app has mouse mode on.
        if mouse_mode.any_active() && !event.modifiers.shift {
            if let Some(button) = report_button(event.button) {
                self.reported_press = Some(event.button);
                self.send_mouse_report(
                    MouseEventKind::Press,
                    button,
                    event.position,
                    event.modifiers,
                    cx,
                );
            }
            cx.stop_propagation();
            return;
        }

        // Right-click (outside mouse-reporting mode): open the context menu.
        if event.button == MouseButton::Right {
            self.context_menu = Some(event.position);
            cx.notify();
            cx.stop_propagation();
            return;
        }

        // Only the left button selects.
        if event.button != MouseButton::Left {
            return;
        }

        if event.modifiers.shift && self.selection.is_some() {
            self.update_selection_head(abs_point, side, cx);
        } else {
            self.start_selection(abs_point, side, event.click_count, cx);
        }

        // Mark the drag active so mouse-move only mutates the selection until the
        // matching mouse-up; drive autoscroll for the duration of this drag.
        self.is_selecting = true;
        self.drag_position = Some(event.position);
        self.start_autoscroll(cx);
    }

    /// The viewport cell under a window position, for a mouse report. Kept
    /// inside the grid: the element is a little larger than its rows and
    /// columns, and a drag or a release can end outside it.
    fn report_point(&self, position: Point<Pixels>) -> Option<GridPoint> {
        let bounds = self.last_element_bounds?;
        let (cell_width, line_height) = self.last_cell_size?;
        let snapshot = self.last_snapshot.as_ref()?;
        let (point, _) = pixel_to_grid(position, bounds, cell_width, line_height, 0);
        Some(clamp_to_grid(point, snapshot.rows, snapshot.cols))
    }

    /// Reports go out on the same path as keys, so the app gets a press, its
    /// drag, its release and any typing in the order they happened.
    fn send_mouse_report(
        &self,
        kind: MouseEventKind,
        button: ReportButton,
        position: Point<Pixels>,
        modifiers: Modifiers,
        cx: &mut Context<Self>,
    ) {
        let Some(snapshot) = self.last_snapshot.as_ref() else {
            return;
        };
        let Some(point) = self.report_point(position) else {
            return;
        };
        let report = encode_mouse_event(
            snapshot.mouse_mode,
            kind,
            button,
            point,
            modifiers.shift,
            modifiers.control,
            modifiers.alt,
        );
        if let Some(bytes) = report {
            self.send_input(bytes, cx);
        }
    }

    /// Report a left-button drag to the app that got the press. Returns false
    /// when the press was not reported, so the drag is not the app's.
    pub(crate) fn report_mouse_drag(
        &mut self,
        event: &MouseMoveEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.reported_press != Some(MouseButton::Left) {
            return false;
        }
        self.send_mouse_report(
            MouseEventKind::Motion,
            ReportButton::Left,
            event.position,
            event.modifiers,
            cx,
        );
        true
    }

    /// Report the release of a button whose press was reported. Returns false
    /// for any other release.
    pub(crate) fn report_mouse_release(
        &mut self,
        event: &MouseUpEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.reported_press != Some(event.button) {
            return false;
        }
        self.reported_press = None;
        if let Some(button) = report_button(event.button) {
            self.send_mouse_report(
                MouseEventKind::Release,
                button,
                event.position,
                event.modifiers,
                cx,
            );
        }
        true
    }

    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let line_height_f32 = self
            .last_cell_size
            .map(|(_, line_height)| line_height.as_f32())
            .unwrap_or_else(|| Preferences::terminal_font_size(cx) * LINE_HEIGHT_FACTOR);
        let delta_lines = match event.delta {
            ScrollDelta::Lines(p) => p.y,
            ScrollDelta::Pixels(p) => p.y.as_f32() / line_height_f32,
        };
        let lines = take_whole_lines(&mut self.scroll_accumulator, delta_lines);
        cx.stop_propagation();
        if lines == 0 {
            return;
        }

        let Some(snapshot) = self.last_snapshot.as_ref() else {
            return;
        };
        let mouse_mode = snapshot.mouse_mode;

        if mouse_mode.any_active() {
            let bytes = self.wheel_report_bytes(event, mouse_mode, lines);
            self.send_input(bytes, cx);
            return;
        }

        // The alternate screen has no scrollback, so a pager or editor
        // without mouse mode gets the wheel as arrow keys instead.
        if snapshot.alt_screen {
            if snapshot.is_alternate_scroll {
                let bytes = terminal_keymap::wheel_arrow_bytes(lines, snapshot.is_app_cursor);
                self.send_input(bytes, cx);
            }
            return;
        }

        let shell = self.shell.clone();
        cx.background_executor()
            .spawn(async move {
                shell.scroll_delta(lines).await;
            })
            .detach();
    }

    /// One wheel report per line, for an app that has mouse mode on.
    fn wheel_report_bytes(
        &self,
        event: &ScrollWheelEvent,
        mouse_mode: MouseModeFlags,
        lines: i32,
    ) -> Vec<u8> {
        let Some(point) = self.report_point(event.position) else {
            return Vec::new();
        };
        let button = if lines > 0 {
            ReportButton::WheelUp
        } else {
            ReportButton::WheelDown
        };

        encode_mouse_event(
            mouse_mode,
            MouseEventKind::Press,
            button,
            point,
            event.modifiers.shift,
            event.modifiers.control,
            event.modifiers.alt,
        )
        .map(|report| report.repeat(lines.unsigned_abs() as usize))
        .unwrap_or_default()
    }

    fn key_bytes(&self, keystroke: &Keystroke, cx: &App) -> Option<Vec<u8>> {
        let is_app_cursor = self.last_snapshot.as_ref().is_some_and(|s| s.is_app_cursor);
        terminal_keymap::keystroke_to_bytes(
            keystroke,
            Preferences::is_option_meta(cx),
            is_app_cursor,
        )
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.has_auto_focused {
            self.has_auto_focused = true;
            let fh = self.focus_handle.clone();
            cx.on_next_frame(window, move |_, window, cx| {
                window.focus(&fh, cx);
            });
        }

        // The suggestions popup is anchored to the cursor of the current
        // snapshot. The rect painted last frame lags behind the matches and
        // wobbles while typing. The popup flips above the line when it does not
        // fit below inside the terminal area; measured against the window, it
        // would be pushed up over the input line first.
        let area_bottom = self
            .last_element_bounds
            .map(|b| b.origin.y + b.size.height)
            .unwrap_or_else(|| window.viewport_size().height);
        let popup = self
            .cursor_anchor_bounds()
            .and_then(|b| self.suggestions.render_popup(b, area_bottom, cx));

        let context_menu = self.context_menu.map(|pos| {
            let items = self.terminal_menu_items(cx);
            ContextMenu::new(
                "terminal-context-menu",
                pos,
                gpui::Anchor::TopLeft,
                cx.listener(|this, _, _, cx| {
                    this.context_menu = None;
                    cx.notify();
                }),
                items,
            )
        });

        let search_bar = self.render_search_bar(window, cx);

        div()
            .id("terminal-view")
            .track_focus(&self.focus_handle)
            .key_context("Terminal")
            .size_full()
            // Anchor for the search bar, which pins itself to the top-right.
            .relative()
            .on_action(cx.listener(Self::on_tab))
            .on_action(cx.listener(Self::on_shift_tab))
            .on_action(cx.listener(Self::on_paste))
            .on_action(cx.listener(Self::on_copy_selection))
            .on_action(cx.listener(Self::on_select_all))
            .on_action(cx.listener(Self::on_increase_font_size))
            .on_action(cx.listener(Self::on_decrease_font_size))
            .on_action(cx.listener(Self::on_reset_font_size))
            .on_action(cx.listener(Self::on_clear_screen))
            .on_action(cx.listener(Self::on_scroll_page_up))
            .on_action(cx.listener(Self::on_scroll_page_down))
            .on_action(cx.listener(Self::on_scroll_to_top))
            .on_action(cx.listener(Self::on_scroll_to_bottom))
            .on_action(cx.listener(Self::on_toggle_search))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_mouse_down))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .child(TerminalElement::new(cx.entity()))
            .children(search_bar)
            .children(self.render_copied_hint())
            .children(popup)
            .children(context_menu)
    }
}

/// The button to name in a mouse report. `None` for the buttons the protocol
/// has no code for here (back and forward).
fn report_button(button: MouseButton) -> Option<ReportButton> {
    match button {
        MouseButton::Left => Some(ReportButton::Left),
        MouseButton::Middle => Some(ReportButton::Middle),
        MouseButton::Right => Some(ReportButton::Right),
        _ => None,
    }
}

/// Fold a wheel delta into whole lines, keeping the fraction for the next
/// event. A trackpad sends many deltas well under one line per swipe.
fn take_whole_lines(accumulator: &mut f32, delta_lines: f32) -> i32 {
    // A leftover from the other direction would eat the start of this swipe.
    if *accumulator * delta_lines < 0.0 {
        *accumulator = 0.0;
    }
    *accumulator += delta_lines;
    let lines = accumulator.trunc();
    *accumulator -= lines;
    lines as i32
}

/// Text of the note shown after a copy. Only a copy of several lines names
/// its size: that is when the user cannot see at a glance what was taken.
fn copied_hint_text(line_count: usize) -> String {
    if line_count < 2 {
        return "Copied".to_string();
    }
    format!("Copied {line_count} lines")
}

/// Turn clipboard text into the bytes to send for a paste.
///
/// With bracketed paste on, the text goes between the ESC[200~ / ESC[201~
/// markers so the app knows it was pasted: shells then keep a multi-line paste
/// as one editable command instead of running each line, and editors skip
/// auto-indent. ESC and Ctrl-C are dropped from the payload: they would let
/// pasted text close the bracket early and run as keystrokes.
///
/// Without it there is no way to tell the app "this was pasted", so newlines
/// become \r, which is what the Enter key sends.
fn paste_bytes(text: &str, is_bracketed: bool) -> Vec<u8> {
    if !is_bracketed {
        return text.replace("\r\n", "\r").replace('\n', "\r").into_bytes();
    }

    let filtered = text.replace(['\x1b', '\x03'], "");
    let mut bytes = Vec::with_capacity(filtered.len() + 12);
    bytes.extend_from_slice(b"\x1b[200~");
    bytes.extend_from_slice(filtered.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

#[cfg(test)]
mod tests {
    use super::{copied_hint_text, paste_bytes, take_whole_lines};

    #[test]
    fn copy_note_counts_lines_only_for_a_multi_line_copy() {
        assert_eq!(copied_hint_text(0), "Copied");
        assert_eq!(copied_hint_text(1), "Copied");
        assert_eq!(copied_hint_text(1240), "Copied 1240 lines");
    }

    #[test]
    fn plain_paste_turns_newlines_into_carriage_returns() {
        assert_eq!(paste_bytes("a\r\nb\nc", false), b"a\rb\rc".to_vec());
    }

    #[test]
    fn small_wheel_deltas_add_up_to_whole_lines() {
        let mut accumulator = 0.0;
        assert_eq!(take_whole_lines(&mut accumulator, 0.4), 0);
        assert_eq!(take_whole_lines(&mut accumulator, 0.4), 0);
        assert_eq!(take_whole_lines(&mut accumulator, 0.4), 1);
        assert_eq!(take_whole_lines(&mut accumulator, -2.5), -2);
    }

    #[test]
    fn one_wheel_notch_is_taken_at_once() {
        let mut accumulator = 0.0;
        assert_eq!(take_whole_lines(&mut accumulator, 3.0), 3);
        assert_eq!(take_whole_lines(&mut accumulator, -1.0), -1);
        assert_eq!(accumulator, 0.0);
    }

    #[test]
    fn turning_the_wheel_back_drops_the_leftover() {
        let mut accumulator = 0.0;
        assert_eq!(take_whole_lines(&mut accumulator, 0.9), 0);
        // Without the reset, 0.9 - 0.6 would hide the first 0.6 lines down.
        assert_eq!(take_whole_lines(&mut accumulator, -0.6), 0);
        assert_eq!(take_whole_lines(&mut accumulator, -0.6), -1);
    }

    #[test]
    fn bracketed_paste_keeps_newlines_and_wraps_the_text() {
        assert_eq!(
            paste_bytes("a\nb", true),
            b"\x1b[200~a\nb\x1b[201~".to_vec()
        );
    }

    #[test]
    fn bracketed_paste_drops_escape_and_ctrl_c() {
        assert_eq!(
            paste_bytes("a\x1b[201~\x03b", true),
            b"\x1b[200~a[201~b\x1b[201~".to_vec()
        );
    }
}
