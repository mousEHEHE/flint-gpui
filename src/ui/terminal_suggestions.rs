//! Command suggestions for the terminal.
//!
//! As the user types at a shell prompt, a popup of matching commands from the
//! server's shell history (`~/.bash_history`, `~/.zsh_history`, fish history)
//! appears under the cursor. Right accepts the top row (or keeps the highlighted
//! one), fish-style. Down enters the list and steps through it, writing the
//! highlighted command straight into the real terminal line; Up steps back up. At
//! the far edges the arrows leave the popup and reach the shell so the user
//! browses real terminal history: Up from the top row (or before the list is
//! entered), Down past the bottom row. Enter runs the line; Esc hides the popup
//! for the rest of the line.
//!
//! Tab and Shift-Tab are never taken: they belong to the shell's own completion,
//! and pressing either hides the popup for the line so it does not cover the
//! shell's completion list.
//!
//! The popup stays closed inside REPL prompts (`mysql>`, `>>>`, `db=#`), where
//! shell history makes no sense ([`is_repl_line`]). Commands run in this session
//! are learned from the screen and rank first ([`resolve_learn`]).
//!
//! The emulator has no "current input line", only a grid of cells, so the typed
//! query is read from the cursor row of the latest [`TermSnapshot`]. The prompt
//! is cut off with a few delimiter heuristics ([`extract_query`]), which is
//! enough for default bash, zsh and fish prompts. OSC 133 shell-integration
//! marks would give the exact start of the input; they are not read yet.

use std::collections::HashSet;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    anchored, deferred, div, px, Anchor, AnyElement, App, Bounds, FontWeight, Pixels, Point,
    SharedString,
};
use gpui_component::{h_flex, v_flex, ActiveTheme as _};

use crate::preferences::Preferences;
use crate::shell::TermSnapshot;
use crate::theme as flint_theme;
use crate::ui::terminal_element::terminal_font_family;

/// Cap on how many history entries we keep in memory per session.
const MAX_HISTORY: usize = 5_000;
/// Cap on how many suggestions the popup shows at once.
const MAX_VISIBLE: usize = 8;
/// Height the key-hint footer adds to the popup, for the flip-above estimate.
const FOOTER_HEIGHT: f32 = 20.0;
/// How much taller a popup row is than its text.
const ROW_PADDING: f32 = 10.0;

/// Prompt terminators we look for to split `prompt` from typed `command`. We take
/// the text after the *rightmost* match. Covers bash/zsh/sh (`$ `, `# `, `% `)
/// and common powerline/starship prompts (`❯ `, `➜ `), plus secondary `> `.
const PROMPT_DELIMS: &[&str] = &["$ ", "# ", "% ", "❯ ", "➜ ", "> "];

/// What a command line starts with in fish's history file.
const FISH_COMMAND_PREFIX: &str = "- cmd: ";

/// Parse the concatenated contents of one or more shell history files into a
/// newest-first, de-duplicated list of command lines.
///
/// Three on-disk formats:
/// - bash: one command per line, verbatim.
/// - zsh extended: `: <timestamp>:<elapsed>;<command>`, the command follows the `;`.
/// - fish: `- cmd: <command>` lines; the `  when:` and `  paths:` metadata is skipped.
///
/// A suggestion is typed into the live shell key by key, so a command that
/// cannot be typed as one plain line is left out: one with a control character
/// (a Tab would start completion, an Escape a key sequence, a newline would
/// run the line) and one that spans several lines.
pub fn parse_history(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut is_continuation = false;
    for line in raw.lines() {
        // zsh stores a command of several lines as lines that end in a
        // backslash. fish keeps a command on one line and escapes instead.
        let is_continued = line.ends_with('\\') && !line.starts_with(FISH_COMMAND_PREFIX);
        let is_part_of_multiline = is_continuation || is_continued;
        is_continuation = is_continued;
        if is_part_of_multiline {
            continue;
        }
        let Some(cmd) = parse_history_line(line) else {
            continue;
        };
        let cmd = cmd.trim();
        if cmd.is_empty() || cmd.chars().any(char::is_control) {
            continue;
        }
        out.push(cmd.to_string());
    }
    // History files grow at the end. Flip so recent commands rank first, then
    // drop the older duplicates.
    out.reverse();
    let mut seen: HashSet<String> = HashSet::new();
    out.retain(|c| seen.insert(c.clone()));
    out.truncate(MAX_HISTORY);
    out
}

fn parse_history_line(line: &str) -> Option<String> {
    let line = line.trim_end_matches('\r');
    if line.is_empty() {
        return None;
    }
    // fish: the command lives on a `- cmd: ...` line.
    if let Some(rest) = line.strip_prefix(FISH_COMMAND_PREFIX) {
        return Some(unescape_fish(rest));
    }
    // fish metadata and list entries, not commands.
    if line.starts_with("  ") || line.trim_start().starts_with("- ") {
        return None;
    }
    // zsh extended history: ": 1700000000:0;git status"
    if line.starts_with(": ") {
        if let Some(semi) = line.find(';') {
            return Some(line[semi + 1..].to_string());
        }
    }
    // bash or plain zsh: the line is the command.
    Some(line.to_string())
}

/// Undo the two escapes fish uses in its history file: `\\` is a backslash and
/// `\n` is a newline inside the command.
fn unescape_fish(command: &str) -> String {
    let mut out = String::with_capacity(command.len());
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('\\') | None => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    out
}

/// Rank history against the typed `query`: case-insensitive prefix matches first
/// (already newest-first from [`parse_history`]), then substring matches. The
/// query itself is excluded so an already-complete line doesn't suggest itself.
pub fn filter(history: &[String], query: &str) -> Vec<String> {
    if query.is_empty() {
        return Vec::new();
    }
    let ql = query.to_lowercase();
    let mut prefix: Vec<String> = Vec::new();
    let mut substr: Vec<String> = Vec::new();
    for cmd in history {
        if cmd == query {
            continue;
        }
        let cl = cmd.to_lowercase();
        if cl.starts_with(&ql) {
            prefix.push(cmd.clone());
        } else if cl.contains(&ql) {
            substr.push(cmd.clone());
        }
    }
    prefix.extend(substr);
    prefix.truncate(MAX_VISIBLE);
    prefix
}

/// Byte offset just past the rightmost [`PROMPT_DELIMS`] match, i.e. where the
/// typed command starts. `None` when the line holds no delimiter.
fn prompt_end(line: &str) -> Option<usize> {
    PROMPT_DELIMS
        .iter()
        .filter_map(|delim| line.rfind(delim).map(|pos| pos + delim.len()))
        .max()
}

/// Byte offset just past the leftmost [`PROMPT_DELIMS`] match.
fn first_prompt_end(line: &str) -> Option<usize> {
    PROMPT_DELIMS
        .iter()
        .filter_map(|delim| line.find(delim).map(|pos| pos + delim.len()))
        .min()
}

/// True for `host:port`, with an optional `[db]` index: the redis-cli prompt.
fn is_host_port(token: &str) -> bool {
    let token = match token.strip_suffix(']').and_then(|t| t.rsplit_once('[')) {
        Some((head, index)) if is_digits(index) => head,
        _ => token,
    };
    token
        .rsplit_once(':')
        .is_some_and(|(host, port)| !host.is_empty() && is_digits(port))
}

fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_digit())
}

/// True when `prompt` (a line cut just past a `> ` or `# ` delimiter) belongs to
/// a known REPL and not to a shell. The list is narrow and name-based on purpose:
/// `> ` is also a real shell prompt for some people, and a greedy match would
/// silently turn suggestions off for them.
fn is_repl_prompt(prompt: &str) -> bool {
    let Some(head) = prompt
        .strip_suffix("> ")
        .or_else(|| prompt.strip_suffix("# "))
    else {
        return false;
    };
    let is_angle = prompt.ends_with("> ");
    let head = head.trim_start();
    // Bare `> `: the shell's own continuation prompt, or node / R / mongo.
    if head.is_empty() {
        return is_angle;
    }
    // A REPL name sits right against its mark. A gap means a redirect in a
    // shell command, as in `curl box:8080 > out`.
    if head.ends_with(char::is_whitespace) {
        return false;
    }
    // psql: `db=> `, `db-> `, `db=# `, `db-# `, `db(> `. Needs a name before the
    // mark, so an arrow prompt like `~ -> ` stays a shell.
    let last_token = head.split_whitespace().next_back().unwrap_or("");
    let is_psql =
        last_token.chars().count() > 1 && last_token.ends_with(['=', '-', '(', '\'', '"']);
    if is_psql {
        return true;
    }
    if !is_angle {
        return false;
    }
    // `-> ` alone is the mysql continuation prompt.
    if head == "-" {
        return true;
    }
    let lower = head.to_lowercase();
    // The client name must be a whole word: fish and PowerShell end their
    // prompt with the directory, and `~/src/mysql> ` is still a shell.
    let name = last_token.to_lowercase();
    lower.ends_with(">>")
        || matches!(name.as_str(), "mysql" | "sqlite" | "redis")
        || lower.starts_with("mariadb [")
        || lower.starts_with("irb(")
        || lower.contains("pry(")
        || is_host_port(last_token)
}

/// True when the cursor line is a REPL prompt (python, mysql, psql, redis, irb,
/// sqlite) or a continuation line. Both the leftmost and the rightmost delimiter
/// are checked, because the typed text can hold a delimiter of its own
/// (`mysql> select 1 where a > 2`).
pub fn is_repl_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    // python continuation has no delimiter at all.
    if trimmed == "..." || trimmed.starts_with("... ") {
        return true;
    }
    [first_prompt_end(line), prompt_end(line)]
        .into_iter()
        .flatten()
        .any(|end| is_repl_prompt(&line[..end]))
}

/// Strip the shell prompt from a line, returning the typed command portion.
/// Takes the text after the rightmost [`PROMPT_DELIMS`] match; if none is found
/// (unusual prompt) the whole line is used. Leading prompt-to-command whitespace
/// is trimmed. Returns `None` inside a REPL, where nothing should be suggested.
pub fn extract_query(line_before_cursor: &str) -> Option<String> {
    if is_repl_line(line_before_cursor) {
        return None;
    }
    let tail = match prompt_end(line_before_cursor) {
        Some(end) => &line_before_cursor[end..],
        None => line_before_cursor,
    };
    Some(tail.trim_start().to_string())
}

/// A command line the user pressed Enter on, waiting for the screen to show it
/// in full. The echo can lag behind the keys, so the line at the moment of Enter
/// may still be missing its last characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingLearn {
    /// The prompt in front of the command, exactly as it was on screen.
    prompt: String,
    /// What the screen showed of the command when Enter was pressed.
    seen: String,
}

/// What a fresh screen says about a [`PendingLearn`].
#[derive(Debug, PartialEq, Eq)]
pub enum LearnOutcome {
    /// The shell has not taken the Enter yet; look again on the next snapshot.
    Wait,
    Learn(String),
    Nothing,
}

/// Find the command behind `pending` on the screen. `rows` are the full grid
/// rows from the top down to and including the cursor row.
///
/// Once the cursor has left the command's row, that row holds the whole echoed
/// command, so it is read from there. A row filled to its last column has
/// wrapped, and is skipped: only its first part is on that row. When the row is
/// gone (`clear`, long output), the text seen at Enter is used.
pub fn resolve_learn(pending: &PendingLearn, rows: &[String]) -> LearnOutcome {
    let Some((cursor_row, above)) = rows.split_last() else {
        return LearnOutcome::Nothing;
    };
    let is_still_typing = cursor_row
        .strip_prefix(pending.prompt.as_str())
        .is_some_and(|rest| rest.trim().starts_with(&pending.seen));
    if is_still_typing {
        return LearnOutcome::Wait;
    }
    let echoed = above.iter().rev().find_map(|row| {
        row.strip_prefix(pending.prompt.as_str())
            .map(|rest| (row, rest.trim()))
    });
    let command = match echoed {
        Some((row, _)) if !row.ends_with(' ') => return LearnOutcome::Nothing,
        Some((_, rest)) if !rest.is_empty() && rest.starts_with(&pending.seen) => rest,
        _ => pending.seen.as_str(),
    };
    if command.is_empty() {
        return LearnOutcome::Nothing;
    }
    LearnOutcome::Learn(command.to_string())
}

/// Byte sequence that turns the current terminal line `current` into `target`
/// using the shell's own line editor: backspace away the diverging suffix of
/// `current`, then type the diverging suffix of `target`. Relies only on `\x7f`
/// (backspace) + printable bytes, so it works in any shell / edit mode. Assumes
/// the cursor is at the end of the line (guaranteed by the show conditions).
pub fn fill_bytes(current: &str, target: &str) -> Vec<u8> {
    let cur: Vec<char> = current.chars().collect();
    let tgt: Vec<char> = target.chars().collect();
    let mut k = 0;
    while k < cur.len() && k < tgt.len() && cur[k] == tgt[k] {
        k += 1;
    }
    let mut bytes = vec![0x7f; cur.len() - k];
    let suffix: String = tgt[k..].iter().collect();
    bytes.extend_from_slice(suffix.as_bytes());
    bytes
}

/// The text on the cursor's row up to the cursor, but only when the snapshot is
/// in a state where suggestions make sense: live tail (not scrolled back),
/// cursor visible, not in an alt-screen app (vim/htop/less), not mouse-reporting,
/// and the cursor sitting at the end of its line (nothing but blanks to its
/// right). Returns `None` otherwise.
fn readable_line(snap: &TermSnapshot) -> Option<String> {
    if snap.display_offset != 0
        || !snap.cursor_visible
        || snap.alt_screen
        || snap.mouse_mode.any_active()
    {
        return None;
    }
    let row = snap.cursor.0 as usize;
    let col = snap.cursor.1 as usize;
    let cells = snap.cells.get(row)?;
    let rest_blank = cells.iter().skip(col).all(|c| c.c == ' ');
    if !rest_blank {
        return None;
    }
    Some(cells.iter().take(col).map(|c| c.c).collect())
}

/// Full text of every grid row from the top down to the cursor row, for
/// [`resolve_learn`]. `None` while the grid is not the live shell screen.
fn rows_to_cursor(snap: &TermSnapshot) -> Option<Vec<String>> {
    if snap.display_offset != 0 || snap.alt_screen {
        return None;
    }
    let cursor_row = snap.cursor.0 as usize;
    let rows = snap.cells.get(..=cursor_row)?;
    Some(
        rows.iter()
            .map(|cells| cells.iter().map(|c| c.c).collect())
            .collect(),
    )
}

/// What an arrow press resolves to at the popup (see
/// [`SuggestionState::navigate_up`] / [`SuggestionState::navigate_down`]).
pub enum NavAction {
    /// Highlight moved one row; rewrite the terminal line with these bytes.
    Moved(Vec<u8>),
    /// Leave the popup at a far edge: the top row for Up (or before the list is
    /// entered), the bottom row for Down. Send these bytes to restore the typed
    /// query, then forward the arrow so the shell walks its own history. The byte
    /// vector is empty when the line already holds the query.
    Exit(Vec<u8>),
}

/// Live state of the suggestion popup, owned by `TerminalView`.
pub struct SuggestionState {
    history: Arc<Vec<String>>,
    pub is_open: bool,
    matches: Vec<String>,
    selected: Option<usize>,
    /// The query the user typed, which is the filter key. The live grid line
    /// may hold a filled-in suggestion in its place.
    typed: String,
    /// Suppress the popup for the rest of this line (set by Esc, Tab, or an
    /// arrow that leaves the list); cleared when the line empties.
    dismissed_for_line: bool,
    /// The suggestion we just wrote into the line; the echoing snapshot equals it
    /// and must not be mistaken for the user typing a new query.
    pending_fill: Option<String>,
    /// The shell prompt of the current line, recorded while the line was still
    /// empty. With it the typed command is an exact cut, also when the command
    /// holds a prompt delimiter of its own (`echo a > b`).
    prompt: Option<String>,
    /// A command the user just ran, not yet read back from the screen.
    pending_learn: Option<PendingLearn>,
}

impl Default for SuggestionState {
    fn default() -> Self {
        Self {
            history: Arc::new(Vec::new()),
            is_open: false,
            matches: Vec::new(),
            selected: None,
            typed: String::new(),
            dismissed_for_line: false,
            pending_fill: None,
            prompt: None,
            pending_learn: None,
        }
    }
}

impl SuggestionState {
    /// Add the server's shell history under what is already known. The fetch
    /// lands a moment after connect, so commands learned in the meantime stay
    /// in front of it.
    pub fn set_history(&mut self, fetched: Vec<String>) {
        let history = Arc::make_mut(&mut self.history);
        let mut known: HashSet<String> = history.iter().cloned().collect();
        history.extend(fetched.into_iter().filter(|cmd| known.insert(cmd.clone())));
        history.truncate(MAX_HISTORY);
    }

    /// Put `command` at the front of the history, so it is the first suggestion
    /// the next time its start is typed. Kept in memory only.
    pub fn learn(&mut self, command: &str) {
        let command = command.trim();
        if command.is_empty() {
            return;
        }
        let history = Arc::make_mut(&mut self.history);
        history.retain(|known| known != command);
        history.insert(0, command.to_string());
        history.truncate(MAX_HISTORY);
    }

    /// The user pressed Enter. Remember the line so the command can be learned
    /// once the screen shows it in full (see [`resolve_learn`]). Does nothing
    /// unless the line starts with a recorded shell prompt, which keeps password
    /// prompts, REPL input and wrapped lines out of the history.
    pub fn note_enter(&mut self, snap: &TermSnapshot) {
        let Some(prompt) = self.prompt.as_ref() else {
            return;
        };
        let Some(line) = readable_line(snap) else {
            return;
        };
        let Some(rest) = line.strip_prefix(prompt.as_str()) else {
            return;
        };
        self.pending_learn = Some(PendingLearn {
            prompt: prompt.clone(),
            seen: rest.trim().to_string(),
        });
    }

    /// Forget everything about the current line and close the popup.
    pub fn reset(&mut self) {
        self.reset_line();
        self.prompt = None;
        self.pending_learn = None;
        self.pending_fill = None;
    }

    fn reset_line(&mut self) {
        self.dismissed_for_line = false;
        self.typed.clear();
        self.matches.clear();
        self.selected = None;
        self.is_open = false;
    }

    fn learn_from_screen(&mut self, snap: &TermSnapshot) {
        let Some(pending) = self.pending_learn.as_ref() else {
            return;
        };
        let Some(rows) = rows_to_cursor(snap) else {
            return;
        };
        match resolve_learn(pending, &rows) {
            LearnOutcome::Wait => {}
            LearnOutcome::Learn(command) => {
                self.pending_learn = None;
                self.learn(&command);
            }
            LearnOutcome::Nothing => self.pending_learn = None,
        }
    }

    /// The typed command on `line`: an exact cut when the line starts with the
    /// recorded prompt, the delimiter heuristic otherwise. `None` in a REPL.
    fn query_for(&self, line: &str) -> Option<String> {
        let exact = self
            .prompt
            .as_ref()
            .and_then(|prompt| line.strip_prefix(prompt.as_str()));
        match exact {
            Some(rest) => Some(rest.trim_start().to_string()),
            None => extract_query(line),
        }
    }

    /// Recompute popup state from a fresh snapshot. The caller then reads
    /// [`SuggestionState::is_open`] and [`SuggestionState::render_popup`].
    pub fn on_snapshot(&mut self, snap: &TermSnapshot) {
        // Before the prompt is re-recorded below: the pending command sits
        // behind the prompt it was typed at.
        self.learn_from_screen(snap);
        let Some(line) = readable_line(snap) else {
            self.is_open = false;
            return;
        };
        // Echo of our own fill: keep the popup exactly as it was. Compare on
        // the full line (`prompt + target`) with `ends_with`, so commands that
        // themselves contain a prompt delimiter (e.g. `echo a > b`) still match.
        if let Some(target) = self.pending_fill.clone() {
            if line.ends_with(&target) {
                self.pending_fill = None;
            }
            // Matched or still settling: either way this is not the user typing.
            return;
        }

        let Some(query) = self.query_for(&line) else {
            // REPL prompt: nothing to suggest, and no shell prompt to cut at.
            self.reset_line();
            self.prompt = None;
            return;
        };

        if query.is_empty() {
            // Empty line: start over and record the prompt for the next command.
            self.reset_line();
            self.prompt = prompt_end(&line).map(|end| line[..end].to_string());
            return;
        }

        if self.dismissed_for_line {
            self.is_open = false;
            return;
        }

        if query != self.typed {
            self.typed = query.clone();
            self.matches = filter(&self.history, &query);
            self.selected = None;
        }
        self.is_open = !self.matches.is_empty();
    }

    /// Handle the Up arrow. Down is the only key that enters the list, so Up
    /// never takes a row from a popup that was not entered: it leaves the popup
    /// for shell history. Pressing Up on the top row exits the same way. See
    /// [`NavAction`].
    pub fn navigate_up(&mut self) -> Option<NavAction> {
        if !self.is_open || self.matches.is_empty() {
            return None;
        }
        match self.selected {
            // Not in the list yet: ignore the popup, hand Up to the shell.
            None => {
                self.dismiss_for_line();
                Some(NavAction::Exit(Vec::new()))
            }
            // On the top row: restore the typed query, then leave for the shell.
            Some(0) => {
                let bytes = fill_bytes(&self.matches[0], &self.typed);
                self.dismiss_for_line();
                Some(NavAction::Exit(bytes))
            }
            Some(i) => {
                let current = self.matches[i].clone();
                let next = i - 1;
                self.selected = Some(next);
                let target = self.matches[next].clone();
                let bytes = fill_bytes(&current, &target);
                self.pending_fill = Some(target);
                Some(NavAction::Moved(bytes))
            }
        }
    }

    /// Handle the Down arrow. Down enters the list, so on a popup that was not
    /// entered it selects the first row. Stepping past the last row leaves the
    /// popup: the typed query is restored and Down reaches the shell, which walks
    /// forward through its own history. Mirrors [`Self::navigate_up`].
    pub fn navigate_down(&mut self) -> Option<NavAction> {
        if !self.is_open || self.matches.is_empty() {
            return None;
        }
        let last = self.matches.len() - 1;
        match self.selected {
            // Not in the list yet: enter at the top row.
            None => {
                let target = self.matches[0].clone();
                let bytes = fill_bytes(&self.typed, &target);
                self.selected = Some(0);
                self.pending_fill = Some(target);
                Some(NavAction::Moved(bytes))
            }
            // On the bottom row: restore the typed query, then leave for the shell.
            Some(i) if i == last => {
                let bytes = fill_bytes(&self.matches[i], &self.typed);
                self.dismiss_for_line();
                Some(NavAction::Exit(bytes))
            }
            Some(i) => {
                let current = self.matches[i].clone();
                let next = i + 1;
                self.selected = Some(next);
                let target = self.matches[next].clone();
                let bytes = fill_bytes(&current, &target);
                self.pending_fill = Some(target);
                Some(NavAction::Moved(bytes))
            }
        }
    }

    /// Accept a suggestion and close the popup (Right arrow, fish-style): the
    /// highlighted row when Down has entered the list, the top row otherwise.
    /// Returns the bytes that complete the terminal line, empty when the line
    /// already holds the row. `None` when the popup is closed.
    pub fn accept(&mut self) -> Option<Vec<u8>> {
        if !self.is_open || self.matches.is_empty() {
            return None;
        }
        let (target, bytes) = match self.selected {
            Some(i) => (self.matches[i].clone(), Vec::new()),
            None => {
                let target = self.matches[0].clone();
                let bytes = fill_bytes(&self.typed, &target);
                self.pending_fill = Some(target.clone());
                (target, bytes)
            }
        };
        // The accepted command is now "what the user typed", so its echo does
        // not re-open the popup; the next real edit filters again.
        self.typed = target;
        self.matches.clear();
        self.selected = None;
        self.is_open = false;
        Some(bytes)
    }

    /// Hide the popup and suppress it until the line is cleared (Esc, Tab, or
    /// an arrow that leaves the list).
    pub fn dismiss_for_line(&mut self) {
        self.is_open = false;
        self.dismissed_for_line = true;
        self.pending_fill = None;
    }

    /// Hide the popup without suppressing future re-opens (Enter).
    pub fn close(&mut self) {
        self.is_open = false;
        self.pending_fill = None;
    }

    /// The user edited the line directly (printable key / paste). Forget any
    /// in-flight fill so the next snapshot re-filters from the real input.
    pub fn note_user_edit(&mut self) {
        self.pending_fill = None;
    }

    /// The footer line naming the keys, for the list as it stands.
    fn key_hint(&self) -> &'static str {
        match self.selected {
            None => "→ accept  ·  ↓ browse  ·  esc hide",
            Some(_) => "⏎ run  ·  → keep  ·  ↑↓ browse  ·  esc hide",
        }
    }

    /// Build the popup anchored to the cursor cell `cursor` (window-space rect).
    /// It hangs below the cursor, and sits above the line when it would cross
    /// `area_bottom`, the terminal's bottom edge. Returns `None` when closed.
    pub fn render_popup(
        &self,
        cursor: Bounds<Pixels>,
        area_bottom: Pixels,
        cx: &App,
    ) -> Option<AnyElement> {
        if !self.is_open || self.matches.is_empty() {
            return None;
        }
        let fg = cx.theme().foreground;
        let selected_bg = cx.theme().list_hover;
        let typed_chars = self.typed.chars().count();
        // The rows complete the line above them, so they use its font and size.
        let font_family = terminal_font_family(cx);
        let font_size = Preferences::terminal_font_size(cx);
        let row_height = font_size + ROW_PADDING;

        let rows = self.matches.iter().enumerate().map(|(ix, cmd)| {
            let is_sel = self.selected == Some(ix);
            // Bold the matched leading prefix (only when this is a prefix match),
            // mute the remainder.
            let is_prefix = cmd.to_lowercase().starts_with(&self.typed.to_lowercase());
            let split = if is_prefix { typed_chars } else { 0 };
            let head: String = cmd.chars().take(split).collect();
            let tail: String = cmd.chars().skip(split).collect();

            let head_color = fg;
            let tail_color = if is_sel {
                fg
            } else {
                flint_theme::muted_gray()
            };

            // The command text, truncated with an ellipsis so long lines stay
            // inside the popup instead of spilling past its edge.
            let mut text = h_flex()
                .flex_1()
                .min_w(px(0.))
                .items_center()
                .overflow_hidden();
            if !head.is_empty() {
                text = text.child(
                    div()
                        .flex_none()
                        .text_color(head_color)
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(SharedString::from(head)),
                );
            }
            text = text.child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .text_color(tail_color)
                    .child(SharedString::from(tail)),
            );

            h_flex()
                .w_full()
                .h(px(row_height))
                .px(px(6.))
                .gap(px(8.))
                .items_center()
                .rounded(px(4.))
                .text_size(px(font_size))
                .font_family(font_family)
                .when(is_sel, |r| r.bg(selected_bg))
                .child(text)
        });

        let panel = v_flex()
            .occlude()
            .min_w(px(240.))
            .max_w(px(560.))
            .p(px(4.))
            .gap(px(1.))
            .bg(flint_theme::pop_surface())
            .border_1()
            .border_color(flint_theme::border_focused())
            .rounded(px(6.))
            .shadow_md()
            .children(rows)
            .child(
                div()
                    .px(px(6.))
                    .pt(px(3.))
                    .text_size(px(11.))
                    .text_color(flint_theme::text_dim())
                    .child(self.key_hint()),
            );

        // The height is estimated from the row count. A popup that does not fit
        // below is anchored by its bottom to the top of the cursor cell, so it
        // grows upward and leaves the current line visible.
        let est_height = 10.0 + self.matches.len() as f32 * (row_height + 1.0) + FOOTER_HEIGHT;
        let below_y = cursor.origin.y + cursor.size.height;
        let place_above = below_y.as_f32() + est_height + 8.0 > area_bottom.as_f32();

        let (position, corner) = if place_above {
            (
                Point::new(cursor.origin.x, cursor.origin.y),
                Anchor::BottomLeft,
            )
        } else {
            (Point::new(cursor.origin.x, below_y), Anchor::TopLeft)
        };

        Some(
            deferred(
                anchored()
                    .position(position)
                    .anchor(corner)
                    .snap_to_window_with_margin(px(8.))
                    .child(panel),
            )
            .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bash_plain() {
        let h = parse_history("git status\nls -la\ncargo build\n");
        // Newest-first.
        assert_eq!(h, vec!["cargo build", "ls -la", "git status"]);
    }

    #[test]
    fn parse_zsh_extended() {
        let raw = ": 1700000000:0;git status\n: 1700000001:5;cargo build\n";
        let h = parse_history(raw);
        assert_eq!(h, vec!["cargo build", "git status"]);
    }

    #[test]
    fn parse_fish() {
        let raw = "- cmd: git push\n  when: 1700000000\n- cmd: vim foo\n  when: 1700000001\n  paths:\n    - foo\n";
        let h = parse_history(raw);
        assert_eq!(h, vec!["vim foo", "git push"]);
    }

    #[test]
    fn parse_skips_commands_with_control_characters() {
        let h = parse_history("ls\necho a\tb\nprintf \x1b[0m\nclear\rreboot\npwd\n");
        assert_eq!(h, vec!["pwd", "ls"]);
    }

    #[test]
    fn parse_skips_zsh_commands_of_several_lines() {
        let raw = ": 1700000000:0;ls\n: 1700000001:0;for f in *; do\\\n  rm $f\\\ndone\n: 1700000002:0;pwd\n";
        assert_eq!(parse_history(raw), vec!["pwd", "ls"]);
    }

    #[test]
    fn parse_fish_undoes_its_escapes() {
        let raw = "- cmd: echo a\\\\b\n  when: 1\n- cmd: echo a\\nrm b\n  when: 2\n- cmd: cd C:\\\\\n  when: 3\n";
        assert_eq!(parse_history(raw), vec![r"cd C:\", r"echo a\b"]);
    }

    #[test]
    fn parse_dedup_keeps_recent() {
        let h = parse_history("git status\nls\ngit status\n");
        // Most-recent `git status` wins its position; older dup dropped.
        assert_eq!(h, vec!["git status", "ls"]);
    }

    #[test]
    fn filter_prefix_before_substring() {
        let hist = vec![
            "git log".to_string(),
            "echo git".to_string(),
            "git status".to_string(),
        ];
        let r = filter(&hist, "git");
        assert_eq!(r, vec!["git log", "git status", "echo git"]);
    }

    #[test]
    fn filter_excludes_exact_query() {
        let hist = vec!["git status".to_string(), "git status --short".to_string()];
        // The fully-typed line shouldn't suggest itself, but longer matches stay.
        let r = filter(&hist, "git status");
        assert_eq!(r, vec!["git status --short"]);
    }

    #[test]
    fn filter_case_insensitive() {
        let hist = vec!["Git Status".to_string()];
        assert_eq!(filter(&hist, "git"), vec!["Git Status"]);
    }

    #[test]
    fn extract_query_common_prompts() {
        assert_eq!(extract_query("user@host:~/proj$ git lo").unwrap(), "git lo");
        assert_eq!(extract_query("root@box:/# systemctl").unwrap(), "systemctl");
        assert_eq!(
            extract_query("[root@box ~]# systemctl").unwrap(),
            "systemctl"
        );
        assert_eq!(extract_query("~/repo ❯ cargo b").unwrap(), "cargo b");
        assert_eq!(extract_query("host% ls").unwrap(), "ls");
        // fish's default prompt, PowerShell and arrow prompts are shells.
        assert_eq!(extract_query("bob@box ~> ls").unwrap(), "ls");
        assert_eq!(extract_query("PS C:\\Users\\bob> dir").unwrap(), "dir");
        assert_eq!(extract_query("~/repo -> ls").unwrap(), "ls");
        assert_eq!(extract_query("bob => ls").unwrap(), "ls");
        assert_eq!(extract_query("myhost> ls").unwrap(), "ls");
        // A directory named like a database client is not that client.
        assert_eq!(extract_query("bob@box ~/src/mysql> ls").unwrap(), "ls");
        assert_eq!(extract_query("bob@box ~/redis> ls").unwrap(), "ls");
        assert_eq!(extract_query("PS C:\\dev\\sqlite> dir").unwrap(), "dir");
        // A redirect in the command is not a continuation prompt.
        assert_eq!(extract_query("bob@box:~$ echo a > b").unwrap(), "b");
        assert_eq!(
            extract_query("bob@box:~$ curl box:8080 > out").unwrap(),
            "out"
        );
        assert_eq!(extract_query("bob@box:~$ cat a >> b").unwrap(), "b");
    }

    #[test]
    fn extract_query_rejects_repl_prompts() {
        for line in [
            ">>> import os",
            ">>> ",
            "... return 1",
            "...     if x > 3",
            "mysql> select 1",
            "mysql> select 1 from t where a > 2",
            "    -> from t",
            "MariaDB [(none)]> show databases",
            "MariaDB [shop]> ",
            "sqlite> .tables",
            "127.0.0.1:6379> keys *",
            "127.0.0.1:6379[2]> get a",
            "redis> ping",
            "(prod) mysql> select 1",
            "irb(main):001:0> puts 1",
            "[1] pry(main)> ls",
            "shop=> select 1",
            "shop=# select 1",
            "shop-# from t",
            "shop-> from t",
            "shop(> 1",
            "postgres=# select '$ ' ",
            "> ",
            "> echo continued",
        ] {
            assert_eq!(extract_query(line), None, "{line:?}");
        }
    }

    #[test]
    fn host_port_detection() {
        assert!(is_host_port("127.0.0.1:6379"));
        assert!(is_host_port("cache:6379[3]"));
        assert!(!is_host_port("bob@box:~"));
        assert!(!is_host_port(":6379"));
        assert!(!is_host_port("box:"));
    }

    #[test]
    fn extract_query_rightmost_delim() {
        // The `$ ` inside an earlier path must not win over the prompt's.
        assert_eq!(extract_query("a$ b:~$ git").unwrap(), "git");
    }

    #[test]
    fn extract_query_no_delim() {
        assert_eq!(extract_query("plainword").unwrap(), "plainword");
    }

    #[test]
    fn fill_bytes_pure_append() {
        // "git lo" to "git log": no backspaces, append "g".
        assert_eq!(fill_bytes("git lo", "git log"), b"g".to_vec());
    }

    #[test]
    fn fill_bytes_replace_suffix() {
        // "git log" to "git status": share "git ", backspace "log" (3), type "status".
        let bytes = fill_bytes("git log", "git status");
        let mut expected = vec![0x7f, 0x7f, 0x7f];
        expected.extend_from_slice(b"status");
        assert_eq!(bytes, expected);
    }

    #[test]
    fn fill_bytes_full_replace() {
        let bytes = fill_bytes("abc", "xyz");
        assert_eq!(bytes, vec![0x7f, 0x7f, 0x7f, b'x', b'y', b'z']);
    }

    fn open_state() -> SuggestionState {
        SuggestionState {
            is_open: true,
            typed: "git".to_string(),
            matches: vec!["git status".to_string(), "git log".to_string()],
            ..Default::default()
        }
    }

    #[test]
    fn up_unentered_exits_to_shell() {
        let mut s = open_state();
        // Down never ran, so nothing is selected: Up leaves the popup untouched.
        match s.navigate_up() {
            Some(NavAction::Exit(bytes)) => assert!(bytes.is_empty()),
            _ => panic!("expected Exit"),
        }
        assert!(!s.is_open);
        assert!(s.dismissed_for_line);
    }

    #[test]
    fn up_from_top_row_restores_query_and_exits() {
        let mut s = open_state();
        s.selected = Some(0); // highlight on "git status"
        match s.navigate_up() {
            // Restore "git status" back to the typed "git": backspace 7 chars.
            Some(NavAction::Exit(bytes)) => assert_eq!(bytes, vec![0x7f; 7]),
            _ => panic!("expected Exit"),
        }
        assert!(!s.is_open);
    }

    #[test]
    fn up_from_lower_row_moves_within_list() {
        let mut s = open_state();
        s.selected = Some(1); // highlight on "git log"
        match s.navigate_up() {
            Some(NavAction::Moved(_)) => {}
            _ => panic!("expected Moved"),
        }
        assert_eq!(s.selected, Some(0));
        assert!(s.is_open);
    }

    #[test]
    fn down_unentered_enters_top_row() {
        let mut s = open_state();
        // Down is the entry key: from the unentered state it selects row 0.
        match s.navigate_down() {
            Some(NavAction::Moved(_)) => {}
            _ => panic!("expected Moved"),
        }
        assert_eq!(s.selected, Some(0));
        assert!(s.is_open);
    }

    #[test]
    fn down_from_lower_row_moves_within_list() {
        let mut s = open_state();
        s.selected = Some(0); // highlight on "git status"
        match s.navigate_down() {
            Some(NavAction::Moved(_)) => {}
            _ => panic!("expected Moved"),
        }
        assert_eq!(s.selected, Some(1));
        assert!(s.is_open);
    }

    #[test]
    fn down_from_bottom_row_restores_query_and_exits() {
        let mut s = open_state();
        s.selected = Some(1); // highlight on the last row "git log"
        match s.navigate_down() {
            // Restore "git log" back to the typed "git": backspace 4 chars.
            Some(NavAction::Exit(bytes)) => assert_eq!(bytes, vec![0x7f; 4]),
            _ => panic!("expected Exit"),
        }
        assert!(!s.is_open);
        assert!(s.dismissed_for_line);
    }

    #[test]
    fn accept_unentered_fills_top_row_and_closes() {
        let mut s = open_state();
        // "git" to "git status": append " status".
        assert_eq!(s.accept(), Some(b" status".to_vec()));
        assert!(!s.is_open);
        assert!(!s.dismissed_for_line);
        assert_eq!(s.typed, "git status");
        assert_eq!(s.pending_fill.as_deref(), Some("git status"));
    }

    #[test]
    fn accept_selected_row_keeps_the_line() {
        let mut s = open_state();
        s.selected = Some(1); // the line already holds "git log"
        assert_eq!(s.accept(), Some(Vec::new()));
        assert!(!s.is_open);
        assert_eq!(s.typed, "git log");
    }

    #[test]
    fn accept_closed_popup_is_none() {
        let mut s = SuggestionState::default();
        assert_eq!(s.accept(), None);
    }

    #[test]
    fn key_hint_follows_selection() {
        let mut s = open_state();
        assert!(s.key_hint().contains("accept"));
        s.selected = Some(0);
        assert!(s.key_hint().contains("run"));
    }

    #[test]
    fn learn_puts_command_first_without_duplicates() {
        let mut s = SuggestionState::default();
        s.set_history(vec!["ls".to_string(), "kubectl get pods".to_string()]);
        s.learn("  kubectl get pods ");
        s.learn("");
        assert_eq!(*s.history, vec!["kubectl get pods", "ls"]);
        assert_eq!(filter(&s.history, "kub"), vec!["kubectl get pods"]);
    }

    #[test]
    fn set_history_keeps_learned_commands_in_front() {
        let mut s = SuggestionState::default();
        s.learn("make deploy");
        s.set_history(vec![
            "ls".to_string(),
            "make deploy".to_string(),
            "pwd".to_string(),
        ]);
        assert_eq!(*s.history, vec!["make deploy", "ls", "pwd"]);
    }

    const PROMPT: &str = "bob@box:~$ ";

    fn pending(seen: &str) -> PendingLearn {
        PendingLearn {
            prompt: PROMPT.to_string(),
            seen: seen.to_string(),
        }
    }

    /// Pad each line to a 40-column grid row, as the snapshot delivers them.
    fn grid(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|line| format!("{line:<40}")).collect()
    }

    #[test]
    fn resolve_learn_reads_the_row_above_the_new_prompt() {
        let rows = grid(&[
            "bob@box:~$ kubectl get pods",
            "No resources found",
            "bob@box:~$ ",
        ]);
        assert_eq!(
            resolve_learn(&pending("kubectl get pods"), &rows),
            LearnOutcome::Learn("kubectl get pods".to_string())
        );
    }

    #[test]
    fn resolve_learn_completes_a_lagging_echo() {
        // Enter was pressed when only "l" had been echoed.
        let rows = grid(&["bob@box:~$ ls -la", "total 0", "bob@box:~$ "]);
        assert_eq!(
            resolve_learn(&pending("l"), &rows),
            LearnOutcome::Learn("ls -la".to_string())
        );
        let rows = grid(&["bob@box:~$ ls -la", "total 0", "bob@box:~$ "]);
        assert_eq!(resolve_learn(&pending(""), &rows), LearnOutcome::Wait);
    }

    #[test]
    fn resolve_learn_waits_while_the_cursor_is_on_the_command() {
        let rows = grid(&["bob@box:~$ ls -la"]);
        assert_eq!(resolve_learn(&pending("ls"), &rows), LearnOutcome::Wait);
    }

    #[test]
    fn resolve_learn_takes_the_newest_matching_row() {
        let rows = grid(&[
            "bob@box:~$ ls",
            "bob@box:~$ pwd",
            "/home/bob",
            "bob@box:~$ ",
        ]);
        assert_eq!(
            resolve_learn(&pending("pwd"), &rows),
            LearnOutcome::Learn("pwd".to_string())
        );
    }

    #[test]
    fn resolve_learn_falls_back_to_the_seen_text() {
        // `clear` wiped the row; a changed prompt after `cd` hides nothing.
        let rows = grid(&["bob@box:~$ "]);
        assert_eq!(
            resolve_learn(&pending("clear"), &rows),
            LearnOutcome::Learn("clear".to_string())
        );
        let rows = grid(&["bob@box:~$ cd /var", "bob@box:/var$ "]);
        assert_eq!(
            resolve_learn(&pending("cd /var"), &rows),
            LearnOutcome::Learn("cd /var".to_string())
        );
        // The newest prompt row is some other command: trust what was seen.
        let rows = grid(&["bob@box:~$ ls", "a b", ">>> "]);
        assert_eq!(
            resolve_learn(&pending("python3"), &rows),
            LearnOutcome::Learn("python3".to_string())
        );
    }

    #[test]
    fn resolve_learn_skips_wrapped_and_empty_commands() {
        let wrapped = format!("{PROMPT}{}", "x".repeat(40 - PROMPT.len()));
        let rows = vec![wrapped, format!("{:<40}", "xxxx"), format!("{PROMPT:<40}")];
        assert_eq!(resolve_learn(&pending("xx"), &rows), LearnOutcome::Nothing);
        // Enter on an empty prompt.
        let rows = grid(&["bob@box:~$ ", "bob@box:~$ "]);
        assert_eq!(resolve_learn(&pending(""), &rows), LearnOutcome::Wait);
        assert_eq!(resolve_learn(&pending("x"), &[]), LearnOutcome::Nothing);
    }
}
