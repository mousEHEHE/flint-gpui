//! Terminal tabs: several shells for one host over the same SSH connection,
//! so a second shell needs no second login. Also the session header with the
//! switches for the file panes and for where the terminal sits.
//!
//! The methods belong to `ServerSession`. They live here so that
//! `server_session.rs` stays about the connection lifecycle.

use gpui::prelude::*;
use gpui::{
    div, px, AnyElement, App, ClickEvent, Context, Entity, Focusable as _, MouseButton,
    SharedString, Window,
};
use gpui_component::{
    h_flex,
    input::{Input, InputEvent, InputState},
    resizable::{h_resizable, resizable_panel},
    tooltip::Tooltip,
    v_flex, ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, WindowExt as _,
};

use crate::connection::Connection;
use crate::preferences::{Preferences, TerminalPosition};
use crate::shell::ShellHandle;
use crate::theme;
use crate::ui::keymap;
use crate::ui::rems_from_px;
use crate::ui::server_session::{ServerSession, ServerSessionEvent};
use crate::ui::shortcut_hints;
use crate::ui::terminal_view::{TerminalEvent, TerminalView};

/// Also where the task and forward popovers start, just under it.
pub(crate) const SESSION_HEADER_HEIGHT: f32 = 46.;

/// Tabs past this would not fit the header. It is also close to what a server
/// allows: OpenSSH's default `MaxSessions` is 10 and SFTP takes two of them.
const MAX_TERMINALS: usize = 8;

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

/// Two terminals side by side: the tab indices of the left and right panes.
pub(crate) type SplitPanes = Option<(usize, usize)>;

/// The panes after the tab at `closed` is removed. A pane's own tab closing
/// ends the split; the tabs after it move down one.
fn split_after_close(split: SplitPanes, closed: usize) -> SplitPanes {
    let (left, right) = split?;
    if closed == left || closed == right {
        return None;
    }
    let shift = |ix: usize| if ix > closed { ix - 1 } else { ix };
    Some((shift(left), shift(right)))
}

/// Where each tab lands after a reconnect brought `shells` fresh server
/// shells: local tabs stay, server tabs take the shells in order, and a
/// server tab left without one goes (`None`).
fn tabs_after_reconnect(is_local: &[bool], shells: usize) -> Vec<Option<usize>> {
    let mut kept = 0;
    let mut used = 0;
    is_local
        .iter()
        .map(|&is_local| {
            if !is_local {
                if used == shells {
                    return None;
                }
                used += 1;
            }
            kept += 1;
            Some(kept - 1)
        })
        .collect()
}

/// The panes after tab `target` is picked while `active` has the keys. A tab
/// already on screen only gets the keys; any other takes the active pane.
fn split_after_activate(split: SplitPanes, active: usize, target: usize) -> SplitPanes {
    let (left, right) = split?;
    if target == left || target == right {
        return split;
    }
    if active == right {
        Some((left, target))
    } else {
        Some((target, right))
    }
}

/// A terminal tab being dragged onto a pane. `session` keeps the drop to the
/// strip it came from. The terminal, not its index: a shell that exits
/// mid-drag shifts the tabs after it.
#[derive(Clone)]
pub(crate) struct DraggedTerminalTab {
    session: gpui::EntityId,
    terminal: gpui::EntityId,
}

/// The panes after tab `dragged` is dropped on the left (`on_left`) or right
/// pane. A tab dropped on the other pane swaps sides; any other takes the pane.
fn split_after_drop(split: SplitPanes, dragged: usize, on_left: bool) -> SplitPanes {
    let (left, right) = split?;
    Some(match (on_left, dragged == left, dragged == right) {
        (true, _, true) | (false, true, _) => (right, left),
        (true, _, _) => (dragged, right),
        (false, _, _) => (left, dragged),
    })
}

impl ServerSession {
    pub fn active_terminal(&self) -> Option<&Entity<TerminalView>> {
        self.terminals.get(self.active_terminal)
    }

    /// Wrap a freshly opened shell in a terminal tab at the end of the strip.
    /// `is_local`: the shell runs on this computer, not on the server.
    pub(crate) fn add_terminal(
        &mut self,
        shell: ShellHandle,
        is_local: bool,
        cx: &mut Context<Self>,
    ) {
        // The server's command history is no help in a shell on this computer.
        let history = if is_local {
            Vec::new()
        } else {
            self.shell_history.clone()
        };
        let terminal = cx.new(|cx| {
            let mut view = TerminalView::new(shell, cx);
            view.is_local = is_local;
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

    fn is_server_terminal(&self, ix: usize, cx: &App) -> bool {
        self.terminals
            .get(ix)
            .is_some_and(|terminal| self.is_local || !terminal.read(cx).is_local)
    }

    pub(crate) fn server_terminal_count(&self, cx: &App) -> usize {
        (0..self.terminals.len())
            .filter(|&ix| self.is_server_terminal(ix, cx))
            .count()
    }

    /// Where commands for the server go: the terminal in front, or when that
    /// one runs on this computer, the other pane, then the first server tab.
    pub(crate) fn server_terminal(&self, cx: &App) -> Option<&Entity<TerminalView>> {
        let partner = self.split.map(|(left, right)| {
            if self.active_terminal == left {
                right
            } else {
                left
            }
        });
        std::iter::once(self.active_terminal)
            .chain(partner)
            .chain(0..self.terminals.len())
            .find(|&ix| self.is_server_terminal(ix, cx))
            .and_then(|ix| self.terminals.get(ix))
    }

    /// A reconnect opened fresh server shells: the server tabs take them,
    /// the local ones keep running as they are.
    pub(crate) fn replace_server_shells(
        &mut self,
        shells: Vec<ShellHandle>,
        cx: &mut Context<Self>,
    ) {
        let old = std::mem::take(&mut self.terminals);
        let is_local: Vec<bool> = old
            .iter()
            .map(|terminal| terminal.read(cx).is_local)
            .collect();
        let new_index = tabs_after_reconnect(&is_local, shells.len());
        let mut shells = shells.into_iter();
        for terminal in old {
            if terminal.read(cx).is_local {
                self.terminals.push(terminal);
            } else if let Some(shell) = shells.next() {
                let title = terminal.read(cx).title.clone();
                self.add_terminal(shell, false, cx);
                if let Some(new) = self.terminals.last() {
                    new.update(cx, |view, _| view.title = title);
                }
            }
        }
        // The first connect, or a tab with only local terminals left.
        for shell in shells {
            self.add_terminal(shell, false, cx);
        }
        let moved = |ix: usize| new_index.get(ix).copied().flatten();
        // A shell the server refused to reopen may have been the active one.
        self.active_terminal = moved(self.active_terminal)
            .unwrap_or(self.active_terminal)
            .min(self.terminals.len().saturating_sub(1));
        self.split = self
            .split
            .and_then(|(left, right)| Some((moved(left)?, moved(right)?)));
    }

    fn can_open_terminal(&self) -> bool {
        (self.is_local || self.browser.is_some())
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
        if self.is_local {
            self.open_local_terminal(cx);
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
                Ok(_) if session.connection_generation != generation => {
                    session.is_split_pending = false;
                }
                Ok(_) if !session.can_open_terminal() => {
                    session.is_split_pending = false;
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
                    session.add_terminal(shell, false, cx);
                    if let Some(terminal) = session.terminals.last() {
                        terminal.update(cx, |view, _| view.edited_file = edited_file);
                    }
                    session.show_new_terminal();
                    tracing::info!(
                        host = %session.conn_info.display_name(),
                        count = session.terminals.len(),
                        "extra terminal opened",
                    );
                    cx.notify();
                }
                Err(error) => {
                    session.is_split_pending = false;
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

    fn open_local_terminal(&mut self, cx: &mut Context<Self>) {
        let (rows, cols) = self
            .active_terminal()
            .and_then(|terminal| terminal.read(cx).grid_size())
            .unwrap_or((24, 80));
        match crate::local_shell::open_local_shell(rows, cols, None) {
            Ok(shell) => {
                self.add_terminal(shell, true, cx);
                self.show_new_terminal();
                cx.notify();
            }
            Err(e) => {
                self.is_split_pending = false;
                tracing::warn!(%e, "could not open another local terminal");
                cx.emit(ServerSessionEvent::StatusChanged(format!(
                    "Error: could not open another terminal ({e})."
                )));
            }
        }
    }

    /// The tab of a local session: its terminals and nothing else.
    pub(crate) fn render_local(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(terminal_view) = self.active_terminal().cloned() else {
            return div().size_full().into_any_element();
        };
        v_flex()
            .size_full()
            .overflow_hidden()
            .child(
                h_flex()
                    .h(rems_from_px(38.))
                    .flex_none()
                    .min_w_0()
                    .px(rems_from_px(8.))
                    .items_center()
                    .child(self.render_terminal_tabs(cx)),
            )
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .child(self.render_terminal_area(&terminal_view, cx)),
            )
            .into_any_element()
    }

    /// Show the shell just added at the end: in a new right pane when a split
    /// asked for it, otherwise in the pane with the keys.
    fn show_new_terminal(&mut self) {
        let new = self.terminals.len() - 1;
        if std::mem::take(&mut self.is_split_pending) && new > 0 {
            self.split = Some((self.active_terminal, new));
        } else {
            self.split = split_after_activate(self.split, self.active_terminal, new);
        }
        self.active_terminal = new;
        self.is_terminal_focus_pending = true;
    }

    /// A shell on this computer next to the server's, in the same tab: in a
    /// new right pane, or as one more tab when the split is already there.
    pub fn open_local_beside(&mut self, cx: &mut Context<Self>) {
        if self.is_split_pending || !self.can_open_terminal() {
            return;
        }
        self.is_split_pending = self.split.is_none();
        self.open_local_terminal(cx);
    }

    /// Open a shell beside the current one, or show one terminal again.
    pub fn toggle_split(&mut self, cx: &mut Context<Self>) {
        if self.split.take().is_some() {
            self.is_terminal_focus_pending = true;
            cx.notify();
            return;
        }
        // A held Cmd+D repeats before the first shell arrives.
        if self.is_split_pending || !self.can_open_terminal() {
            return;
        }
        self.is_split_pending = true;
        self.open_terminal(None, cx);
    }

    pub fn swap_panes(&mut self, cx: &mut Context<Self>) {
        if let Some((left, right)) = self.split {
            self.split = Some((right, left));
            cx.notify();
        }
    }

    fn drop_tab_on_pane(
        &mut self,
        tab: &DraggedTerminalTab,
        on_left: bool,
        cx: &mut Context<Self>,
    ) {
        if tab.session != cx.entity_id() {
            return;
        }
        // Gone if its shell exited mid-drag.
        let Some(ix) = self
            .terminals
            .iter()
            .position(|terminal| terminal.entity_id() == tab.terminal)
        else {
            return;
        };
        self.split = split_after_drop(self.split, ix, on_left);
        self.active_terminal = ix;
        self.is_terminal_focus_pending = true;
        cx.notify();
    }

    /// The active terminal, or both panes of a split. A click in a pane gives
    /// it the keys, before the terminal itself sees the click.
    pub(crate) fn render_terminal_area(
        &self,
        active: &Entity<TerminalView>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let panes = self.split.and_then(|(left, right)| {
            Some((
                (left, self.terminals.get(left)?.clone()),
                (right, self.terminals.get(right)?.clone()),
            ))
        });
        let Some((left, right)) = panes else {
            return active.clone().into_any_element();
        };
        let accent = cx.theme().primary;
        let pane = |(ix, terminal): (usize, Entity<TerminalView>), on_left: bool| {
            let is_active = ix == self.active_terminal;
            // An overlay, not the pane itself: the terminal paints over the pane's background.
            let drop_target = div()
                .absolute()
                .inset_0()
                .drag_over::<DraggedTerminalTab>(move |style, _, _, cx| {
                    style
                        .bg(cx.theme().drop_target.opacity(0.5))
                        .border_2()
                        .border_color(accent)
                })
                .on_drop(cx.listener(move |this, tab: &DraggedTerminalTab, _, cx| {
                    this.drop_tab_on_pane(tab, on_left, cx)
                }));
            div()
                .relative()
                .size_full()
                .overflow_hidden()
                .border_t_2()
                .border_color(if is_active {
                    accent
                } else {
                    gpui::transparent_black()
                })
                .capture_any_mouse_down(cx.listener(move |this, _, _, cx| {
                    if this.active_terminal != ix {
                        this.active_terminal = ix;
                        cx.notify();
                    }
                }))
                .child(terminal)
                .when(cx.has_active_drag(), |el| el.child(drop_target))
        };
        h_resizable("terminal-split")
            .with_state(&self.terminal_split_state)
            .child(resizable_panel().child(pane(left, true)))
            .child(resizable_panel().child(pane(right, false)))
            .into_any_element()
    }

    fn activate_terminal(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.terminals.len() {
            return;
        }
        self.split = split_after_activate(self.split, self.active_terminal, ix);
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
        // The other pane of a split takes over when the active one goes.
        let partner = self.split.and_then(|(left, right)| {
            (ix == left)
                .then_some(right)
                .or((ix == right).then_some(left))
        });
        self.split = split_after_close(self.split, ix);
        self.terminals.remove(ix);
        self.active_terminal = match partner {
            Some(partner) if was_active => partner - usize::from(partner > ix),
            _ => active_after_close(self.active_terminal, ix, self.terminals.len()),
        };
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
        // A local tab is only its shells: when the last one ends, so does the tab.
        if self.is_local && self.terminals.len() == 1 {
            cx.emit(ServerSessionEvent::CloseRequested);
            return;
        }
        self.close_terminal(ix, cx);
    }

    /// Hide or show the file panes. Hidden, the terminal fills the tab.
    pub fn toggle_sftp_pane(&mut self, cx: &mut Context<Self>) {
        self.is_sftp_hidden = !self.is_sftp_hidden;
        // A focused file pane may have just left the screen.
        self.is_terminal_focus_pending = true;
        cx.notify();
    }

    /// Saved, and applies to every session, like the second file pane.
    pub fn toggle_server_panel(&mut self, cx: &mut Context<Self>) {
        let is_shown = !Preferences::is_server_panel_shown(cx);
        Preferences::update(cx, |prefs| prefs.is_server_panel_shown = is_shown);
        // Clicking the header took the keys from the terminal.
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

    /// Only the terminal on screen takes snapshots: none while another app tab is in front.
    pub(crate) fn set_in_front(&self, is_front: bool, cx: &App) {
        for (ix, terminal) in self.terminals.iter().enumerate() {
            let is_in_pane = self
                .split
                .is_some_and(|(left, right)| ix == left || ix == right);
            terminal
                .read(cx)
                .set_visible(is_front && (ix == self.active_terminal || is_in_pane));
        }
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
        // Every tab switch asks for the focus, so this is where hidden tabs are told.
        self.set_in_front(true, cx);
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
            .h(rems_from_px(SESSION_HEADER_HEIGHT))
            .flex_none()
            .pl(rems_from_px(16.))
            .pr(rems_from_px(8.))
            .gap(rems_from_px(10.))
            .items_center()
            .border_b_1()
            .border_color(theme::border())
            // On a narrow window the address goes first, then the name and the
            // running tasks; the switches on the right always stay whole.
            .child(
                div()
                    .flex_shrink(1.)
                    .min_w(rems_from_px(48.))
                    .truncate()
                    .text_size(rems_from_px(14.))
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
                    .text_size(rems_from_px(12.))
                    .text_color(theme::text_dim())
                    .child(address),
            )
            .child(div().flex_1())
            .child(
                h_flex()
                    .flex_shrink(1.)
                    .min_w_0()
                    .overflow_hidden()
                    .gap(rems_from_px(4.))
                    .items_center()
                    .when_some(self.render_forward_chip(cx), |el, chip| el.child(chip))
                    .when_some(self.render_task_pills(cx), |el, pills| el.child(pills)),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap(rems_from_px(4.))
                    .items_center()
                    .child(self.render_second_pane_toggle(cx))
                    .child(self.render_sftp_toggle(cx))
                    .child(
                        div()
                            .mx(rems_from_px(6.))
                            .w(px(1.))
                            .h(rems_from_px(16.))
                            .bg(theme::border()),
                    )
                    .child(self.render_layout_switch(cx))
                    .child(
                        div()
                            .mx(rems_from_px(6.))
                            .w(px(1.))
                            .h(rems_from_px(16.))
                            .bg(theme::border()),
                    )
                    .child(self.render_server_panel_toggle(cx)),
            )
            .into_any_element()
    }

    /// The terminal tabs and the "+" button.
    pub(crate) fn render_terminal_tabs(&self, cx: &Context<Self>) -> AnyElement {
        // Scrolls sideways when the terminal sits narrow next to the files.
        let mut strip = h_flex()
            .id("terminal-tab-strip")
            .min_w_0()
            .overflow_x_scroll()
            .gap(rems_from_px(2.))
            .items_center();
        for ix in 0..self.terminals.len() {
            strip = strip.child(self.render_terminal_tab(ix, cx));
        }
        if self.can_open_terminal() {
            strip = strip.child(
                header_button("terminal-tab-add", cx)
                    // The strip scrolls and would clip a corner badge, so
                    // the key takes the place of the "+" instead.
                    .map(|el| {
                        let hint = shortcut_hints::key("tabs.new_terminal", cx);
                        if let Some(hint) = hint.filter(|_| shortcut_hints::is_shown(cx)) {
                            el.child(shortcut_hints::badge(hint))
                        } else {
                            el.child(Icon::new(IconName::Plus).size(rems_from_px(12.)))
                        }
                    })
                    .tooltip(|window, cx| {
                        Tooltip::new(keymap::with_label("New terminal", "tabs.new_terminal", cx))
                            .build(window, cx)
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.open_extra_terminal(cx))),
            );
            // Windows has no local shell yet.
            if !self.is_local && cfg!(unix) {
                strip = strip.child(
                    header_button("terminal-tab-local", cx)
                        .map(|el| {
                            let hint = shortcut_hints::key("tabs.local_beside", cx);
                            if let Some(hint) = hint.filter(|_| shortcut_hints::is_shown(cx)) {
                                el.child(shortcut_hints::badge(hint))
                            } else {
                                el.child(local_icon().size(rems_from_px(13.)))
                            }
                        })
                        .tooltip(|window, cx| {
                            Tooltip::new(keymap::with_label(
                                "Local terminal beside",
                                "tabs.local_beside",
                                cx,
                            ))
                            .build(window, cx)
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.open_local_beside(cx))),
                );
            }
        }
        strip.into_any_element()
    }

    /// The user's name for the tab, else the edited file, else what runs there.
    fn tab_label(&self, ix: usize, cx: &App) -> String {
        let Some(terminal) = self.terminals.get(ix).map(|terminal| terminal.read(cx)) else {
            return String::new();
        };
        if let Some(title) = &terminal.title {
            return title.clone();
        }
        if let Some(path) = &terminal.edited_file {
            return file_name(path).to_string();
        }
        let kind = if terminal.is_local && !self.is_local {
            "Local"
        } else {
            "Terminal"
        };
        if self.terminals.len() > 1 {
            format!("{kind} {}", ix + 1)
        } else {
            kind.to_string()
        }
    }

    fn tab_icon(&self, ix: usize, is_active: bool, cx: &App) -> Icon {
        let Some(terminal) = self.terminals.get(ix).map(|terminal| terminal.read(cx)) else {
            return terminal_icon();
        };
        let icon = if terminal.edited_file.is_some() {
            Icon::empty().path("icons/pen-line.svg")
        } else if terminal.is_local && !self.is_local {
            local_icon()
        } else {
            terminal_icon()
        };
        let color = if is_active {
            theme::status_online()
        } else {
            theme::text_dim()
        };
        icon.size(rems_from_px(13.)).text_color(color)
    }

    fn render_terminal_tab(&self, ix: usize, cx: &Context<Self>) -> AnyElement {
        let is_active = ix == self.active_terminal;
        let is_closable = self.terminals.len() > 1;
        let rename_input = self.tab_rename.as_ref().and_then(|(terminal, input)| {
            (self.terminals.get(ix) == Some(terminal)).then(|| input.clone())
        });
        let label = self.tab_label(ix, cx);
        let group: SharedString = format!("terminal-tab-{ix}").into();

        let tab = h_flex()
            .id(("terminal-tab", ix))
            .group(group.clone())
            .h(rems_from_px(26.))
            .pl(rems_from_px(8.))
            .pr(rems_from_px(if is_closable { 4. } else { 10. }))
            .gap(rems_from_px(6.))
            .items_center()
            .rounded(rems_from_px(6.))
            .text_size(rems_from_px(12.5))
            .child(self.tab_icon(ix, is_active, cx))
            .map(|el| {
                if is_active {
                    el.bg(theme::element()).text_color(cx.theme().foreground)
                } else {
                    el.text_color(cx.theme().muted_foreground)
                        .hover(|s| s.bg(theme::field()))
                }
            });

        let tab = match rename_input {
            Some(input) => tab.child(self.render_tab_rename_field(&input, cx)),
            None => self.with_tab_label(tab, ix, label, cx),
        };
        tab.when(is_closable, |el| {
            el.child(self.render_tab_close(ix, is_active, group, cx))
        })
        .into_any_element()
    }

    fn with_tab_label(
        &self,
        tab: gpui::Stateful<gpui::Div>,
        ix: usize,
        label: String,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let tooltip = self
            .terminals
            .get(ix)
            .and_then(|terminal| terminal.read(cx).edited_file.clone())
            .or_else(|| {
                self.split
                    .map(|_| "Drag onto a pane to move it".to_string())
            })
            .unwrap_or_else(|| "Double-click to rename".to_string());
        let drag_preview = TabDragPreview {
            icon: self.tab_icon(ix, true, cx),
            label: label.clone().into(),
        };
        tab.cursor_pointer()
            .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            .when_some(
                self.terminals.get(ix).filter(|_| self.terminals.len() > 1),
                |el, terminal| {
                    el.on_drag(
                        DraggedTerminalTab {
                            session: cx.entity_id(),
                            terminal: terminal.entity_id(),
                        },
                        move |_, _, _, cx| cx.new(|_| drag_preview.clone()),
                    )
                },
            )
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if event.click_count() >= 2 {
                    this.start_tab_rename(ix, window, cx);
                } else {
                    this.activate_terminal(ix, cx);
                }
            }))
            .child(div().max_w(rems_from_px(160.)).truncate().child(label))
    }

    /// Sits where the label was, in the same font and as wide as the text, so
    /// the tab does not jump when editing starts.
    fn render_tab_rename_field(
        &self,
        input: &Entity<InputState>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        // Cascadia Code is monospace: one character is about 0.6 of the font size.
        // One spare character and the cursor's few pixels: a typed letter must fit
        // before the field widens, or the field scrolls and hides the first letter.
        let chars = input.read(cx).value().chars().count() + 1;
        let width = (chars as f32 * 12.5 * 0.6 + 4.).clamp(30., 160.);
        div()
            .w(rems_from_px(width))
            .capture_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    cx.stop_propagation();
                    this.tab_rename = None;
                    this.is_terminal_focus_pending = true;
                    cx.notify();
                }
            }))
            .child(
                Input::new(input)
                    .xsmall()
                    .appearance(false)
                    .px_0()
                    .text_size(rems_from_px(12.5)),
            )
    }

    /// Shown on the active tab and on hover, so the strip stays calm.
    fn render_tab_close(
        &self,
        ix: usize,
        is_active: bool,
        group: SharedString,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let fg = cx.theme().foreground;
        div()
            .id(("terminal-tab-close", ix))
            .size(rems_from_px(16.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(rems_from_px(4.))
            .text_color(theme::text_dim())
            .when(!is_active, |el| {
                el.invisible().group_hover(group, |s| s.visible())
            })
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
            .child(Icon::empty().path("icons/x.svg").size(rems_from_px(11.)))
    }

    fn start_tab_rename(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(terminal) = self.terminals.get(ix).cloned() else {
            return;
        };
        let label = self.tab_label(ix, cx);
        let input = cx.new(|cx| InputState::new(window, cx));
        input.update(cx, |state, cx| {
            state.set_value(label, window, cx);
            state.select_all(window, cx);
            state.focus(window, cx);
        });
        cx.subscribe(&input, |this, _, event: &InputEvent, cx| match event {
            InputEvent::PressEnter { .. } => {
                this.finish_tab_rename(cx);
                this.is_terminal_focus_pending = true;
            }
            // A click elsewhere keeps the new name, as in Finder.
            InputEvent::Blur => this.finish_tab_rename(cx),
            // The field grows with the text.
            InputEvent::Change => cx.notify(),
            _ => {}
        })
        .detach();
        self.tab_rename = Some((terminal, input));
        cx.notify();
    }

    /// An empty name goes back to the default label.
    fn finish_tab_rename(&mut self, cx: &mut Context<Self>) {
        let Some((terminal, input)) = self.tab_rename.take() else {
            return;
        };
        let name = input.read(cx).value().trim().to_string();
        terminal.update(cx, |view, _| {
            view.title = (!name.is_empty()).then_some(name)
        });
        cx.notify();
    }

    pub(crate) fn render_sftp_toggle(&self, cx: &Context<Self>) -> AnyElement {
        toggle_button("sftp-toggle", !self.is_sftp_hidden, cx)
            .relative()
            .child(
                Icon::empty()
                    .path("icons/folder-open.svg")
                    .size(rems_from_px(14.)),
            )
            .child("Files")
            .when_some(
                shortcut_hints::key("tabs.toggle_files", cx)
                    .filter(|_| shortcut_hints::is_shown(cx)),
                |el, hint| el.child(shortcut_hints::corner_badge(hint)),
            )
            .tooltip(|window, cx| {
                Tooltip::new(keymap::with_label(
                    "Show or hide the file panes",
                    "tabs.toggle_files",
                    cx,
                ))
                .build(window, cx)
            })
            .on_click(cx.listener(|this, _, _, cx| this.toggle_sftp_pane(cx)))
            .into_any_element()
    }

    pub(crate) fn render_server_panel_toggle(&self, cx: &Context<Self>) -> AnyElement {
        toggle_button(
            "server-panel-toggle",
            Preferences::is_server_panel_shown(cx),
            cx,
        )
        .relative()
        .px(rems_from_px(5.))
        .child(
            Icon::empty()
                .path("icons/panel-right.svg")
                .size(rems_from_px(15.)),
        )
        .when_some(
            shortcut_hints::key("tabs.server_panel", cx).filter(|_| shortcut_hints::is_shown(cx)),
            |el, hint| el.child(shortcut_hints::corner_badge(hint)),
        )
        .tooltip(|window, cx| {
            Tooltip::new(keymap::with_label("Server panel", "tabs.server_panel", cx))
                .build(window, cx)
        })
        .on_click(cx.listener(|this, _, _, cx| this.toggle_server_panel(cx)))
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
            .child(Icon::empty().path(icon).size(rems_from_px(14.)))
            .child(div().max_w(rems_from_px(140.)).truncate().child(label))
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
                .size(rems_from_px(24.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(rems_from_px(5.))
                .cursor_pointer()
                .text_color(if is_active {
                    cx.theme().foreground
                } else {
                    theme::text_dim()
                })
                .when(is_active, |el| el.bg(theme::element()))
                .when(!is_active, |el| el.hover(|s| s.bg(theme::field())))
                .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
                .child(Icon::empty().path(icon).size(rems_from_px(15.)))
                .on_click(cx.listener(move |this, _, _, cx| this.set_terminal_position(value, cx)))
        };
        h_flex()
            .p(rems_from_px(2.))
            .gap(rems_from_px(2.))
            .rounded(rems_from_px(7.))
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

/// A dragged tab looks like the active tab, lifted off the strip.
#[derive(Clone)]
struct TabDragPreview {
    icon: Icon,
    label: SharedString,
}

impl Render for TabDragPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Drawn outside the app's root, so the font is not inherited.
        h_flex()
            .h(rems_from_px(26.))
            .pl(rems_from_px(8.))
            .pr(rems_from_px(10.))
            .gap(rems_from_px(6.))
            .items_center()
            .rounded(rems_from_px(6.))
            .bg(theme::element())
            .border_1()
            .border_color(theme::border())
            .shadow_md()
            .font_family(theme::UI_FONT)
            .text_size(rems_from_px(12.5))
            .text_color(cx.theme().foreground)
            .child(self.icon.clone())
            .child(
                div()
                    .max_w(rems_from_px(160.))
                    .truncate()
                    .child(self.label.clone()),
            )
    }
}

fn session_address(connection: &Connection) -> String {
    crate::ui::ssh_address(&connection.username, &connection.host, connection.port)
}

/// This computer, as the file panes show it.
fn local_icon() -> Icon {
    Icon::empty().path(if cfg!(target_os = "macos") {
        "icons/laptop.svg"
    } else {
        "icons/monitor.svg"
    })
}

fn terminal_icon() -> Icon {
    Icon::empty()
        .path("icons/square-terminal.svg")
        .size(rems_from_px(14.))
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
        .h(rems_from_px(26.))
        .px(rems_from_px(8.))
        .rounded(rems_from_px(6.))
        .text_size(rems_from_px(12.5))
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
    header_button(id, cx)
        .gap(rems_from_px(6.))
        .when(is_on, |el| {
            el.bg(theme::element()).text_color(cx.theme().foreground)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reconnect_keeps_local_tabs_and_drops_server_tabs_without_a_shell() {
        // server, local, server, server: the server reopened two shells.
        assert_eq!(
            tabs_after_reconnect(&[false, true, false, false], 2),
            [Some(0), Some(1), Some(2), None]
        );
        // Only one shell came back: the second server tab goes.
        assert_eq!(
            tabs_after_reconnect(&[false, false, true], 1),
            [Some(0), None, Some(1)]
        );
        // The first connect has no tabs yet.
        assert_eq!(tabs_after_reconnect(&[], 1), []);
    }

    #[test]
    fn a_pane_closing_ends_the_split_and_other_tabs_move_down() {
        assert_eq!(split_after_close(None, 0), None);
        // Tabs [0 | 2] 1 3: closing a pane's tab ends the split.
        assert_eq!(split_after_close(Some((0, 2)), 2), None);
        assert_eq!(split_after_close(Some((0, 2)), 0), None);
        // Closing a tab between them moves the right pane's tab down.
        assert_eq!(split_after_close(Some((0, 2)), 1), Some((0, 1)));
        // Closing one before both moves both; one after both changes nothing.
        assert_eq!(split_after_close(Some((2, 3)), 0), Some((1, 2)));
        assert_eq!(split_after_close(Some((0, 1)), 3), Some((0, 1)));
    }

    #[test]
    fn a_picked_tab_goes_into_the_pane_with_the_keys() {
        assert_eq!(split_after_activate(None, 0, 2), None);
        // Already on screen: the panes stay as they are.
        assert_eq!(split_after_activate(Some((0, 1)), 0, 1), Some((0, 1)));
        // The right pane has the keys, so tab 3 replaces its shell.
        assert_eq!(split_after_activate(Some((0, 1)), 1, 3), Some((0, 3)));
        assert_eq!(split_after_activate(Some((0, 1)), 0, 3), Some((3, 1)));
    }

    #[test]
    fn a_tab_dropped_on_a_pane_swaps_or_takes_it() {
        assert_eq!(split_after_drop(None, 1, true), None);
        // The right pane's tab dropped on the left: the panes swap.
        assert_eq!(split_after_drop(Some((0, 1)), 1, true), Some((1, 0)));
        assert_eq!(split_after_drop(Some((0, 1)), 0, false), Some((1, 0)));
        assert_eq!(split_after_drop(Some((0, 1)), 0, true), Some((0, 1)));
        assert_eq!(split_after_drop(Some((0, 1)), 3, true), Some((3, 1)));
        assert_eq!(split_after_drop(Some((0, 1)), 3, false), Some((0, 3)));
    }

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
