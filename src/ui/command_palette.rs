use gpui::prelude::*;
use gpui::{
    actions, div, px, AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, KeyBinding, MouseButton, ScrollHandle, SharedString, Window,
};
use gpui_component::{
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex, ActiveTheme as _, Icon, IconName,
};
use uuid::Uuid;

use crate::preferences::{Preferences, ThemePreference};
use crate::ssh_target::{find_saved_host, find_saved_host_by_name, parse_ssh_target, SshTarget};
use crate::theme as flint_theme;
use crate::ui::file_pane::{self, PaneKind};
use crate::ui::host_icon::host_icon;
use crate::ui::terminal_tabs::{NEW_TERMINAL_SHORTCUT, TOGGLE_FILES_SHORTCUT};
use crate::ui::VaultState;
use crate::vault::{ServerEntrySummary, Snippet};

actions!(command_palette, [Close, Up, Down, Activate]);

/// Most snippets to surface in the "Recent" section; the rest fall through to
/// the regular "Snippets" list.
const RECENT_CAP: usize = 5;

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", Close, Some("CommandPalette")),
        KeyBinding::new("up", Up, Some("CommandPalette")),
        KeyBinding::new("down", Down, Some("CommandPalette")),
        KeyBinding::new("enter", Activate, Some("CommandPalette")),
    ]);
}

pub enum CommandPaletteEvent {
    Close,
    /// Open this server in a tab (no snippet attached).
    OpenServer(Uuid),
    /// Connect to an address typed into the search box. The user is always set.
    QuickConnect(SshTarget),
    /// Run the snippet on the active session (`None`) or on the given
    /// server, opening it first if needed.
    RunSnippet {
        snippet_id: Uuid,
        server_id: Option<Uuid>,
    },
    RunCommand(PaletteCommand),
    /// What the left file pane should show: this computer (`None`) or a
    /// saved server.
    LeftSourcePicked(Option<Uuid>),
}

/// An app action the palette can run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PaletteCommand {
    NewHost,
    ImportHosts,
    OpenSettings,
    LockVault,
    SetTheme(ThemePreference),
    NewTerminal,
    ToggleSftpPane,
    FocusFiles,
    CloseSession,
    ReconnectSession,
    ChooseLeftSource,
}

struct CommandSpec {
    command: PaletteCommand,
    label: &'static str,
    /// Extra words the search matches; never shown.
    keywords: &'static str,
    note: &'static str,
    shortcut: Option<&'static str>,
    /// Only listed while a session tab is active.
    needs_session: bool,
}

const fn chord(mac: &'static str, other: &'static str) -> &'static str {
    if cfg!(target_os = "macos") {
        mac
    } else {
        other
    }
}

/// Shortcuts here must track the bindings in `app_view::init`.
const COMMANDS: [CommandSpec; 13] = [
    CommandSpec {
        command: PaletteCommand::NewHost,
        label: "New host",
        keywords: "add server connection",
        note: "",
        shortcut: Some(chord("\u{2318}T", "Ctrl+Shift+T")),
        needs_session: false,
    },
    CommandSpec {
        command: PaletteCommand::ImportHosts,
        label: "Import hosts",
        keywords: "config putty termius",
        note: "",
        shortcut: None,
        needs_session: false,
    },
    CommandSpec {
        command: PaletteCommand::OpenSettings,
        label: "Settings",
        keywords: "preferences options",
        note: "",
        shortcut: Some(chord("\u{2318},", "Ctrl+,")),
        needs_session: false,
    },
    CommandSpec {
        command: PaletteCommand::LockVault,
        label: "Lock vault",
        keywords: "hide secure",
        note: "sessions stay connected",
        shortcut: Some(crate::ui::app_view::LOCK_SHORTCUT),
        needs_session: false,
    },
    CommandSpec {
        command: PaletteCommand::SetTheme(ThemePreference::Dark),
        label: "Theme: Dark",
        keywords: "appearance color",
        note: "",
        shortcut: None,
        needs_session: false,
    },
    CommandSpec {
        command: PaletteCommand::SetTheme(ThemePreference::Light),
        label: "Theme: Light",
        keywords: "appearance color",
        note: "",
        shortcut: None,
        needs_session: false,
    },
    CommandSpec {
        command: PaletteCommand::SetTheme(ThemePreference::System),
        label: "Theme: System",
        keywords: "appearance color auto",
        note: "",
        shortcut: None,
        needs_session: false,
    },
    CommandSpec {
        command: PaletteCommand::NewTerminal,
        label: "New terminal",
        keywords: "shell tab another second",
        note: "same connection, no new login",
        shortcut: Some(NEW_TERMINAL_SHORTCUT),
        needs_session: true,
    },
    CommandSpec {
        command: PaletteCommand::ToggleSftpPane,
        label: "Show or hide files",
        keywords: "toggle sftp panes browser full height terminal",
        note: "",
        shortcut: Some(TOGGLE_FILES_SHORTCUT),
        needs_session: true,
    },
    CommandSpec {
        command: PaletteCommand::FocusFiles,
        label: "Focus files",
        keywords: "sftp panes browser keyboard",
        note: "the same key goes back to the terminal",
        shortcut: Some(chord("\u{21E7}\u{2318}E", "Ctrl+Shift+E")),
        needs_session: true,
    },
    CommandSpec {
        command: PaletteCommand::ChooseLeftSource,
        label: "Left pane: choose source\u{2026}",
        keywords: "files another server local computer copy between",
        note: "",
        shortcut: None,
        needs_session: true,
    },
    CommandSpec {
        command: PaletteCommand::CloseSession,
        label: "Close session",
        keywords: "tab disconnect",
        note: "",
        shortcut: Some(chord("\u{2318}W", "Ctrl+Shift+W")),
        needs_session: true,
    },
    CommandSpec {
        command: PaletteCommand::ReconnectSession,
        label: "Reconnect session",
        keywords: "tab restart",
        note: "closes the tab and connects again",
        shortcut: None,
        needs_session: true,
    },
];

/// What the palette is currently showing.
enum Mode {
    /// Hosts, snippets and commands.
    Main,
    /// "Choose a host to run `snippet_name`": only hosts listed.
    PickServerForSnippet {
        snippet_id: Uuid,
        snippet_name: String,
    },
    /// "Choose what the left file pane shows": this computer, then the hosts
    /// other than the session's own.
    PickLeftSource { session_server_id: Uuid },
}

/// One render-ready item in the palette list.
enum PaletteItem {
    Connect(ConnectRow),
    Host(HostRow),
    Snippet(SnippetRow),
    Command(&'static CommandSpec),
    /// The local disk, as a source for the left file pane.
    ThisComputer,
}

struct ConnectRow {
    target: SshTarget,
    /// A host with this login, address and port is already in the vault, so
    /// Enter opens it instead of saving a new one.
    is_saved: bool,
    /// No user was typed and no saved host has this name, so the local user
    /// name was filled in. Such a row is a guess and must not take Enter away
    /// from a real match (`ssh re` while looking for a snippet "ssh restart").
    is_user_guessed: bool,
}

struct HostRow {
    id: Uuid,
    label: String,
    sub: String,
    /// What the connection probe found, for the system's mark.
    os: Option<String>,
    is_connected: bool,
}

struct SnippetRow {
    id: Uuid,
    name: String,
    description: Option<String>,
}

pub struct CommandPalette {
    focus_handle: FocusHandle,
    vault_state: Entity<VaultState>,
    search: Entity<InputState>,
    selected_ix: usize,
    scroll_handle: ScrollHandle,
    mode: Mode,
    has_active_server: bool,
    /// The server whose tab is active, if any. Snippets recently run on it are
    /// surfaced first in the list.
    active_server_id: Option<Uuid>,
    /// Ids of the servers with an open tab, as strings. They get the green dot.
    connected_server_ids: std::collections::HashSet<String>,
    /// The row count and section starts the last render showed. The arrow keys
    /// work from these instead of building every row again.
    shown_count: usize,
    shown_sections: Vec<(SharedString, usize)>,
}

impl CommandPalette {
    pub fn new(
        vault_state: Entity<VaultState>,
        has_active_server: bool,
        active_server_id: Option<Uuid>,
        connected_server_ids: std::collections::HashSet<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::with_mode(
            vault_state,
            Mode::Main,
            "Search hosts, snippets, commands, or type user@host\u{2026}",
            has_active_server,
            active_server_id,
            connected_server_ids,
            window,
            cx,
        )
    }

    /// A palette that only picks what the left file pane of the session on
    /// `session_server_id` shows.
    pub fn new_left_source_picker(
        vault_state: Entity<VaultState>,
        session_server_id: Uuid,
        connected_server_ids: std::collections::HashSet<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::with_mode(
            vault_state,
            Mode::PickLeftSource { session_server_id },
            "Search hosts\u{2026}",
            true,
            Some(session_server_id),
            connected_server_ids,
            window,
            cx,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn with_mode(
        vault_state: Entity<VaultState>,
        mode: Mode,
        placeholder: &'static str,
        has_active_server: bool,
        active_server_id: Option<Uuid>,
        connected_server_ids: std::collections::HashSet<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let focus_handle = cx.focus_handle();
        search.update(cx, |s, cx| s.focus(window, cx));

        cx.observe(&vault_state, |this, _, cx| this.select(0, cx))
            .detach();

        cx.subscribe(&search, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.select(0, cx);
            }
        })
        .detach();

        Self {
            focus_handle,
            vault_state,
            search,
            selected_ix: 0,
            scroll_handle: ScrollHandle::new(),
            mode,
            has_active_server,
            active_server_id,
            connected_server_ids,
            shown_count: 0,
            shown_sections: Vec::new(),
        }
    }

    fn query(&self, cx: &App) -> String {
        self.search
            .read(cx)
            .value()
            .to_string()
            .to_lowercase()
            .trim()
            .to_string()
    }

    /// The address in the search box, if it reads as one. Parsed from the raw
    /// text: `query()` lowercases, and user names are case-sensitive.
    fn connect_row(&self, cx: &App) -> Option<ConnectRow> {
        let target = parse_ssh_target(&self.search.read(cx).value())?;
        let entries = self
            .vault_state
            .read(cx)
            .manager()
            .and_then(|vm| vm.list_entries().ok())
            .unwrap_or_default();
        connect_row_for(target, &entries, local_user_name())
    }

    fn host_rows(&self, cx: &App) -> Vec<HostRow> {
        let entries: Vec<ServerEntrySummary> = self
            .vault_state
            .read(cx)
            .manager()
            .and_then(|vm| vm.list_entries().ok())
            .unwrap_or_default();
        let q = self.query(cx);
        entries
            .into_iter()
            .filter(|e| host_matches(e, &q))
            .map(|e| {
                let label = if e.label.is_empty() {
                    e.host.clone()
                } else {
                    e.label
                };
                // Port 22 is implied, as in the host list.
                let sub = if e.port == 22 {
                    format!("{}@{}", e.username, e.host)
                } else {
                    format!("{}@{}:{}", e.username, e.host, e.port)
                };
                let is_connected = self.connected_server_ids.contains(&e.id.to_string());
                HostRow {
                    id: e.id,
                    label,
                    sub,
                    os: e.os.filter(|os| !os.trim().is_empty()),
                    is_connected,
                }
            })
            .collect()
    }

    /// Snippets matching the query, ranked most-relevant first (snippets run on
    /// the active server ahead of the rest; never-used fall back to last-edited).
    fn ranked_snippets(&self, cx: &App) -> Vec<Snippet> {
        let all: Vec<Snippet> = self
            .vault_state
            .read(cx)
            .manager()
            .and_then(|vm| vm.list_snippets().ok())
            .unwrap_or_default();
        let q = self.query(cx);
        let mut name_hits = Vec::new();
        let mut desc_hits = Vec::new();
        for s in all {
            if q.is_empty() || s.name.to_lowercase().contains(&q) {
                name_hits.push(s);
            } else if s
                .description
                .as_deref()
                .map(|d| d.to_lowercase().contains(&q))
                .unwrap_or(false)
            {
                desc_hits.push(s);
            }
        }
        let active = self.active_server_id;
        let by_recency =
            |a: &Snippet, b: &Snippet| snippet_rank(a, active).cmp(&snippet_rank(b, active));
        name_hits.sort_by(by_recency);
        desc_hits.sort_by(by_recency);
        name_hits.extend(desc_hits);
        name_hits
    }

    /// True when this snippet has been run on the currently-active server.
    fn is_recent_on_active(&self, s: &Snippet) -> bool {
        match self.active_server_id {
            Some(srv) => s.usages.iter().any(|u| u.server_id == srv),
            None => false,
        }
    }

    /// The items in view order, and for each section its label and the index
    /// of its first item.
    fn items(&self, cx: &App) -> (Vec<PaletteItem>, Vec<(SharedString, usize)>) {
        match &self.mode {
            Mode::Main => {
                let hosts = self.host_rows(cx);
                // Snippets used on the active server get their own "Recent"
                // section at the top; the overflow and everything else drop into
                // the regular "Snippets" section below the hosts.
                let (mut recent, mut others): (Vec<Snippet>, Vec<Snippet>) = self
                    .ranked_snippets(cx)
                    .into_iter()
                    .partition(|s| self.is_recent_on_active(s));
                if recent.len() > RECENT_CAP {
                    let overflow = recent.split_off(RECENT_CAP);
                    others.splice(0..0, overflow);
                }

                let mut items: Vec<PaletteItem> = Vec::new();
                let mut sections: Vec<(SharedString, usize)> = Vec::new();
                let (connect_first, connect_last) = match self.connect_row(cx) {
                    Some(row) if row.is_user_guessed => (None, Some(row)),
                    row => (row, None),
                };
                if let Some(row) = connect_first {
                    sections.push((SharedString::from("Connect"), items.len()));
                    items.push(PaletteItem::Connect(row));
                }
                if !recent.is_empty() {
                    sections.push((SharedString::from("Recent"), items.len()));
                    items.extend(recent.into_iter().map(snippet_to_item));
                }
                if !hosts.is_empty() {
                    sections.push((SharedString::from("Hosts"), items.len()));
                    items.extend(hosts.into_iter().map(PaletteItem::Host));
                }
                if !others.is_empty() {
                    sections.push((SharedString::from("Snippets"), items.len()));
                    items.extend(others.into_iter().map(snippet_to_item));
                }
                let commands = matching_commands(&self.query(cx), self.active_server_id.is_some());
                if !commands.is_empty() {
                    sections.push((SharedString::from("Commands"), items.len()));
                    items.extend(commands.into_iter().map(PaletteItem::Command));
                }
                if let Some(row) = connect_last {
                    sections.push((SharedString::from("Connect"), items.len()));
                    items.push(PaletteItem::Connect(row));
                }
                (items, sections)
            }
            Mode::PickServerForSnippet { .. } => {
                let hosts = self.host_rows(cx);
                let sections = if hosts.is_empty() {
                    vec![]
                } else {
                    vec![(SharedString::from("Hosts"), 0)]
                };
                let items = hosts.into_iter().map(PaletteItem::Host).collect();
                (items, sections)
            }
            Mode::PickLeftSource { session_server_id } => {
                let items =
                    left_source_items(self.host_rows(cx), *session_server_id, &self.query(cx));
                let sections = if items.is_empty() {
                    vec![]
                } else {
                    vec![(SharedString::from("Left pane"), 0)]
                };
                (items, sections)
            }
        }
    }

    fn handle_close(&mut self, _: &Close, window: &mut Window, cx: &mut Context<Self>) {
        match self.mode {
            Mode::PickServerForSnippet { .. } => {
                self.mode = Mode::Main;
                self.search.update(cx, |s, cx| s.set_value("", window, cx));
                self.select(0, cx);
            }
            Mode::Main | Mode::PickLeftSource { .. } => cx.emit(CommandPaletteEvent::Close),
        }
    }

    /// Move the selection and scroll the list so the row stays in view.
    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.selected_ix = ix;
        self.scroll_handle
            .scroll_to_item(list_child_index(ix, &self.shown_sections));
        cx.notify();
    }

    fn handle_up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_ix > 0 {
            self.select(self.selected_ix - 1, cx);
        }
    }

    fn handle_down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_ix + 1 < self.shown_count {
            self.select(self.selected_ix + 1, cx);
        }
    }

    fn handle_activate(&mut self, _: &Activate, window: &mut Window, cx: &mut Context<Self>) {
        self.activate_index(self.selected_ix, window, cx);
    }

    /// Activate the row at `ix`. A click and Enter both come through here, so
    /// both act on the same full item.
    fn activate_index(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let (items, _) = self.items(cx);
        let Some(item) = items.into_iter().nth(ix) else {
            return;
        };
        self.activate(item, window, cx);
    }

    fn activate(&mut self, item: PaletteItem, window: &mut Window, cx: &mut Context<Self>) {
        match (&self.mode, item) {
            (Mode::Main, PaletteItem::Connect(row)) => {
                cx.emit(CommandPaletteEvent::QuickConnect(row.target));
            }
            (Mode::Main, PaletteItem::Host(h)) => {
                cx.emit(CommandPaletteEvent::OpenServer(h.id));
            }
            (Mode::Main, PaletteItem::Snippet(s)) => {
                if self.has_active_server {
                    cx.emit(CommandPaletteEvent::RunSnippet {
                        snippet_id: s.id,
                        server_id: None,
                    });
                } else {
                    self.mode = Mode::PickServerForSnippet {
                        snippet_id: s.id,
                        snippet_name: s.name,
                    };
                    self.search
                        .update(cx, |st, cx| st.set_value("", window, cx));
                    self.select(0, cx);
                }
            }
            (Mode::Main, PaletteItem::Command(spec)) => {
                cx.emit(CommandPaletteEvent::RunCommand(spec.command));
            }
            (Mode::PickServerForSnippet { snippet_id, .. }, PaletteItem::Host(h)) => {
                cx.emit(CommandPaletteEvent::RunSnippet {
                    snippet_id: *snippet_id,
                    server_id: Some(h.id),
                });
            }
            (Mode::PickLeftSource { .. }, PaletteItem::ThisComputer) => {
                cx.emit(CommandPaletteEvent::LeftSourcePicked(None));
            }
            (Mode::PickLeftSource { .. }, PaletteItem::Host(h)) => {
                cx.emit(CommandPaletteEvent::LeftSourcePicked(Some(h.id)));
            }
            // Not listed in these modes; nothing to do if one gets here.
            (
                Mode::PickServerForSnippet { .. },
                PaletteItem::Snippet(_)
                | PaletteItem::Connect(_)
                | PaletteItem::Command(_)
                | PaletteItem::ThisComputer,
            )
            | (
                Mode::PickLeftSource { .. },
                PaletteItem::Snippet(_) | PaletteItem::Connect(_) | PaletteItem::Command(_),
            )
            | (Mode::Main, PaletteItem::ThisComputer) => {}
        }
    }
}

impl EventEmitter<CommandPaletteEvent> for CommandPalette {}

impl Focusable for CommandPalette {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for CommandPalette {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let bg_float = flint_theme::pop_surface();
        let border = flint_theme::border();
        let border_focused = flint_theme::float_border();
        let text = theme.foreground;
        let text_muted = theme.muted_foreground;
        let scrim_bg = theme.overlay;

        let (items, sections) = self.items(cx);
        let total = items.len();
        self.shown_count = total;
        self.shown_sections = sections.clone();
        let selected = self.selected_ix.min(total.saturating_sub(1));

        // Banner shown above the input in the picking modes.
        let banner_text: Option<(String, Option<&'static str>)> = match &self.mode {
            Mode::PickServerForSnippet { snippet_name, .. } => Some((
                format!("Choose a host to run \u{201C}{}\u{201D}", snippet_name),
                Some("esc to go back"),
            )),
            Mode::PickLeftSource { .. } => {
                Some(("Choose what the left file pane shows".to_string(), None))
            }
            Mode::Main => None,
        };
        let banner: Option<AnyElement> = banner_text.map(|(text, escape_hint)| {
            h_flex()
                .items_center()
                .gap(px(6.))
                .h(px(32.))
                .px(px(14.))
                .border_b_1()
                .border_color(border)
                .text_size(px(12.))
                .text_color(text_muted)
                .child(div().child(text))
                .child(div().flex_1())
                .when_some(escape_hint, |el, hint| {
                    el.child(div().text_color(flint_theme::text_dim()).child(hint))
                })
                .into_any_element()
        });

        let mut body_rows: Vec<AnyElement> = Vec::new();
        let mut next_section = 0usize;
        for (ix, item) in items.into_iter().enumerate() {
            if next_section < sections.len() && sections[next_section].1 == ix {
                let label = sections[next_section].0.clone();
                body_rows.push(
                    div()
                        .h(px(28.))
                        .flex()
                        .items_end()
                        .pb(px(5.))
                        .px(px(8.))
                        .text_size(px(11.5))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(flint_theme::text_dim())
                        .child(label)
                        .into_any_element(),
                );
                next_section += 1;
            }
            let is_selected = ix == selected;
            body_rows.push(render_palette_row(ix, item, is_selected, cx));
        }

        let empty = body_rows.is_empty();
        let body: AnyElement = if empty {
            let q = self.search.read(cx).value().to_string();
            div()
                .h(px(44.))
                .flex()
                .items_center()
                .px(px(16.))
                .text_size(px(13.))
                .text_color(flint_theme::text_dim())
                .child(if q.is_empty() {
                    "No saved hosts".to_string()
                } else {
                    format!("No matches for \u{201C}{}\u{201D}", q)
                })
                .into_any_element()
        } else {
            div()
                .id("palette-list")
                .track_scroll(&self.scroll_handle)
                .overflow_y_scroll()
                .max_h(px(380.))
                .pb(px(6.))
                .px(px(6.))
                .children(body_rows)
                .into_any_element()
        };

        div()
            .key_context("CommandPalette")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::handle_close))
            .on_action(cx.listener(Self::handle_up))
            .on_action(cx.listener(Self::handle_down))
            .on_action(cx.listener(Self::handle_activate))
            .occlude()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .bg(scrim_bg)
            .flex()
            .justify_center()
            .items_start()
            .pt(px(64.))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.emit(CommandPaletteEvent::Close)),
            )
            .child(
                v_flex()
                    .w(px(600.))
                    .max_w(px(600.))
                    .bg(bg_float)
                    .border_1()
                    .border_color(border_focused)
                    .rounded(px(12.))
                    .shadow(crate::ui::modal_shadow())
                    .occlude()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .children(banner)
                    .child(
                        h_flex()
                            .items_center()
                            .h(px(48.))
                            .gap(px(10.))
                            .px(px(16.))
                            .border_b_1()
                            .border_color(border)
                            .child(
                                Icon::new(IconName::Search)
                                    .size(px(16.))
                                    .text_color(text_muted),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .child(Input::new(&self.search).appearance(false)),
                            )
                            // Input paints typed text in the inherited colour; the
                            // placeholder uses muted_foreground.
                            .text_color(text),
                    )
                    .child(body),
            )
    }
}

fn local_user_name() -> Option<String> {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .ok()
        .filter(|name| !name.is_empty())
}

/// Fill in the user of a typed address. `ssh name` with no user means the
/// saved host of that name if there is one, else the local user, like ssh.
fn connect_row_for(
    mut target: SshTarget,
    entries: &[ServerEntrySummary],
    local_user: Option<String>,
) -> Option<ConnectRow> {
    if let Some(user) = &target.user {
        let is_saved =
            find_saved_host(entries, user, &target.host, target.port.unwrap_or(22)).is_some();
        return Some(ConnectRow {
            target,
            is_saved,
            is_user_guessed: false,
        });
    }
    if let Some(saved) = find_saved_host_by_name(entries, None, &target.host, target.port) {
        target.user = Some(saved.username.clone());
        target.host = saved.host.clone();
        target.port = Some(saved.port);
        return Some(ConnectRow {
            target,
            is_saved: true,
            is_user_guessed: false,
        });
    }
    let user = local_user?;
    let is_saved =
        find_saved_host(entries, &user, &target.host, target.port.unwrap_or(22)).is_some();
    target.user = Some(user);
    Some(ConnectRow {
        target,
        is_saved,
        is_user_guessed: !is_saved,
    })
}

/// What the Connect row says next to the address: the jump host and any
/// option that is not applied, so nothing is dropped without a word.
fn connect_row_note(row: &ConnectRow) -> String {
    if row.is_saved {
        return "already saved".to_string();
    }
    let mut notes: Vec<String> = Vec::new();
    if let Some(jump) = &row.target.jump {
        notes.push(format!("via {jump}"));
    }
    if !row.target.ignored_options.is_empty() {
        notes.push(format!("ignores {}", row.target.ignored_options.join(" ")));
    }
    notes.push("saves the host, signs in with the SSH agent".to_string());
    notes.join(" \u{00B7} ")
}

/// Same fields the hosts list searches, so both find the same hosts.
fn host_matches(entry: &ServerEntrySummary, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let hay = format!(
        "{} {} {} {} {} {}",
        entry.label,
        entry.host,
        entry.username,
        entry.port,
        entry.group_name.as_deref().unwrap_or_default(),
        entry.tag_names.join(" "),
    );
    hay.to_lowercase().contains(query)
}

/// Commands whose label or keywords contain every word of the lowercased
/// query. Word by word, so "theme dark" finds "Theme: Dark" despite the colon.
fn matching_commands(query: &str, has_session_tab: bool) -> Vec<&'static CommandSpec> {
    COMMANDS
        .iter()
        .filter(|spec| has_session_tab || !spec.needs_session)
        .filter(|spec| {
            let hay = format!("{} {}", spec.label.to_lowercase(), spec.keywords);
            query.split_whitespace().all(|word| hay.contains(word))
        })
        .collect()
}

const THIS_COMPUTER_LABEL: &str = "This computer";

/// The rows of the left-source picker: this computer first, then every host
/// but the session's own, which is already in the right pane. `hosts` is
/// already filtered by the lowercased `query`.
fn left_source_items(
    hosts: Vec<HostRow>,
    session_server_id: Uuid,
    query: &str,
) -> Vec<PaletteItem> {
    let is_local_matching = query
        .split_whitespace()
        .all(|word| "this computer local".contains(word));
    is_local_matching
        .then_some(PaletteItem::ThisComputer)
        .into_iter()
        .chain(
            hosts
                .into_iter()
                .filter(|host| host.id != session_server_id)
                .map(PaletteItem::Host),
        )
        .collect()
}

/// Position of a row among the list's children. Section headers are children
/// of the same list, so every header up to the row shifts it by one.
fn list_child_index(selected_ix: usize, sections: &[(SharedString, usize)]) -> usize {
    if selected_ix == 0 {
        // Scroll to the first header, not past it.
        return 0;
    }
    let headers_before = sections
        .iter()
        .filter(|(_, start)| *start <= selected_ix)
        .count();
    selected_ix + headers_before
}

fn command_icon(command: PaletteCommand) -> Icon {
    let name = match command {
        PaletteCommand::NewHost => IconName::Plus,
        PaletteCommand::OpenSettings => IconName::Settings,
        PaletteCommand::SetTheme(_) => IconName::Palette,
        PaletteCommand::LockVault => return Icon::empty().path("icons/lock.svg"),
        PaletteCommand::CloseSession => IconName::Close,
        PaletteCommand::NewTerminal => IconName::SquareTerminal,
        PaletteCommand::ToggleSftpPane
        | PaletteCommand::FocusFiles
        | PaletteCommand::ChooseLeftSource => IconName::FolderOpen,
        PaletteCommand::ReconnectSession => return Icon::empty().path("icons/rotate-cw.svg"),
        PaletteCommand::ImportHosts => return Icon::empty().path("icons/download.svg"),
    };
    Icon::new(name)
}

fn snippet_to_item(s: Snippet) -> PaletteItem {
    PaletteItem::Snippet(SnippetRow {
        id: s.id,
        name: s.name,
        description: s.description,
    })
}

/// Sort key ordering snippets by usefulness: run-on-active-server first (most
/// recent first), then run-on-any-server, then never-run by last-edited time.
/// Smaller sorts earlier, so recency is stored negated.
fn snippet_rank(s: &Snippet, active: Option<Uuid>) -> (u8, i64) {
    if let Some(srv) = active {
        if let Some(u) = s.usages.iter().find(|u| u.server_id == srv) {
            return (0, -u.at);
        }
    }
    if let Some(max_at) = s.usages.iter().map(|u| u.at).max() {
        return (1, -max_at);
    }
    (2, -s.updated_at)
}

/// Every row's leading slot is this wide, so host tiles and bare icons line
/// up in one column.
const LEAD_SIZE: f32 = 24.;

fn render_palette_row(
    ix: usize,
    item: PaletteItem,
    is_selected: bool,
    cx: &mut Context<CommandPalette>,
) -> AnyElement {
    let row = h_flex()
        .id(("palette-row", ix))
        .h(px(36.))
        .items_center()
        .gap(px(10.))
        .px(px(8.))
        .rounded(px(7.))
        .cursor_pointer()
        .when(is_selected, |el| el.bg(flint_theme::accent_soft()))
        .when(!is_selected, |el| el.hover(|s| s.bg(flint_theme::field())))
        .active(|s| s.bg(flint_theme::element_active()))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, _, cx| {
                this.selected_ix = ix;
                cx.notify();
            }),
        )
        .on_click(cx.listener(move |this, _, window, cx| this.activate_index(ix, window, cx)));

    let (lead, title, note, is_note_mono, shortcut): (
        AnyElement,
        SharedString,
        SharedString,
        bool,
        Option<&'static str>,
    ) = match item {
        PaletteItem::Connect(connect) => (
            lead_icon(Icon::new(IconName::ArrowRight), flint_theme::accent()),
            format!("Connect to {}", connect.target.address()).into(),
            connect_row_note(&connect).into(),
            false,
            None,
        ),
        PaletteItem::Host(h) => (
            host_icon(
                h.os.as_deref(),
                h.is_connected,
                flint_theme::pop_surface(),
                LEAD_SIZE,
            ),
            h.label.into(),
            h.sub.into(),
            true,
            None,
        ),
        PaletteItem::Snippet(s) => (
            lead_icon(Icon::new(IconName::SquareTerminal), flint_theme::cyan()),
            s.name.into(),
            s.description.unwrap_or_default().into(),
            false,
            None,
        ),
        PaletteItem::ThisComputer => (
            lead_icon(
                Icon::empty().path(file_pane::source_icon(PaneKind::Local)),
                flint_theme::text_dim(),
            ),
            THIS_COMPUTER_LABEL.into(),
            "local files".into(),
            false,
            None,
        ),
        PaletteItem::Command(spec) => {
            let is_current_theme =
                spec.command == PaletteCommand::SetTheme(cx.global::<Preferences>().theme);
            let note = if is_current_theme {
                "current"
            } else {
                spec.note
            };
            (
                lead_icon(command_icon(spec.command), flint_theme::text_dim()),
                spec.label.into(),
                note.into(),
                false,
                spec.shortcut,
            )
        }
    };

    row.child(lead)
        .child(
            h_flex()
                .flex_1()
                .min_w_0()
                .items_baseline()
                .gap(px(10.))
                .child(
                    div()
                        .flex_shrink_0()
                        .max_w(px(320.))
                        .text_size(px(13.))
                        .text_color(cx.theme().foreground)
                        .truncate()
                        .child(title),
                )
                .when(!note.is_empty(), |el| {
                    el.child(
                        div()
                            .min_w_0()
                            .text_size(px(12.))
                            .text_color(flint_theme::text_dim())
                            .when(is_note_mono, |el| el.font_family(flint_theme::MONO_FONT))
                            .truncate()
                            .child(note),
                    )
                }),
        )
        .when_some(shortcut, |el, shortcut| el.child(key_cap(shortcut)))
        // What Enter does is shown on the row it would act on.
        .when(is_selected && shortcut.is_none(), |el| {
            el.child(key_cap("\u{21B5}"))
        })
        .into_any_element()
}

/// A bare icon centred in the leading slot.
fn lead_icon(icon: Icon, color: gpui::Hsla) -> AnyElement {
    div()
        .flex_none()
        .size(px(LEAD_SIZE))
        .flex()
        .items_center()
        .justify_center()
        .child(icon.size(px(15.)).text_color(color))
        .into_any_element()
}

/// A key or a chord, drawn as a small key cap.
fn key_cap(keys: &'static str) -> impl IntoElement {
    div()
        .flex_none()
        .h(px(20.))
        .px(px(6.))
        .flex()
        .items_center()
        .rounded(px(5.))
        .bg(flint_theme::field())
        .text_size(px(11.))
        .text_color(flint_theme::text_dim())
        .child(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(input: &str, is_saved: bool) -> ConnectRow {
        ConnectRow {
            target: parse_ssh_target(input).unwrap(),
            is_saved,
            is_user_guessed: false,
        }
    }

    fn saved(label: &str, user: &str, host: &str, port: u16) -> ServerEntrySummary {
        ServerEntrySummary {
            id: Uuid::new_v4(),
            label: label.to_string(),
            host: host.to_string(),
            port,
            username: user.to_string(),
            os: None,
            last_opened_at: None,
            group_name: None,
            tag_names: Vec::new(),
            team_name: None,
            proxy: crate::vault::ProxyChoice::Default,
        }
    }

    fn row_for(input: &str, entries: &[ServerEntrySummary]) -> ConnectRow {
        connect_row_for(
            parse_ssh_target(input).unwrap(),
            entries,
            Some("me".to_string()),
        )
        .unwrap()
    }

    #[test]
    fn a_typed_user_is_kept_and_is_not_a_guess() {
        let entries = vec![saved("prod", "deploy", "10.0.0.5", 22)];
        let new = row_for("Root@10.0.0.5", &entries);
        assert_eq!(new.target.user.as_deref(), Some("Root"));
        assert!(!new.is_saved && !new.is_user_guessed);
        let known = row_for("deploy@10.0.0.5", &entries);
        assert!(known.is_saved && !known.is_user_guessed);
    }

    #[test]
    fn a_bare_name_opens_the_saved_host_of_that_name() {
        let entries = vec![saved("prod", "deploy", "10.0.0.5", 2222)];
        let known = row_for("ssh prod", &entries);
        assert_eq!(known.target.address(), "deploy@10.0.0.5:2222");
        assert!(known.is_saved && !known.is_user_guessed);
    }

    #[test]
    fn a_bare_unknown_name_falls_back_to_the_local_user_as_a_guess() {
        let guess = row_for("ssh restart", &[]);
        assert_eq!(guess.target.address(), "me@restart:22");
        assert!(guess.is_user_guessed);
        let target = parse_ssh_target("ssh restart").unwrap();
        assert!(connect_row_for(target, &[], None).is_none());
    }

    fn command_labels(query: &str, has_session_tab: bool) -> Vec<&'static str> {
        matching_commands(query, has_session_tab)
            .iter()
            .map(|spec| spec.label)
            .collect()
    }

    #[test]
    fn commands_match_by_label_or_keyword() {
        assert_eq!(command_labels("settings", false), ["Settings"]);
        assert_eq!(command_labels("preferences", false), ["Settings"]);
        assert_eq!(command_labels("new host", false), ["New host"]);
        assert_eq!(command_labels("import", false), ["Import hosts"]);
        assert_eq!(
            command_labels("theme", false),
            ["Theme: Dark", "Theme: Light", "Theme: System"]
        );
        assert_eq!(command_labels("light", false), ["Theme: Light"]);
        assert_eq!(command_labels("theme dark", false), ["Theme: Dark"]);
        assert_eq!(command_labels("dark theme", false), ["Theme: Dark"]);
        assert!(command_labels("theme host", false).is_empty());
        assert!(command_labels("no such thing", true).is_empty());
    }

    #[test]
    fn the_lock_command_is_found_by_its_own_words_only() {
        for query in ["lock", "vault", "lock vault", "vault lock"] {
            assert_eq!(command_labels(query, false), ["Lock vault"], "{query:?}");
        }
        for query in ["prod", "unlock"] {
            assert!(command_labels(query, false).is_empty(), "{query:?}");
        }
    }

    #[test]
    fn session_commands_need_a_session_tab() {
        assert!(command_labels("session", false).is_empty());
        assert_eq!(
            command_labels("session", true),
            ["Close session", "Reconnect session"]
        );
        assert!(command_labels("terminal", false).is_empty());
        assert_eq!(
            command_labels("terminal", true),
            ["New terminal", "Show or hide files"]
        );
        assert_eq!(
            command_labels("sftp", true),
            ["Show or hide files", "Focus files"]
        );
        assert_eq!(command_labels("", false).len(), 7);
        assert_eq!(command_labels("", true).len(), COMMANDS.len());
    }

    fn host_row(label: &str) -> HostRow {
        HostRow {
            id: Uuid::new_v4(),
            label: label.to_string(),
            sub: String::new(),
            os: None,
            is_connected: false,
        }
    }

    fn item_labels(items: &[PaletteItem]) -> Vec<&str> {
        items
            .iter()
            .map(|item| match item {
                PaletteItem::ThisComputer => THIS_COMPUTER_LABEL,
                PaletteItem::Host(host) => host.label.as_str(),
                _ => "unexpected",
            })
            .collect()
    }

    #[test]
    fn left_source_picker_lists_this_computer_first_and_not_the_own_host() {
        let own = host_row("web-01");
        let own_id = own.id;
        let items = left_source_items(vec![own, host_row("web-02"), host_row("db")], own_id, "");
        assert_eq!(item_labels(&items), ["This computer", "web-02", "db"]);
    }

    #[test]
    fn left_source_picker_filters_this_computer_by_the_query() {
        let own_id = Uuid::new_v4();
        // `hosts` arrives already filtered by the query.
        let items = left_source_items(vec![host_row("web-02")], own_id, "web");
        assert_eq!(item_labels(&items), ["web-02"]);
        for query in ["local", "this", "comp", "this computer"] {
            let items = left_source_items(Vec::new(), own_id, query);
            assert_eq!(item_labels(&items), ["This computer"], "{query:?}");
        }
    }

    #[test]
    fn the_left_source_command_needs_a_session_tab() {
        assert!(command_labels("left pane", false).is_empty());
        assert_eq!(
            command_labels("left pane", true),
            ["Left pane: choose source\u{2026}"]
        );
        assert_eq!(
            command_labels("another server", true),
            ["Left pane: choose source\u{2026}"]
        );
    }

    #[test]
    fn hosts_match_by_folder_and_tag() {
        let mut entry = saved("db", "deploy", "10.0.0.5", 22);
        entry.group_name = Some("Clients / Acme".to_string());
        entry.tag_names = vec!["Production".to_string()];
        assert!(host_matches(&entry, "acme"));
        assert!(host_matches(&entry, "prod"));
        assert!(host_matches(&entry, "10.0.0"));
        assert!(host_matches(&entry, ""));
        assert!(!host_matches(&entry, "staging"));
    }

    #[test]
    fn list_child_index_counts_the_headers_above_the_row() {
        let sections = vec![
            (SharedString::from("Hosts"), 0),
            (SharedString::from("Commands"), 3),
        ];
        assert_eq!(list_child_index(0, &sections), 0);
        assert_eq!(list_child_index(1, &sections), 2);
        assert_eq!(list_child_index(2, &sections), 3);
        assert_eq!(list_child_index(3, &sections), 5);
        assert_eq!(list_child_index(4, &sections), 6);
    }

    #[test]
    fn connect_note_says_what_enter_will_do() {
        assert_eq!(
            connect_row_note(&row("deploy@web", false)),
            "saves the host, signs in with the SSH agent",
        );
        assert_eq!(connect_row_note(&row("deploy@web", true)), "already saved");
    }

    #[test]
    fn connect_note_names_the_jump_host_and_ignored_options() {
        assert_eq!(
            connect_row_note(&row("ssh -i key -J bastion deploy@web", false)),
            "via bastion \u{00B7} ignores -i \u{00B7} saves the host, signs in with the SSH agent",
        );
    }
}
