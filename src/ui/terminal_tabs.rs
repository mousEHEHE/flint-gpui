//! Terminal tabs: several shells for one host over the same SSH connection,
//! so a second shell needs no second login. Also the session header with the
//! switches for the file panes and for where the terminal sits.
//!
//! The methods belong to `ServerSession`. They live here so that
//! `server_session.rs` stays about the connection lifecycle.

use gpui::prelude::*;
use gpui::{
    div, px, AnyElement, App, Context, Entity, Focusable as _, MouseButton, SharedString, Window,
};
use gpui_component::{
    h_flex, tooltip::Tooltip, ActiveTheme as _, Icon, IconName, StyledExt as _, WindowExt as _,
};

use crate::connection::Connection;
use crate::preferences::{Preferences, TerminalPosition};
use crate::shell::ShellHandle;
use crate::theme;
use crate::ui::server_session::{ServerSession, ServerSessionEvent};
use crate::ui::shortcut_hints;
use crate::ui::terminal_view::{TerminalEvent, TerminalView};

/// Also where the task and forward popovers start, just under it.
pub(crate) const SESSION_HEADER_HEIGHT: f32 = 46.;

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

/// Why a file cannot open in a new tab: every tab is taken, or the session is
/// not connected.
fn full_or_offline_status(terminal_count: usize, name: &str) -> String {
    if terminal_count >= MAX_TERMINALS {
        format!("Close a terminal tab to open {name}")
    } else {
        format!("Can't open {name}: not connected")
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

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
        self.open_terminal(None, cx);
    }

    /// Edit a remote file with the editor from the preferences, in a new tab.
    /// Never in a tab that is open already: its shell may be busy, and the
    /// keys would go to whatever runs there.
    pub(crate) fn edit_in_terminal(&mut self, path: String, cx: &mut Context<Self>) {
        let name = file_name(&path).to_string();
        if !self.can_open_terminal() {
            cx.emit(ServerSessionEvent::StatusChanged(full_or_offline_status(
                self.terminals.len(),
                &name,
            )));
            return;
        }
        let command = Preferences::editor_command(cx);
        let Some(input) = crate::shell::editor_input(&command, &path) else {
            cx.emit(ServerSessionEvent::StatusChanged(format!(
                "Can't open {name} in the terminal: the name or the editor command has a \
                 control character"
            )));
            return;
        };
        tracing::info!(
            host = %self.conn_info.display_name(),
            path,
            command,
            "editing a file in the terminal",
        );
        self.open_terminal(Some((input, path)), cx);
    }

    /// `startup` is typed into the new shell, and its path names the tab.
    fn open_terminal(&mut self, startup: Option<(Vec<u8>, String)>, cx: &mut Context<Self>) {
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
                Ok(_) if !session.can_open_terminal() => {
                    if let Some((_, path)) = startup {
                        cx.emit(ServerSessionEvent::StatusChanged(full_or_offline_status(
                            session.terminals.len(),
                            file_name(&path),
                        )));
                    }
                }
                Ok(shell) => {
                    // The shell is not ready to read yet. The keys wait in the
                    // terminal's input buffer until it is.
                    let edited_file = startup.map(|(input, path)| {
                        let shell = shell.clone();
                        cx.background_spawn(async move { shell.send_input(input).await })
                            .detach();
                        path
                    });
                    session.add_terminal(shell, cx);
                    if let Some(terminal) = session.terminals.last() {
                        terminal.update(cx, |view, _| view.edited_file = edited_file);
                    }
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
                         The host may limit sessions per connection."
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
        // The editor may have saved the file, so its size and date changed.
        if self.terminals[ix].read(cx).edited_file.is_some() {
            if let Some(browser) = &self.browser {
                browser.update(cx, |browser, cx| browser.list_session_pane_again(cx));
            }
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

    /// Show or hide the second file pane, the one with this computer or
    /// another server. The choice is saved and applies to every session.
    pub fn toggle_second_pane(&mut self, cx: &mut Context<Self>) {
        let is_shown = !self.is_second_pane_visible(cx);
        Preferences::update(cx, |prefs| prefs.is_second_pane_shown = is_shown);
        if is_shown {
            self.is_sftp_hidden = false;
        }
        // Clicking the header took the keys from the terminal.
        self.is_terminal_focus_pending = true;
        self.repaint_files(cx);
    }

    /// With all the file panes hidden the second one is hidden too, whatever
    /// the saved choice.
    fn is_second_pane_visible(&self, cx: &App) -> bool {
        Preferences::is_second_pane_shown(cx) && !self.is_sftp_hidden
    }

    /// Put the terminal under the files or next to them. Saved, like the
    /// second pane.
    pub fn set_terminal_position(&mut self, position: TerminalPosition, cx: &mut Context<Self>) {
        if Preferences::terminal_position(cx) == position {
            return;
        }
        Preferences::update(cx, |prefs| prefs.terminal_position = position);
        // Clicking the header took the keys from the terminal.
        self.is_terminal_focus_pending = true;
        self.repaint_files(cx);
    }

    /// The browser lays its panes out from the preferences, which notify
    /// nobody, so it is told to draw again.
    pub(crate) fn repaint_files(&mut self, cx: &mut Context<Self>) {
        if let Some(browser) = &self.browser {
            browser.update(cx, |_, cx| cx.notify());
        }
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

    /// The bar over a connected session: which server this is, what runs in
    /// the background, and how the files and the terminal are laid out.
    pub(crate) fn render_session_header(&self, cx: &Context<Self>) -> AnyElement {
        let address = session_address(&self.conn_info);
        h_flex()
            .h(px(SESSION_HEADER_HEIGHT))
            .flex_none()
            .pl(px(16.))
            .pr(px(8.))
            .gap(px(10.))
            .items_center()
            .border_b_1()
            .border_color(theme::border())
            // On a narrow window the address goes first, then the name and the
            // running tasks; the switches on the right always stay whole.
            .child(
                div()
                    .flex_shrink(1.)
                    .min_w(px(48.))
                    .truncate()
                    .text_size(px(14.))
                    .font_semibold()
                    .text_color(cx.theme().foreground)
                    .child(self.conn_info.display_name().to_string()),
            )
            .child(
                div()
                    .flex_shrink(4.)
                    .min_w_0()
                    .truncate()
                    .font_family(theme::MONO_FONT)
                    .text_size(px(12.))
                    .text_color(theme::text_dim())
                    .child(address),
            )
            .child(div().flex_1())
            .child(
                h_flex()
                    .flex_shrink(1.)
                    .min_w_0()
                    .overflow_hidden()
                    .gap(px(4.))
                    .items_center()
                    .when_some(self.render_forward_chip(cx), |el, chip| el.child(chip))
                    .when_some(self.render_task_pills(cx), |el, pills| el.child(pills)),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap(px(4.))
                    .items_center()
                    .child(self.render_second_pane_toggle(cx))
                    .child(self.render_sftp_toggle(cx))
                    .child(div().mx(px(6.)).w(px(1.)).h(px(16.)).bg(theme::border()))
                    .child(self.render_layout_switch(cx)),
            )
            .into_any_element()
    }

    /// One "Terminal" tab, or numbered tabs once there are several shells,
    /// and the "+" button.
    pub(crate) fn render_terminal_tabs(&self, cx: &Context<Self>) -> AnyElement {
        // Scrolls sideways when the terminal sits narrow next to the files.
        let mut strip = h_flex()
            .id("terminal-tab-strip")
            .min_w_0()
            .overflow_x_scroll()
            .gap(px(2.))
            .items_center();
        if self.terminals.len() > 1 {
            strip = strip.child(terminal_icon().ml(px(4.)).mr(px(2.)));
            for ix in 0..self.terminals.len() {
                strip = strip.child(self.render_terminal_tab(ix, cx));
            }
        } else {
            let edited_file = self
                .active_terminal()
                .and_then(|terminal| terminal.read(cx).edited_file.clone());
            let label = match &edited_file {
                Some(path) => file_name(path).to_string(),
                None => "Terminal".to_string(),
            };
            strip = strip.child(
                h_flex()
                    .id("terminal-single-tab")
                    .h(px(26.))
                    .px(px(9.))
                    .gap(px(6.))
                    .items_center()
                    .rounded(px(6.))
                    .bg(theme::element())
                    .text_size(px(12.5))
                    .text_color(cx.theme().foreground)
                    .child(terminal_icon())
                    .child(div().max_w(px(140.)).truncate().child(label))
                    .when_some(edited_file, |el, path| {
                        el.tooltip(move |window, cx| Tooltip::new(path.clone()).build(window, cx))
                    }),
            );
        }
        if self.can_open_terminal() {
            strip = strip.child(
                header_button("terminal-tab-add", cx)
                    // The strip scrolls and would clip a corner badge, so
                    // the key takes the place of the "+" instead.
                    .map(|el| {
                        if shortcut_hints::is_shown(cx) {
                            el.child(shortcut_hints::badge(shortcut_hints::NEW_TERMINAL))
                        } else {
                            el.child(Icon::new(IconName::Plus).size(px(12.)))
                        }
                    })
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
        let fg = cx.theme().foreground;
        let close = div()
            .id(("terminal-tab-close", ix))
            .px(px(2.))
            .rounded(px(3.))
            .text_color(theme::text_dim())
            .hover(move |s| s.bg(theme::element_hover()).text_color(fg))
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

        let edited_file = self
            .terminals
            .get(ix)
            .and_then(|terminal| terminal.read(cx).edited_file.clone());
        let label = match &edited_file {
            Some(path) => format!("{} {}", ix + 1, file_name(path)),
            None => (ix + 1).to_string(),
        };

        header_button(("terminal-tab", ix), cx)
            .gap(px(4.))
            .when_some(edited_file, |el, path| {
                el.tooltip(move |window, cx| Tooltip::new(path.clone()).build(window, cx))
            })
            .when(is_active, |el| {
                el.bg(theme::element())
                    .text_color(cx.theme().foreground)
                    .font_semibold()
            })
            .on_click(cx.listener(move |this, _, _, cx| this.activate_terminal(ix, cx)))
            .child(div().max_w(px(140.)).truncate().child(label))
            .child(close)
            .into_any_element()
    }

    pub(crate) fn render_sftp_toggle(&self, cx: &Context<Self>) -> AnyElement {
        toggle_button("sftp-toggle", !self.is_sftp_hidden, cx)
            .relative()
            .child(Icon::empty().path("icons/folder-open.svg").size(px(14.)))
            .child("Files")
            .when(shortcut_hints::is_shown(cx), |el| {
                el.child(shortcut_hints::corner_badge(shortcut_hints::FILES))
            })
            .tooltip(|window, cx| {
                Tooltip::new(format!(
                    "Show or hide the file panes ({TOGGLE_FILES_SHORTCUT})"
                ))
                .build(window, cx)
            })
            .on_click(cx.listener(|this, _, _, cx| this.toggle_sftp_pane(cx)))
            .into_any_element()
    }

    /// Named after what the second pane shows: "Local" or the other server.
    pub(crate) fn render_second_pane_toggle(&self, cx: &Context<Self>) -> AnyElement {
        let is_shown = self.is_second_pane_visible(cx);
        let peer_name = self
            .browser
            .as_ref()
            .and_then(|browser| browser.read(cx).left_source_name());
        let (icon, label): (&'static str, SharedString) = match peer_name {
            Some(name) => ("icons/server.svg", name.into()),
            None if cfg!(target_os = "macos") => ("icons/laptop.svg", "Local".into()),
            None => ("icons/monitor.svg", "Local".into()),
        };
        toggle_button("second-pane-toggle", is_shown, cx)
            .child(Icon::empty().path(icon).size(px(14.)))
            .child(div().max_w(px(140.)).truncate().child(label))
            .tooltip(|window, cx| {
                Tooltip::new("Show or hide the second file pane").build(window, cx)
            })
            .on_click(cx.listener(|this, _, _, cx| this.toggle_second_pane(cx)))
            .into_any_element()
    }

    /// Two icons: terminal under the files, or terminal next to them.
    pub(crate) fn render_layout_switch(&self, cx: &Context<Self>) -> AnyElement {
        let position = Preferences::terminal_position(cx);
        let option = |id: &'static str, value: TerminalPosition, icon: &'static str, tip| {
            let is_active = position == value;
            div()
                .id(id)
                .size(px(24.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.))
                .cursor_pointer()
                .text_color(if is_active {
                    cx.theme().foreground
                } else {
                    theme::text_dim()
                })
                .when(is_active, |el| el.bg(theme::element()))
                .when(!is_active, |el| el.hover(|s| s.bg(theme::field())))
                .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
                .child(Icon::empty().path(icon).size(px(15.)))
                .on_click(cx.listener(move |this, _, _, cx| this.set_terminal_position(value, cx)))
        };
        h_flex()
            .p(px(2.))
            .gap(px(2.))
            .rounded(px(7.))
            .bg(theme::field())
            .child(option(
                "layout-terminal-below",
                TerminalPosition::Below,
                "icons/panel-bottom.svg",
                "Terminal below the files",
            ))
            .child(option(
                "layout-terminal-beside",
                TerminalPosition::Beside,
                "icons/panel-left.svg",
                "Terminal beside the files",
            ))
            .into_any_element()
    }
}

/// `user@host`, with the port only when it is not the usual one.
fn session_address(connection: &Connection) -> String {
    if connection.port == 22 {
        format!("{}@{}", connection.username, connection.host)
    } else {
        format!(
            "{}@{}:{}",
            connection.username, connection.host, connection.port
        )
    }
}

fn terminal_icon() -> Icon {
    Icon::empty()
        .path("icons/square-terminal.svg")
        .size(px(14.))
        .text_color(theme::status_online())
}

/// A small flat button sized for the session and terminal headers.
pub(crate) fn header_button(
    id: impl Into<gpui::ElementId>,
    cx: &Context<ServerSession>,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .h(px(26.))
        .px(px(8.))
        .rounded(px(6.))
        .text_size(px(12.5))
        .text_color(cx.theme().muted_foreground)
        .cursor_pointer()
        .hover(|s| s.bg(theme::field()))
        .active(|s| s.bg(theme::element_active()))
}

/// A header button that stays pressed while what it shows is on screen.
fn toggle_button(
    id: &'static str,
    is_on: bool,
    cx: &Context<ServerSession>,
) -> gpui::Stateful<gpui::Div> {
    header_button(id, cx).gap(px(6.)).when(is_on, |el| {
        el.bg(theme::element()).text_color(cx.theme().foreground)
    })
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
