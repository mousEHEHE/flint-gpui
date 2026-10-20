//! Cmd+F search bar for the terminal.
//!
//! The bar sits over the top-right corner of the terminal. Typing highlights
//! every hit on the visible rows; Enter / Shift+Enter walk all hits in the whole
//! scrollback and scroll them into view. The matching itself happens in the shell
//! worker, which owns the terminal buffer: see [`flint_core::term_emulator`].

use gpui::prelude::*;
use gpui::{
    actions, div, px, AnyElement, App, Context, Entity, Focusable as _, KeyBinding, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    IconName, Sizable as _,
};

use crate::theme as flint_theme;
use crate::ui::keyboard_layout::bind_chord;
use crate::ui::terminal_view::{TerminalView, SIDE_PADDING};

actions!(
    terminal_search,
    [ToggleSearch, SearchNext, SearchPrev, CloseSearch]
);

/// Key context of the bar. It lives on the bar itself, not on the terminal, so
/// Enter and Escape only mean "step" and "close" while the field has focus.
const CONTEXT: &str = "TerminalSearch";

pub fn init(cx: &mut App) {
    // The input field binds enter/escape too, but for a single-line input it
    // lets them through, so these still fire and consume them before the
    // terminal's raw key handler can send them to the remote shell.
    cx.bind_keys([
        KeyBinding::new("enter", SearchNext, Some(CONTEXT)),
        KeyBinding::new("shift-enter", SearchPrev, Some(CONTEXT)),
        KeyBinding::new("escape", CloseSearch, Some(CONTEXT)),
    ]);

    #[cfg(target_os = "macos")]
    bind_chord(cx, "cmd-f", ToggleSearch, Some("Terminal"));

    // Cmd is free on macOS, but on Linux/Windows Ctrl+F belongs to the shell
    // (forward-char in readline), so the chord takes Shift as well.
    #[cfg(not(target_os = "macos"))]
    bind_chord(cx, "ctrl-shift-f", ToggleSearch, Some("Terminal"));
}

#[derive(Default)]
pub struct SearchState {
    pub is_open: bool,
    input: Option<Entity<InputState>>,
    /// Mirror of the field's text, so read paths don't need `cx` and stale
    /// replies from the worker can be told apart from current ones.
    query: String,
    /// 1-based position of the match the user stepped to; 0 = not stepped yet.
    position: usize,
    /// Matches in the whole buffer. `None` = not counted (yet, or at all,
    /// because the session ended), which must not be shown as "no matches".
    total: Option<usize>,
}

impl TerminalView {
    pub(crate) fn on_toggle_search(
        &mut self,
        _: &ToggleSearch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Always open and focus, never close. On macOS the field swallows Cmd+F
        // itself, so a "toggle" would only ever fire with the terminal focused,
        // where closing the bar is not what the user is asking for. Escape
        // closes it.
        self.suggestions.close();
        let input = self.search_input(window, cx);
        input.update(cx, |state, cx| state.focus(window, cx));
        self.search.is_open = true;
        cx.notify();
    }

    pub(crate) fn on_search_next(
        &mut self,
        _: &SearchNext,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step_search(true, cx);
    }

    pub(crate) fn on_search_prev(
        &mut self,
        _: &SearchPrev,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step_search(false, cx);
    }

    pub(crate) fn on_close_search(
        &mut self,
        _: &CloseSearch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_search(window, cx);
    }

    fn search_input(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<InputState> {
        if let Some(input) = &self.search.input {
            return input.clone();
        }

        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Find\u{2026}"));
        cx.subscribe(&input, |this, input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.set_search_query(input.read(cx).value().to_string(), cx);
            }
        })
        .detach();
        self.search.input = Some(input.clone());
        input
    }

    fn set_search_query(&mut self, query: String, cx: &mut Context<Self>) {
        self.search.query = query.clone();
        self.search.position = 0;
        // The count keeps its old value until the worker replies: dropping it
        // here would flash a blank counter between the keystroke and the reply,
        // which on a long scrollback is long enough to see.
        if query.is_empty() {
            self.search.total = None;
        }
        cx.notify();

        let shell = self.shell.clone();
        cx.spawn(async move |this, cx| {
            let total = shell.set_search(query.clone()).await;
            let _ = this.update(cx, |view, cx| {
                // Typing while the worker was counting: the reply belongs to a
                // query that is already gone.
                if view.search.query != query {
                    return;
                }
                view.search.total = total;
                cx.notify();
            });
        })
        .detach();
    }

    /// Count the matches again after the buffer changed under the search.
    pub(crate) fn recount_search(&mut self, cx: &mut Context<Self>) {
        if self.search.query.is_empty() {
            return;
        }
        self.set_search_query(self.search.query.clone(), cx);
    }

    fn step_search(&mut self, forward: bool, cx: &mut Context<Self>) {
        // Nothing to step through, or the session is gone and the buffer can no
        // longer be searched.
        if !matches!(self.search.total, Some(total) if total > 0) {
            return;
        }

        let query = self.search.query.clone();
        let shell = self.shell.clone();
        cx.spawn(async move |this, cx| {
            let progress = shell.step_search(forward).await;
            let _ = this.update(cx, |view, cx| {
                // Counting the whole scrollback takes a moment; by now the user
                // may have typed on, and these numbers belong to the old query.
                if view.search.query != query {
                    return;
                }
                let Some((position, total)) = progress else {
                    return;
                };
                view.search.position = position;
                view.search.total = Some(total);
                cx.notify();
            });
        })
        .detach();
    }

    fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search.is_open = false;
        self.search.query.clear();
        self.search.position = 0;
        self.search.total = None;
        if let Some(input) = self.search.input.clone() {
            input.update(cx, |state, cx| state.set_value("", window, cx));
        }

        let shell = self.shell.clone();
        cx.spawn(async move |_, _| {
            shell.set_search(String::new()).await;
        })
        .detach();

        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// Text on the right of the field: how many hits there are, and which one the
    /// user is on once they start stepping. Stays empty while the count is
    /// unknown, so a session that ended mid-search claims nothing.
    fn search_status(&self) -> String {
        if self.search.query.is_empty() {
            return String::new();
        }
        match (self.search.position, self.search.total) {
            (_, None) => String::new(),
            (_, Some(0)) => "no matches".to_string(),
            (0, Some(total)) => format!("{total} found"),
            (position, Some(total)) => format!("{position}/{total}"),
        }
    }

    pub(crate) fn render_search_bar(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.search.is_open {
            return None;
        }
        let input = self.search.input.as_ref()?;

        // Clicking the terminal (or switching tabs) takes focus away while the
        // bar stays open. Dim the border there, or the bar keeps claiming keys
        // it no longer receives.
        let border = if input.read(cx).focus_handle(cx).is_focused(window) {
            flint_theme::border_focused()
        } else {
            flint_theme::element_active()
        };

        Some(
            h_flex()
                .key_context(CONTEXT)
                .on_action(cx.listener(Self::on_search_next))
                .on_action(cx.listener(Self::on_search_prev))
                .on_action(cx.listener(Self::on_close_search))
                .occlude()
                .absolute()
                .top(px(8.))
                // Clear of the scrollbar's grab band on the right edge, or the
                // bar's hitbox would swallow clicks meant for the thumb.
                .right(px(20. + SIDE_PADDING))
                .w(px(300.))
                .items_center()
                .gap(px(6.))
                .px(px(8.))
                .py(px(4.))
                .bg(flint_theme::pop_surface())
                .border_1()
                .border_color(border)
                .rounded(px(4.))
                .shadow_md()
                .child(
                    div()
                        .flex_1()
                        .child(Input::new(input).xsmall().appearance(false)),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(px(10.))
                        .text_color(flint_theme::muted_gray())
                        .child(self.search_status()),
                )
                .child(
                    Button::new("search-prev")
                        .ghost()
                        .icon(IconName::ChevronUp)
                        .xsmall()
                        .tooltip("Previous match")
                        .on_click(cx.listener(|this, _, _window, cx| this.step_search(false, cx))),
                )
                .child(
                    Button::new("search-next")
                        .ghost()
                        .icon(IconName::ChevronDown)
                        .xsmall()
                        .tooltip("Next match")
                        .on_click(cx.listener(|this, _, _window, cx| this.step_search(true, cx))),
                )
                .child(
                    Button::new("search-close")
                        .ghost()
                        .icon(IconName::Close)
                        .xsmall()
                        .tooltip("Close")
                        .on_click(cx.listener(|this, _, window, cx| this.close_search(window, cx))),
                )
                .into_any_element(),
        )
    }
}
