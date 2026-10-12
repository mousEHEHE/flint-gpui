use async_channel::{Receiver, Sender};
use russh::Channel;
use russh::ChannelMsg;
use tokio::sync::oneshot;

use crate::term_emulator::TermEmulator;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MouseModeFlags {
    pub report_click: bool,
    pub report_drag: bool,
    pub report_motion: bool,
    pub sgr: bool,
}

impl MouseModeFlags {
    pub fn any_active(&self) -> bool {
        self.report_click || self.report_drag || self.report_motion
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SelectionType {
    Simple,
    Semantic,
    Lines,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SelectionSide {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridPoint {
    pub line: i32,
    pub column: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridSelectionRange {
    pub start: GridPoint,
    pub end: GridPoint,
}

/// Where a scrollback key moves the viewport.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScrollTarget {
    PageUp,
    PageDown,
    Top,
    Bottom,
}

#[derive(Clone)]
pub struct ShellHandle {
    cmd_tx: Sender<ShellCmd>,
    event_rx: Receiver<ShellEvent>,
}

impl ShellHandle {
    pub(crate) fn new(cmd_tx: Sender<ShellCmd>, event_rx: Receiver<ShellEvent>) -> Self {
        Self { cmd_tx, event_rx }
    }

    pub async fn send_input(&self, data: Vec<u8>) {
        let _ = self.cmd_tx.send(ShellCmd::Input(data)).await;
    }

    pub async fn resize(&self, rows: u16, cols: u16) {
        let _ = self.cmd_tx.send(ShellCmd::Resize { rows, cols }).await;
    }

    pub fn subscribe(&self) -> Receiver<ShellEvent> {
        self.event_rx.clone()
    }

    pub async fn set_selection(
        &self,
        ty: SelectionType,
        anchor: GridPoint,
        anchor_side: SelectionSide,
        head: GridPoint,
        head_side: SelectionSide,
    ) -> Option<GridSelectionRange> {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .cmd_tx
            .send(ShellCmd::SetSelection {
                ty,
                anchor,
                anchor_side,
                head,
                head_side,
                reply: tx,
            })
            .await;
        rx.await.ok().flatten()
    }

    /// Extend the head of the existing selection, preserving (and keeping pinned)
    /// its anchor. Unlike `set_selection`, this does not reset the anchor, so a
    /// drag stays correct even while the buffer scrolls underneath it.
    pub async fn update_selection_head(
        &self,
        head: GridPoint,
        head_side: SelectionSide,
    ) -> Option<GridSelectionRange> {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .cmd_tx
            .send(ShellCmd::UpdateSelectionHead {
                head,
                head_side,
                reply: tx,
            })
            .await;
        rx.await.ok().flatten()
    }

    pub async fn extract_selection_text(&self) -> Option<String> {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .cmd_tx
            .send(ShellCmd::ExtractSelectionText { reply: tx })
            .await;
        rx.await.ok().flatten()
    }

    pub async fn select_all(&self) -> Option<GridSelectionRange> {
        let (tx, rx) = oneshot::channel();
        let _ = self.cmd_tx.send(ShellCmd::SelectAll { reply: tx }).await;
        rx.await.ok().flatten()
    }

    /// Set the terminal search query; an empty query clears it. Returns how many
    /// matches the whole buffer holds, or `None` once the session is gone and
    /// the search can no longer run: a closed session must not be reported as
    /// "no matches" while its scrollback is still on screen.
    pub async fn set_search(&self, query: String) -> Option<usize> {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .cmd_tx
            .send(ShellCmd::SetSearch { query, reply: tx })
            .await;
        rx.await.ok()
    }

    /// Step to the next (or previous) match and scroll it into view. Returns the
    /// 1-based position of that match and the total, `(0, 0)` when nothing
    /// matches, or `None` when the session is gone.
    pub async fn step_search(&self, forward: bool) -> Option<(usize, usize)> {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .cmd_tx
            .send(ShellCmd::StepSearch { forward, reply: tx })
            .await;
        rx.await.ok()
    }

    pub async fn scroll_delta(&self, lines: i32) {
        let _ = self.cmd_tx.send(ShellCmd::ScrollDelta(lines)).await;
    }

    /// Scroll so the viewport sits `offset` lines above the bottom (0 = live tail,
    /// `history_size` = fully scrolled up). Used by the draggable scrollbar; the
    /// delta is resolved against the emulator's own current offset so rapid drags
    /// can't accumulate drift.
    pub async fn scroll_to_offset(&self, offset: usize) {
        let _ = self.cmd_tx.send(ShellCmd::ScrollToOffset(offset)).await;
    }

    pub async fn scroll_to(&self, target: ScrollTarget) {
        let _ = self.cmd_tx.send(ShellCmd::ScrollTo(target)).await;
    }

    /// Wipe the scrollback and the screen. The shell is not told, so follow it
    /// with a Ctrl-L to make it draw its prompt again.
    pub async fn clear_scrollback(&self) {
        let _ = self.cmd_tx.send(ShellCmd::ClearScrollback).await;
    }
}

pub enum ShellCmd {
    Input(Vec<u8>),
    Resize {
        rows: u16,
        cols: u16,
    },
    SetSelection {
        ty: SelectionType,
        anchor: GridPoint,
        anchor_side: SelectionSide,
        head: GridPoint,
        head_side: SelectionSide,
        reply: oneshot::Sender<Option<GridSelectionRange>>,
    },
    UpdateSelectionHead {
        head: GridPoint,
        head_side: SelectionSide,
        reply: oneshot::Sender<Option<GridSelectionRange>>,
    },
    ExtractSelectionText {
        reply: oneshot::Sender<Option<String>>,
    },
    SelectAll {
        reply: oneshot::Sender<Option<GridSelectionRange>>,
    },
    SetSearch {
        query: String,
        reply: oneshot::Sender<usize>,
    },
    StepSearch {
        forward: bool,
        reply: oneshot::Sender<(usize, usize)>,
    },
    ScrollDelta(i32),
    ScrollToOffset(usize),
    ScrollTo(ScrollTarget),
    ClearScrollback,
}

pub enum ShellEvent {
    Snapshot(TermSnapshot),
    /// The remote shell ended by itself (`exit`, Ctrl+D) or the server closed
    /// its channel. The connection may still be fine.
    Exited,
    /// The channel went away with the connection.
    Disconnected,
    Error(String),
}

pub struct TermSnapshot {
    pub rows: u16,
    pub cols: u16,
    pub cells: Vec<Vec<SnapshotCell>>,
    /// One flag per row: true when the row's text runs on into the next row
    /// (a soft wrap), so the two rows are one line of output.
    pub is_row_wrapped: Vec<bool>,
    pub cursor: (u16, u16),
    pub cursor_visible: bool,
    /// True while an app holds the alternate screen (vim, htop, less). Used to
    /// suppress command suggestions, which only make sense at a shell prompt.
    pub alt_screen: bool,
    /// True while the app has bracketed paste on (DECSET 2004). Pasted text then
    /// has to be wrapped in ESC[200~ and ESC[201~ so the app takes it as data and
    /// not as typing. Otherwise every newline runs a command.
    pub bracketed_paste: bool,
    /// True while the app wants cursor keys in application form (DECCKM):
    /// arrows are then ESC O A instead of ESC [ A.
    pub is_app_cursor: bool,
    /// True while the wheel should act as arrow keys on the alternate screen
    /// (DECSET 1007). On by default; an app can turn it off.
    pub is_alternate_scroll: bool,
    pub mouse_mode: MouseModeFlags,
    pub display_offset: usize,
    pub history_size: usize,
    /// Current selection range, already rotated by the emulator to stay pinned to
    /// its content as new output scrolls the buffer. `None` when there is no
    /// selection (or it has collapsed to empty).
    pub selection: Option<GridSelectionRange>,
    /// Search matches on the visible rows, in the same absolute coordinates as
    /// `selection`. Empty when no search is running.
    pub search_matches: Vec<GridSelectionRange>,
    /// The match the user stepped to, painted stronger than the rest.
    pub current_match: Option<GridSelectionRange>,
}

pub struct SnapshotCell {
    pub c: char,
    pub fg: SnapshotColor,
    pub bg: SnapshotColor,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub dim: bool,
    pub inverse: bool,
    pub wide_char: bool,
    pub wide_char_spacer: bool,
}

impl Default for SnapshotCell {
    fn default() -> Self {
        Self {
            c: ' ',
            fg: SnapshotColor::Default,
            bg: SnapshotColor::Default,
            bold: false,
            italic: false,
            underline: false,
            strikethrough: false,
            dim: false,
            inverse: false,
            wide_char: false,
            wide_char_spacer: false,
        }
    }
}

pub enum SnapshotColor {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

pub async fn handle_shell_commands(
    mut channel: Channel<russh::client::Msg>,
    initial_rows: u16,
    initial_cols: u16,
    cmd_rx: Receiver<ShellCmd>,
    event_tx: Sender<ShellEvent>,
) {
    let mut emulator = TermEmulator::new(initial_rows, initial_cols);

    // Send initial empty snapshot so the UI has something to render immediately.
    let _ = event_tx
        .send(ShellEvent::Snapshot(emulator.snapshot()))
        .await;

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                match cmd {
                    Ok(ShellCmd::Input(data)) => {
                        emulator.scroll_to_bottom();
                        if let Err(e) = channel.data(data.as_slice()).await {
                            let _ = event_tx.send(ShellEvent::Error(format!("send input: {e}"))).await;
                            break;
                        }
                    }
                    Ok(ShellCmd::Resize { rows, cols }) => {
                        if let Err(e) = channel.window_change(cols as u32, rows as u32, 0, 0).await {
                            let _ = event_tx.send(ShellEvent::Error(format!("resize: {e}"))).await;
                            break;
                        }
                        emulator.resize(rows, cols);
                        let _ = event_tx.send(ShellEvent::Snapshot(emulator.snapshot())).await;
                    }
                    Ok(ShellCmd::SetSelection { ty, anchor, anchor_side, head, head_side, reply }) => {
                        let range = emulator.set_selection(ty, anchor, anchor_side, head, head_side);
                        let _ = reply.send(range);
                    }
                    Ok(ShellCmd::UpdateSelectionHead { head, head_side, reply }) => {
                        let range = emulator.update_selection_head(head, head_side);
                        let _ = reply.send(range);
                    }
                    Ok(ShellCmd::ExtractSelectionText { reply }) => {
                        let _ = reply.send(emulator.selection_to_string());
                    }
                    Ok(ShellCmd::SelectAll { reply }) => {
                        let range = emulator.select_all();
                        let _ = reply.send(range);
                    }
                    Ok(ShellCmd::SetSearch { query, reply }) => {
                        let total = emulator.set_search(query);
                        let _ = reply.send(total);
                        let _ = event_tx.send(ShellEvent::Snapshot(emulator.snapshot())).await;
                    }
                    Ok(ShellCmd::StepSearch { forward, reply }) => {
                        let progress = emulator.step_search(forward);
                        let _ = reply.send(progress);
                        let _ = event_tx.send(ShellEvent::Snapshot(emulator.snapshot())).await;
                    }
                    Ok(ShellCmd::ScrollDelta(lines)) => {
                        emulator.scroll_delta(lines);
                        let _ = event_tx.send(ShellEvent::Snapshot(emulator.snapshot())).await;
                    }
                    Ok(ShellCmd::ScrollToOffset(offset)) => {
                        emulator.scroll_to_offset(offset);
                        let _ = event_tx.send(ShellEvent::Snapshot(emulator.snapshot())).await;
                    }
                    Ok(ShellCmd::ScrollTo(target)) => {
                        emulator.scroll_to(target);
                        let _ = event_tx.send(ShellEvent::Snapshot(emulator.snapshot())).await;
                    }
                    Ok(ShellCmd::ClearScrollback) => {
                        emulator.clear_scrollback();
                        let _ = event_tx.send(ShellEvent::Snapshot(emulator.snapshot())).await;
                    }
                    Err(_) => {
                        // Every handle is gone: the terminal tab was closed.
                        // russh does not close a dropped channel, so without
                        // this the remote shell would live on until the whole
                        // connection ends.
                        let _ = channel.close().await;
                        break;
                    }
                }
            }

            msg = channel.wait() => {
                match msg {
                    Some(ChannelMsg::Data { data }) => {
                        emulator.advance(data.as_ref());
                        let replies = emulator.take_replies();
                        if !replies.is_empty() {
                            if let Err(e) = channel.data(replies.as_slice()).await {
                                let _ = event_tx.send(ShellEvent::Error(format!("send reply: {e}"))).await;
                                break;
                            }
                        }
                        let _ = event_tx.send(ShellEvent::Snapshot(emulator.snapshot())).await;
                    }
                    Some(ChannelMsg::ExtendedData { data, .. }) => {
                        emulator.advance(data.as_ref());
                        let replies = emulator.take_replies();
                        if !replies.is_empty() {
                            if let Err(e) = channel.data(replies.as_slice()).await {
                                let _ = event_tx.send(ShellEvent::Error(format!("send reply: {e}"))).await;
                                break;
                            }
                        }
                        let _ = event_tx.send(ShellEvent::Snapshot(emulator.snapshot())).await;
                    }
                    Some(ChannelMsg::ExitStatus { .. })
                    | Some(ChannelMsg::Eof)
                    | Some(ChannelMsg::Close) => {
                        let _ = event_tx.send(ShellEvent::Exited).await;
                        break;
                    }
                    None => {
                        let _ = event_tx.send(ShellEvent::Disconnected).await;
                        break;
                    }
                    Some(_) => {}
                }
            }
        }
    }
}

/// Quote `text` as one shell word. Works in POSIX shells and in fish: a single
/// quote and a backslash are written outside the quotes, because fish reads a
/// backslash inside single quotes as an escape.
fn shell_quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('\'');
    for character in text.chars() {
        match character {
            '\'' => quoted.push_str(r"'\''"),
            '\\' => quoted.push_str(r"'\\'"),
            other => quoted.push(other),
        }
    }
    quoted.push('\'');
    quoted
}

/// The keys that make an interactive shell change into `path`.
///
/// `None` when the path has a control character: the line editor acts on those
/// even inside quotes (a newline runs the line), so such a path cannot be typed.
pub fn cd_input(path: &str) -> Option<Vec<u8>> {
    if path.is_empty() || path.chars().any(char::is_control) {
        return None;
    }
    // Ctrl+E, Ctrl+U drop a half-typed line first: `rm -rf ` followed by our
    // text would be a different command. The leading space keeps the cd out of
    // the history where the shell is set up for that.
    Some(format!("\x05\x15 cd -- {}\r", shell_quote(path)).into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(path: &str) -> String {
        String::from_utf8(cd_input(path).unwrap()).unwrap()
    }

    #[test]
    fn cd_input_quotes_a_plain_path() {
        assert_eq!(typed("/var/www/app"), "\x05\x15 cd -- '/var/www/app'\r");
    }

    #[test]
    fn cd_input_keeps_shell_syntax_inside_quotes() {
        assert_eq!(
            typed("/srv/my app/$(reboot);`id` && !x *"),
            "\x05\x15 cd -- '/srv/my app/$(reboot);`id` && !x *'\r"
        );
        assert_eq!(typed("/srv/-rf"), "\x05\x15 cd -- '/srv/-rf'\r");
        assert_eq!(typed("/srv/папка"), "\x05\x15 cd -- '/srv/папка'\r");
    }

    #[test]
    fn shell_quote_escapes_quotes_and_backslashes() {
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("'"), r"''\'''");
        assert_eq!(shell_quote(r"a\b"), r"'a'\\'b'");
        // A trailing backslash must not swallow the closing quote in fish.
        assert_eq!(shell_quote("dir\\"), r"'dir'\\''");
        assert_eq!(shell_quote(r#"a"b"#), r#"'a"b'"#);
    }

    #[test]
    fn cd_input_refuses_control_characters_and_empty_paths() {
        assert!(cd_input("/srv/a\nrm -rf ~").is_none());
        assert!(cd_input("/srv/a\rb").is_none());
        assert!(cd_input("/srv/a\tb").is_none());
        assert!(cd_input("/srv/a\x1b[201~").is_none());
        assert!(cd_input("/srv/a\x7fb").is_none());
        assert!(cd_input("").is_none());
    }
}
