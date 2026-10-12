//! Terminal tabs: several shells for one host over the same SSH connection,
//! so a second shell needs no second login. Also the switch that hides the
//! file panes to give the terminal the whole tab.
//!
//! The methods belong to `ServerSession`. They live here so that
//! `server_session.rs` stays about the connection lifecycle.

use gpui::prelude::*;
use gpui::{
    div, px, AnyElement, Context, Entity, Focusable as _, MouseButton, SharedString, Window,
};
use gpui_component::{h_flex, tooltip::Tooltip, ActiveTheme as _, StyledExt as _, WindowExt as _};

use crate::shell::ShellHandle;
use crate::theme;
use crate::ui::server_session::{ServerSession, ServerSessionEvent};
use crate::ui::terminal_view::{TerminalEvent, TerminalView};

/// Tabs past this would not fit the header. It is also close to what a server
/// allows: OpenSSH's default `MaxSessions` is 10 and SFTP takes two of them.
const MAX_TERMINALS: usize = 8;

/// Shown in the tooltips and the command palette. Must track the bindings in
/// `app_view::init`.
pub const NEW_TERMINAL_SHORTCUT: &str = if cfg!(target_os = "macos") {
    "\u{21E7}\u{2318}T"
} else {
    "Ctrl+Shift+N"
};
pub const TOGGLE_FILES_SHORTCUT: &str = if cfg!(target_os = "macos") {
    "\u{2318}B"
} else {
    "Ctrl+Shift+B"
};

/// Which tab is active after the one at `closed` is removed, leaving
/// `remaining` tabs. The active shell stays active; closing it moves to the
/// tab that slid into its place.
fn active_after_close(active: usize, closed: usize, remaining: usize) -> usize {
    let next = if closed < active { active - 1 } else { active };
    next.min(remaining.saturating_sub(1))
}

impl ServerSession {
    pub fn active_terminal(&self) -> Option<&Entity<TerminalView>> {
        self.terminals.get(self.active_terminal)
    }

    /// Wrap a freshly opened shell in a terminal tab at the end of the strip.
    pub(crate) fn add_terminal(&mut self, shell: ShellHandle, cx: &mut Context<Self>) {
        let history = self.shell_history.clone();
        let terminal = cx.new(|cx| {
            let mut view = TerminalView::new(shell, cx);
            if !history.is_empty() {
                view.set_history(history, cx);
            }
            view
        });
        cx.subscribe(
            &terminal,
            |this, terminal, event: &TerminalEvent, cx| match event {
                TerminalEvent::Exited => this.close_exited_terminal(&terminal, cx),
            },
        )
        .detach();
        self.terminals.push(terminal);
    }

    fn can_open_terminal(&self) -> bool {
        self.browser.is_some()
            && self.disconnected.is_none()
            && self.reconnecting.is_none()
            && self.terminals.len() < MAX_TERMINALS
    }

    /// Open one more shell on the live connection and show it.
    pub fn open_extra_terminal(&mut self, cx: &mut Context<Self>) {
        if !self.can_open_terminal() {
            return;
        }
        let Some(conn) = self.connection.clone() else {
            return;
        };
        let generation = self.connection_generation;
        // Start at the size of the tab it will replace on screen, so the shell
        // does not draw its first prompt for a default 80x24 grid.
        let (rows, cols) = self
            .active_terminal()
            .and_then(|terminal| terminal.read(cx).grid_size())
            .unwrap_or((24, 80));

        cx.spawn(async move |this, cx| {
            let result = conn.open_shell(rows, cols).await;
            this.update(cx, |session, cx| match result {
                // A reconnect in the meantime replaced every shell. This one
                // belongs to the old connection, so it is dropped.
                Ok(_) if session.connection_generation != generation => {}
                Ok(_) if !session.can_open_terminal() => {}
                Ok(shell) => {
                    session.add_terminal(shell, cx);
                    session.active_terminal = session.terminals.len() - 1;
                    session.is_terminal_focus_pending = true;
                    tracing::info!(
                        host = %session.conn_info.display_name(),
                        count = session.terminals.len(),
                        "extra terminal opened",
                    );
                    cx.notify();
                }
                Err(error) => {
                    tracing::warn!(
                        host = %session.conn_info.display_name(),
                        %error,
                        "could not open an extra terminal",
                    );
                    // The usual cause is the server's MaxSessions limit.
                    cx.emit(ServerSessionEvent::StatusChanged(format!(
                        "Error: could not open another terminal ({error}). \
                         The server may limit sessions per connection."
                    )));
                }
            })
            .ok();
        })
        .detach();
    }

    fn activate_terminal(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.terminals.len() {
            return;
        }
        self.active_terminal = ix;
        self.is_terminal_focus_pending = true;
        cx.notify();
    }

    /// Close one tab. Dropping the view drops the last handle to its shell,
    /// which closes the channel. The last tab cannot be closed.
    fn close_terminal(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.terminals.len() < 2 || ix >= self.terminals.len() {
            return;
        }
        let was_active = ix == self.active_terminal;
        self.terminals.remove(ix);
        self.active_terminal = active_after_close(self.active_terminal, ix, self.terminals.len());
        // The focus went away with the closed view.
        if was_active {
            self.is_terminal_focus_pending = true;
        }
        cx.notify();
    }

    /// The user left a shell (`exit`, Ctrl+D). Its tab closes, the session
    /// stays. The only shell is kept on screen with its last output.
    fn close_exited_terminal(&mut self, terminal: &Entity<TerminalView>, cx: &mut Context<Self>) {
        let Some(ix) = self
            .terminals
            .iter()
            .position(|candidate| candidate == terminal)
        else {
            return;
        };
        self.close_terminal(ix, cx);
    }

    /// Hide or show the file panes. Hidden, the terminal fills the tab.
    pub fn toggle_sftp_pane(&mut self, cx: &mut Context<Self>) {
        self.is_sftp_hidden = !self.is_sftp_hidden;
        // A focused file pane may have just left the screen.
        self.is_terminal_focus_pending = true;
        cx.notify();
    }

    /// Tabs change from places that have no `Window` (a shell exiting, an
    /// async open), so the focus is moved here, on the next render.
    pub(crate) fn focus_terminal_if_pending(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !std::mem::take(&mut self.is_terminal_focus_pending) {
            return;
        }
        // A dialog on top keeps the keyboard.
        if window.has_active_dialog(cx) {
            return;
        }
        if let Some(terminal) = self.active_terminal() {
            terminal.read(cx).focus_handle(cx).focus(window, cx);
        }
    }

    /// The numbered tabs and the "+" button. One shell needs no tab, so the
    /// strip is then just the "+".
    pub(crate) fn render_terminal_tabs(&self, cx: &Context<Self>) -> AnyElement {
        let mut strip = h_flex().gap(px(2.)).items_center();
        if self.terminals.len() > 1 {
            for ix in 0..self.terminals.len() {
                strip = strip.child(self.render_terminal_tab(ix, cx));
            }
        }
        if self.can_open_terminal() {
            strip = strip.child(
                header_button("terminal-tab-add", cx)
                    .child("+")
                    .tooltip(|window, cx| {
                        Tooltip::new(format!("New terminal ({NEW_TERMINAL_SHORTCUT})"))
                            .build(window, cx)
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.open_extra_terminal(cx))),
            );
        }
        strip.into_any_element()
    }

    fn render_terminal_tab(&self, ix: usize, cx: &Context<Self>) -> AnyElement {
        let is_active = ix == self.active_terminal;
        let close = div()
            .id(("terminal-tab-close", ix))
            .px(px(2.))
            .rounded(px(2.))
            .text_color(theme::text_dim())
            .hover(|s| s.text_color(theme::red_error()))
            // Mouse-down, not click: it must not reach the tab under it, which
            // would activate the tab being closed.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.close_terminal(ix, cx);
                }),
            )
            .child("\u{00D7}");

        header_button(("terminal-tab", ix), cx)
            .gap(px(3.))
            .when(is_active, |el| {
                el.bg(theme::element_active())
                    .text_color(cx.theme().foreground)
                    .font_semibold()
            })
            .on_click(cx.listener(move |this, _, _, cx| this.activate_terminal(ix, cx)))
            .child(SharedString::from((ix + 1).to_string()))
            .child(close)
            .into_any_element()
    }

    pub(crate) fn render_sftp_toggle(&self, cx: &Context<Self>) -> AnyElement {
        let label = if self.is_sftp_hidden {
            "Show files"
        } else {
            "Hide files"
        };
        header_button("sftp-toggle", cx)
            .child(label)
            .tooltip(|window, cx| {
                Tooltip::new(format!(
                    "Show or hide the file panes ({TOGGLE_FILES_SHORTCUT})"
                ))
                .build(window, cx)
            })
            .on_click(cx.listener(|this, _, _, cx| this.toggle_sftp_pane(cx)))
            .into_any_element()
    }
}

/// A small flat button sized for the 22px terminal header.
fn header_button(
    id: impl Into<gpui::ElementId>,
    cx: &Context<ServerSession>,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .h(px(17.))
        .px(px(6.))
        .rounded(px(3.))
        .text_size(px(10.5))
        .text_color(cx.theme().muted_foreground)
        .cursor_pointer()
        .hover(|s| s.bg(theme::element_hover()))
        .active(|s| s.bg(theme::element_active()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_a_tab_before_the_active_one_keeps_the_same_shell_active() {
        // Tabs 0 1 [2] 3, close 0: the active shell is now at index 1.
        assert_eq!(active_after_close(2, 0, 3), 1);
    }

    #[test]
    fn closing_a_tab_after_the_active_one_changes_nothing() {
        assert_eq!(active_after_close(1, 3, 3), 1);
    }

    #[test]
    fn closing_the_active_tab_moves_to_the_one_that_took_its_place() {
        assert_eq!(active_after_close(1, 1, 3), 1);
    }

    #[test]
    fn closing_the_last_active_tab_moves_to_the_new_last_one() {
        assert_eq!(active_after_close(3, 3, 3), 2);
        assert_eq!(active_after_close(1, 1, 1), 0);
    }
}
