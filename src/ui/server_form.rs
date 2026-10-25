use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    canvas, deferred, div, point, px, svg, Anchor, AnyElement, App, Bounds, Context, ElementId,
    Entity, EventEmitter, FocusHandle, Focusable, Hsla, KeyDownEvent, MouseButton, MouseDownEvent,
    Pixels, Point, SharedString, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    dialog::DialogHeader,
    h_flex,
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    radio::{Radio, RadioGroup},
    v_flex, ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _,
};
use uuid::Uuid;

use crate::ui::context_menu::AnchoredToTrigger;
use crate::ui::dialog_max_height;

use crate::connection::{is_loopback_bind, ForwardKind};
use crate::ssh_target::{find_saved_jump_host, parse_host_field};
use crate::ui::VaultState;
use crate::vault::{
    AuthMethod, HostSnippet, PortForward, ServerEntry, ServerEntrySummary, VaultError,
};

pub enum FormMode {
    Add,
    Edit { id: Uuid, created_at: i64 },
}

// Boxing would move the entry's credentials onto the heap for no real gain:
// the event is emitted once per save.
#[allow(clippy::large_enum_variant)]
pub enum FormEvent {
    Saved(ServerEntry),
    Cancelled,
}

#[derive(Clone, Copy, PartialEq)]
enum AuthMode {
    Password,
    PrivateKey,
    Agent,
}

/// What is sent to the shell after every connect.
#[derive(Clone, Copy, PartialEq)]
enum Startup {
    None,
    /// Typed in the form, kept as one of the host's own snippets.
    Command,
    Shared(Uuid),
}

#[derive(Clone, PartialEq)]
enum FormState {
    Main,
    AddingKey,
    EditingForward,
}

#[derive(Clone, PartialEq)]
enum PemSource {
    File,
    Paste,
}

pub struct ServerFormView {
    focus_handle: FocusHandle,
    mode: FormMode,
    form_state: FormState,

    auth_mode: AuthMode,

    name_input: Entity<InputState>,
    host_input: Entity<InputState>,
    /// Host text as of the last change, to tell a paste from typing.
    last_host_value: String,
    port_input: Entity<InputState>,
    user_input: Entity<InputState>,
    pass_input: Entity<InputState>,

    vault_state: Entity<VaultState>,
    selected_key_id: Option<Uuid>,
    key_dropdown_open: bool,

    /// Host this one is tunnelled through; `None` = connect directly.
    selected_jump_id: Option<Uuid>,
    jump_dropdown_open: bool,

    startup: Startup,
    startup_dropdown_open: bool,
    startup_command: Entity<TextareaState>,
    /// The host's own snippet the command edits; `None` makes a new one.
    startup_command_id: Option<Uuid>,
    /// Carried through, so that a save keeps the ones made in the server panel.
    host_snippets: Vec<HostSnippet>,
    /// The jump host, the startup snippet and the forwards are folded away
    /// unless the host uses one of them.
    is_advanced_open: bool,

    /// Folder path as typed/picked ("infra / web"); empty = ungrouped.
    /// Resolved to group ids on submit, creating missing folders.
    group_input: Entity<InputState>,
    /// Committed tag chips. The input below them holds the not-yet-committed
    /// text; Enter or a suggestion click moves it here.
    tag_names: Vec<String>,
    tag_input: Entity<InputState>,
    /// Keyboard-highlighted row in the group / tag suggestion popups. `None` means
    /// the list isn't entered: the user is still typing. Index into
    /// [`Self::group_suggestions`] / [`Self::tag_suggestions`].
    group_highlight: Option<usize>,
    tag_highlight: Option<usize>,

    // Mirrors the add-key form in `settings::view`; worth extracting once a third
    // copy shows up.
    addkey_name: Entity<InputState>,
    addkey_pem_source: PemSource,
    addkey_file_path: Option<PathBuf>,
    addkey_paste: Entity<TextareaState>,
    addkey_passphrase: Entity<InputState>,
    /// The selected/pasted key is encrypted, so the passphrase field is shown
    /// and required. It stays hidden for unencrypted keys.
    addkey_encrypted: bool,
    addkey_error: Option<String>,

    /// Port forwards for this host, edited here and saved with the entry.
    /// A forward is meaningless without the host it tunnels through.
    forwards: Vec<PortForward>,
    /// Row being edited in the forward editor; `None` while adding a new one.
    editing_forward: Option<usize>,
    forward_kind: ForwardKind,
    forward_name: Entity<InputState>,
    forward_bind_port: Entity<InputState>,
    forward_target_host: Entity<InputState>,
    forward_target_port: Entity<InputState>,
    /// Listen on every interface instead of loopback. Off unless the user
    /// clicks it: it opens the tunnel to everyone on the network.
    forward_bind_public: bool,
    forward_auto_start: bool,
    forward_error: Option<String>,

    error: Option<String>,
    /// Something the user should know before saving that is not a mistake.
    warning: Option<String>,
}

impl EventEmitter<FormEvent> for ServerFormView {}

impl Focusable for ServerFormView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ServerFormView {
    pub fn new(
        mode: FormMode,
        prefill: Option<ServerEntry>,
        vault_state: Entity<VaultState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&vault_state, |this, _, cx| {
            if matches!(this.auth_mode, AuthMode::PrivateKey) && this.selected_key_id.is_none() {
                let keys = this
                    .vault_state
                    .read(cx)
                    .manager()
                    .and_then(|vm| vm.list_keys().ok())
                    .unwrap_or_default();
                if let Some(latest) = keys.into_iter().max_by_key(|k| k.created_at) {
                    this.selected_key_id = Some(latest.id);
                }
            }
            cx.notify();
        })
        .detach();

        let (label, host, port, user, password, auth_mode, selected_key_id) = match &prefill {
            None => (
                String::new(),
                String::new(),
                "22".to_string(),
                String::new(),
                String::new(),
                AuthMode::Password,
                None,
            ),
            Some(e) => {
                let (am, pw, kid) = match &e.auth {
                    AuthMethod::Password(p) => (AuthMode::Password, p.clone(), None),
                    AuthMethod::PrivateKey { key_id } => {
                        (AuthMode::PrivateKey, String::new(), Some(*key_id))
                    }
                    AuthMethod::Agent => (AuthMode::Agent, String::new(), None),
                };
                // A name that only repeats the address was filled in for a
                // host saved without one: show it empty, so a new address
                // renames the host too.
                let label = if e.label == e.host {
                    String::new()
                } else {
                    e.label.clone()
                };
                (
                    label,
                    e.host.clone(),
                    e.port.to_string(),
                    e.username.clone(),
                    pw,
                    am,
                    kid,
                )
            }
        };

        let host_snippets = prefill
            .as_ref()
            .map(|e| e.snippets.clone())
            .unwrap_or_default();
        let startup_id = prefill.as_ref().and_then(|e| e.startup_snippet);
        let own_startup = startup_id.and_then(|id| host_snippets.iter().find(|s| s.id == id));
        let startup = match (startup_id, own_startup) {
            (None, _) => Startup::None,
            (Some(_), Some(_)) => Startup::Command,
            (Some(id), None) => Startup::Shared(id),
        };
        let startup_command_id = own_startup.map(|s| s.id);
        let command = own_startup.map(|s| s.script.clone()).unwrap_or_default();
        let startup_command = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(3)
                .placeholder("cd /srv/app")
                .default_value(SharedString::from(command))
        });

        let addkey_paste = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(6)
                .placeholder("-----BEGIN OPENSSH PRIVATE KEY-----")
        });
        // Pasting an encrypted key should reveal the passphrase field at once.
        cx.subscribe(&addkey_paste, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) && this.addkey_pem_source == PemSource::Paste {
                this.refresh_addkey_encrypted(cx);
                cx.notify();
            }
        })
        .detach();

        let name_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Defaults to the host")
                .default_value(SharedString::from(label))
        });
        let host_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("example.com, or paste user@host:port")
                .default_value(SharedString::from(host))
        });

        let port_input =
            cx.new(|cx| InputState::new(window, cx).default_value(SharedString::from(port)));
        let user_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("root")
                .default_value(SharedString::from(user))
        });
        let pass_input = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .default_value(SharedString::from(password))
        });

        for input in [
            &name_input,
            &host_input,
            &port_input,
            &user_input,
            &pass_input,
        ] {
            cx.subscribe(input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. })
                    && matches!(this.form_state, FormState::Main)
                {
                    this.submit(cx);
                }
            })
            .detach();
        }

        cx.subscribe_in(
            &host_input,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.split_pasted_host(window, cx);
                }
            },
        )
        .detach();
        let last_host_value = host_input.read(cx).value().to_string();

        let (group_text, tag_names) = match &prefill {
            None => (String::new(), Vec::new()),
            Some(e) => {
                let vm = vault_state.read(cx).manager();
                let group_text = e
                    .group
                    .and_then(|gid| {
                        vm.and_then(|vm| {
                            vm.list_group_paths()
                                .into_iter()
                                .find(|(id, _)| *id == gid)
                                .map(|(_, path)| path)
                        })
                    })
                    .unwrap_or_default();
                let all_tags = vm.map(|vm| vm.list_tags()).unwrap_or_default();
                let tag_names = e
                    .tags
                    .iter()
                    .filter_map(|tid| all_tags.iter().find(|t| t.id == *tid))
                    .map(|t| t.name.clone())
                    .collect();
                (group_text, tag_names)
            }
        };
        let group_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Type to pick or create a group, use / to nest")
                .default_value(SharedString::from(group_text))
        });
        let tag_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Type a tag and press Enter"));
        // Suggestion lists follow the text live; a new query drops any highlight
        // so the user is back to "just typing" until they press Down again.
        // (Enter / arrow handling lives in the pickers' `capture_key_down`.)
        for input in [&group_input, &tag_input] {
            cx.subscribe(input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.group_highlight = None;
                    this.tag_highlight = None;
                    cx.notify();
                }
            })
            .detach();
        }

        Self {
            focus_handle: cx.focus_handle(),
            mode,
            form_state: FormState::Main,
            auth_mode,
            name_input,
            host_input,
            last_host_value,
            port_input,
            user_input,
            pass_input,
            vault_state,
            selected_key_id,
            key_dropdown_open: false,
            selected_jump_id: prefill.as_ref().and_then(|e| e.jump_host),
            jump_dropdown_open: false,
            startup,
            startup_command,
            startup_command_id,
            host_snippets,
            is_advanced_open: prefill.as_ref().is_some_and(|e| {
                e.jump_host.is_some() || e.startup_snippet.is_some() || !e.forwards.is_empty()
            }),
            startup_dropdown_open: false,
            group_input,
            tag_names,
            tag_input,
            group_highlight: None,
            tag_highlight: None,
            addkey_name: cx
                .new(|cx| InputState::new(window, cx).placeholder("e.g. Production key")),
            addkey_pem_source: PemSource::File,
            addkey_file_path: None,
            addkey_paste,
            addkey_encrypted: false,
            addkey_passphrase: cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder("Passphrase")
            }),
            addkey_error: None,
            forwards: prefill
                .as_ref()
                .map(|e| e.forwards.clone())
                .unwrap_or_default(),
            editing_forward: None,
            forward_kind: ForwardKind::Local,
            forward_name: cx.new(|cx| InputState::new(window, cx).placeholder("e.g. Postgres")),
            forward_bind_port: cx.new(|cx| InputState::new(window, cx).placeholder("8080")),
            forward_target_host: cx
                .new(|cx| InputState::new(window, cx).placeholder("hostname or IP")),
            forward_target_port: cx.new(|cx| InputState::new(window, cx).placeholder("5432")),
            forward_bind_public: false,
            forward_auto_start: false,
            forward_error: None,
            error: None,
            warning: None,
        }
    }

    /// Put the keys in the host field. Called once the dialog is open: the
    /// dialog takes the focus when it opens and gives it back to whatever
    /// had it before, so focusing earlier would hand it to a field that is
    /// gone after the dialog closes.
    pub fn focus_host(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.host_input
            .update(cx, |input, cx| input.focus(window, cx));
    }

    /// Show a message the form could not know about: a vault write the
    /// caller made on our behalf and the vault refused.
    pub fn set_error(&mut self, message: String, cx: &mut Context<Self>) {
        self.error = Some(message);
        self.warning = None;
        cx.notify();
    }

    /// Show a caution about what saving will do. The form is fine as it is;
    /// the caller decides what a second Save means.
    pub fn set_warning(&mut self, message: String, cx: &mut Context<Self>) {
        self.warning = Some(message);
        self.error = None;
        cx.notify();
    }

    /// Pasting `ssh user@host -p 2222` or `user@host:2222` into the host field
    /// moves each part to its own field. Typed text is left alone: splitting
    /// `host:2` mid-word would send the rest of the port into the host name.
    fn split_pasted_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.host_input.read(cx).value().to_string();
        let previous = std::mem::replace(&mut self.last_host_value, value.clone());
        if inserted_char_count(&previous, &value) < 2 {
            return;
        }
        let Some(target) = parse_host_field(&value) else {
            return;
        };

        // `set_value` does not emit `Change`, so this does not run again.
        self.last_host_value = target.host.clone();
        self.host_input.update(cx, |input, cx| {
            input.set_value(target.host.clone(), window, cx)
        });
        if let Some(user) = target.user {
            self.user_input
                .update(cx, |input, cx| input.set_value(user, window, cx));
        }
        if let Some(port) = target.port {
            self.port_input.update(cx, |input, cx| {
                input.set_value(port.to_string(), window, cx)
            });
        }
        if let Some(jump_id) = target.jump.and_then(|jump| self.saved_jump_host(&jump, cx)) {
            self.selected_jump_id = Some(jump_id);
        }
        cx.notify();
    }

    /// The saved host a pasted `-J` value names, if this host may jump through it.
    fn saved_jump_host(&self, jump: &str, cx: &App) -> Option<Uuid> {
        let own_id = match &self.mode {
            FormMode::Add => None,
            FormMode::Edit { id, .. } => Some(*id),
        };
        let candidates = self
            .vault_state
            .read(cx)
            .manager()?
            .list_jump_candidates(own_id);
        find_saved_jump_host(&candidates, jump)
    }

    fn is_valid(&self, cx: &App) -> bool {
        let host = strip_whitespace(&self.host_input.read(cx).value());
        let port_str = strip_whitespace(&self.port_input.read(cx).value());
        let user = strip_whitespace(&self.user_input.read(cx).value());

        if host.is_empty() || user.is_empty() {
            return false;
        }
        if port_str.parse::<u16>().map(|p| p < 1).unwrap_or(true) {
            return false;
        }

        match self.auth_mode {
            AuthMode::Password => !self.pass_input.read(cx).value().is_empty(),
            AuthMode::PrivateKey => self.selected_key_id.is_some(),
            // The agent supplies the keys; nothing to fill in here.
            AuthMode::Agent => true,
        }
    }

    /// Move the tag input's text into the chip list (deduplicated,
    /// case-insensitive) and clear the input.
    fn commit_tag(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.tag_input.read(cx).value().trim().to_string();
        if value.is_empty() {
            return;
        }
        if !self
            .tag_names
            .iter()
            .any(|t| t.eq_ignore_ascii_case(&value))
        {
            self.tag_names.push(value);
        }
        self.tag_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.tag_highlight = None;
        cx.notify();
    }

    /// Existing folder paths matching the typed group text.
    /// Shared by the renderer and the keyboard handler so row indices line up.
    fn group_suggestions(&self, cx: &App) -> Vec<String> {
        let group_text = self.group_input.read(cx).value().trim().to_string();
        self.vault_state
            .read(cx)
            .manager()
            .map(|vm| vm.list_group_paths())
            .unwrap_or_default()
            .into_iter()
            .map(|(_, path)| path)
            .filter(|p| {
                group_text.is_empty() || p.to_lowercase().contains(&group_text.to_lowercase())
            })
            .filter(|p| !p.eq_ignore_ascii_case(&group_text))
            .take(5)
            .collect()
    }

    /// Existing tags matching the typed text and not already on this server.
    /// Shared by the renderer and the keyboard handler so row indices line up.
    fn tag_suggestions(&self, cx: &App) -> Vec<String> {
        let tag_text = self.tag_input.read(cx).value().trim().to_string();
        self.vault_state
            .read(cx)
            .manager()
            .map(|vm| vm.list_tags())
            .unwrap_or_default()
            .into_iter()
            .map(|t| t.name)
            .filter(|t| !self.tag_names.iter().any(|s| s.eq_ignore_ascii_case(t)))
            .filter(|t| tag_text.is_empty() || t.to_lowercase().contains(&tag_text.to_lowercase()))
            .take(5)
            .collect()
    }

    /// Replace the group field with a picked suggestion and leave the list.
    fn set_group(&mut self, value: String, window: &mut Window, cx: &mut Context<Self>) {
        self.group_input.update(cx, |s, cx| {
            s.set_value(SharedString::from(value), window, cx)
        });
        self.group_highlight = None;
        cx.notify();
    }

    /// Add an existing tag as a chip (deduplicated) and clear the input.
    fn add_tag(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.tag_names.iter().any(|t| t.eq_ignore_ascii_case(&name)) {
            self.tag_names.push(name);
        }
        self.tag_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.tag_highlight = None;
        cx.notify();
    }

    /// Keyboard control for the group suggestion popup. Down enters/steps the
    /// list, Up steps back and out the top, Enter accepts the highlighted folder,
    /// Esc leaves the list. Plain typing falls through to the input untouched.
    fn on_group_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let m = &event.keystroke.modifiers;
        if m.control || m.alt || m.platform || m.shift {
            return;
        }
        match event.keystroke.key.as_str() {
            "down" => {
                let suggestions = self.group_suggestions(cx);
                if suggestions.is_empty() {
                    return;
                }
                let next = match self.group_highlight {
                    None => 0,
                    Some(i) => (i + 1).min(suggestions.len() - 1),
                };
                self.group_highlight = Some(next);
                cx.stop_propagation();
                cx.notify();
            }
            "up" => {
                match self.group_highlight {
                    None => return,
                    Some(0) => self.group_highlight = None,
                    Some(i) => self.group_highlight = Some(i - 1),
                }
                cx.stop_propagation();
                cx.notify();
            }
            "enter" | "return" => {
                if let Some(i) = self.group_highlight {
                    if let Some(value) = self.group_suggestions(cx).get(i).cloned() {
                        self.set_group(value, window, cx);
                        cx.stop_propagation();
                    }
                }
            }
            "escape" if self.group_highlight.is_some() => {
                self.group_highlight = None;
                cx.stop_propagation();
                cx.notify();
            }
            _ => {}
        }
    }

    /// Keyboard control for the tag suggestion popup. Down enters/steps the list,
    /// Up steps back and out the top, Enter accepts the highlighted tag (or, with
    /// nothing highlighted, commits the typed text), Esc leaves the list.
    fn on_tag_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let m = &event.keystroke.modifiers;
        if m.control || m.alt || m.platform || m.shift {
            return;
        }
        match event.keystroke.key.as_str() {
            "down" => {
                let suggestions = self.tag_suggestions(cx);
                if suggestions.is_empty() {
                    return;
                }
                let next = match self.tag_highlight {
                    None => 0,
                    Some(i) => (i + 1).min(suggestions.len() - 1),
                };
                self.tag_highlight = Some(next);
                cx.stop_propagation();
                cx.notify();
            }
            "up" => {
                match self.tag_highlight {
                    None => return,
                    Some(0) => self.tag_highlight = None,
                    Some(i) => self.tag_highlight = Some(i - 1),
                }
                cx.stop_propagation();
                cx.notify();
            }
            "enter" | "return" => {
                match self.tag_highlight {
                    Some(i) => {
                        if let Some(name) = self.tag_suggestions(cx).get(i).cloned() {
                            self.add_tag(name, window, cx);
                        }
                    }
                    None => self.commit_tag(window, cx),
                }
                cx.stop_propagation();
            }
            "escape" if self.tag_highlight.is_some() => {
                self.tag_highlight = None;
                cx.stop_propagation();
                cx.notify();
            }
            _ => {}
        }
    }

    /// Resolve the typed group path and tag chips to vault ids, creating
    /// missing folders/tags. "/" in the group field nests folders. Text left
    /// in the tag input counts as one more tag so it isn't lost on save.
    fn resolve_group_and_tags(
        &self,
        cx: &mut Context<Self>,
    ) -> Result<(Option<Uuid>, Vec<Uuid>), VaultError> {
        let group_text = self.group_input.read(cx).value().trim().to_string();
        let mut tag_names = self.tag_names.clone();
        let pending = self.tag_input.read(cx).value().trim().to_string();
        if !pending.is_empty() && !tag_names.iter().any(|t| t.eq_ignore_ascii_case(&pending)) {
            tag_names.push(pending);
        }

        self.vault_state.update(cx, |vs, _| {
            let Some(vm) = vs.manager_mut() else {
                return Ok((None, Vec::new()));
            };
            let mut group: Option<Uuid> = None;
            for segment in group_text
                .split('/')
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                group = Some(vm.find_or_create_group(segment, group)?);
            }
            let mut tags = Vec::new();
            for name in &tag_names {
                tags.push(vm.find_or_create_tag(name)?);
            }
            Ok::<_, VaultError>((group, tags))
        })
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        if !self.is_valid(cx) {
            return;
        }

        // Pasted values often carry invisible whitespace (stray \r, \n, spaces).
        // Host/port/username can never legitimately contain any whitespace, so
        // strip all of it; the display name only gets leading/trailing trimmed.
        let host = strip_whitespace(&self.host_input.read(cx).value());
        // Without a name the host is saved under its address. The vault and
        // its messages expect a name, so it is filled in rather than left
        // empty; the form shows it empty again (see `new`), so it follows the
        // address when that is edited.
        let label = match self.name_input.read(cx).value().trim() {
            "" => host.clone(),
            name => name.to_string(),
        };
        let port: u16 = strip_whitespace(&self.port_input.read(cx).value())
            .parse()
            .unwrap_or(22);
        let username = strip_whitespace(&self.user_input.read(cx).value());

        let auth = match self.auth_mode {
            AuthMode::Password => {
                AuthMethod::Password(self.pass_input.read(cx).value().to_string())
            }
            AuthMode::PrivateKey => {
                let Some(key_id) = self.selected_key_id else {
                    self.error = Some("Select a key or add a new one.".to_string());
                    cx.notify();
                    return;
                };
                AuthMethod::PrivateKey { key_id }
            }
            AuthMode::Agent => AuthMethod::Agent,
        };

        let (group, tags) = match self.resolve_group_and_tags(cx) {
            Ok(resolved) => resolved,
            Err(e) => {
                tracing::error!(?e, "failed to save the host's group or tags");
                self.error = Some(format!("Could not save the group or tags: {e}"));
                cx.notify();
                return;
            }
        };

        let (startup_snippet, snippets) = self.startup_and_snippets(cx);
        let now = unix_now();
        let entry = match &self.mode {
            FormMode::Add => ServerEntry {
                id: Uuid::new_v4(),
                label,
                host,
                port,
                username,
                auth,
                notes: None,
                os: None,
                last_opened_at: None,
                host_key_fingerprint: None,
                last_remote_dir: None,
                last_local_dir: None,
                group,
                tags,
                proxy: crate::vault::ProxyChoice::Default,
                jump_host: self.selected_jump_id,
                forwards: self.forwards.clone(),
                startup_snippet,
                snippets,
                created_at: now,
                updated_at: now,
            },
            FormMode::Edit { id, created_at } => ServerEntry {
                id: *id,
                label,
                host,
                port,
                username,
                auth,
                notes: None,
                // Carried over from the stored entry on save (see app_view).
                os: None,
                last_opened_at: None,
                host_key_fingerprint: None,
                last_remote_dir: None,
                last_local_dir: None,
                group,
                tags,
                // Carried over from the stored entry on save (see app_view).
                proxy: crate::vault::ProxyChoice::Default,
                jump_host: self.selected_jump_id,
                forwards: self.forwards.clone(),
                startup_snippet,
                snippets,
                created_at: *created_at,
                updated_at: now,
            },
        };

        cx.emit(FormEvent::Saved(entry));
    }

    /// The startup choice as the entry stores it. A typed command goes into
    /// the host's own snippets; an emptied one is removed from them.
    fn startup_and_snippets(&self, cx: &App) -> (Option<Uuid>, Vec<HostSnippet>) {
        let mut snippets = self.host_snippets.clone();
        let startup = match self.startup {
            Startup::None => None,
            Startup::Shared(id) => Some(id),
            Startup::Command => {
                let script = self.startup_command.read(cx).value().trim().to_string();
                if script.is_empty() {
                    if let Some(id) = self.startup_command_id {
                        snippets.retain(|s| s.id != id);
                    }
                    None
                } else {
                    let id = self.startup_command_id.unwrap_or_else(Uuid::new_v4);
                    match snippets.iter_mut().find(|s| s.id == id) {
                        Some(snippet) => snippet.script = script,
                        None => snippets.push(HostSnippet {
                            id,
                            name: "On connect".to_string(),
                            script,
                        }),
                    }
                    Some(id)
                }
            }
        };
        (startup, snippets)
    }

    /// Re-detect whether the currently chosen/pasted key is encrypted, which
    /// controls the passphrase field's visibility.
    fn refresh_addkey_encrypted(&mut self, cx: &Context<Self>) {
        let pem = match self.addkey_pem_source {
            PemSource::File => self
                .addkey_file_path
                .as_ref()
                .and_then(|p| read_key_file(p).ok()),
            PemSource::Paste => Some(self.addkey_paste.read(cx).value().to_string()),
        };
        self.addkey_encrypted = pem
            .as_deref()
            .map(crate::ssh::key_is_encrypted)
            .unwrap_or(false);
    }

    fn save_inline_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.addkey_name.read(cx).value().to_string();
        if name.trim().is_empty() {
            self.addkey_error = Some("Name is required.".to_string());
            cx.notify();
            return;
        }

        let pem = match self.addkey_pem_source {
            PemSource::File => {
                let Some(ref path) = self.addkey_file_path else {
                    self.addkey_error = Some("Choose a key file first.".to_string());
                    cx.notify();
                    return;
                };
                match read_key_file(path) {
                    Ok(content) => content,
                    Err(message) => {
                        self.addkey_error = Some(message);
                        cx.notify();
                        return;
                    }
                }
            }
            PemSource::Paste => {
                let v = self.addkey_paste.read(cx).value().to_string();
                if v.trim().is_empty() {
                    self.addkey_error = Some("Paste the PEM key content.".to_string());
                    cx.notify();
                    return;
                }
                v
            }
        };

        let passphrase = {
            let v = self.addkey_passphrase.read(cx).value().to_string();
            if v.is_empty() {
                None
            } else {
                Some(v)
            }
        };

        // Clear passphrase errors before the vault's generic "invalid key".
        match crate::ssh::check_key_passphrase(&pem, passphrase.as_deref()) {
            Some(crate::ssh::KeyIssue::MissingPassphrase) => {
                self.addkey_encrypted = true;
                self.addkey_error =
                    Some("This key is encrypted. Enter its passphrase.".to_string());
                cx.notify();
                return;
            }
            Some(crate::ssh::KeyIssue::WrongPassphrase) => {
                self.addkey_error = Some("Wrong passphrase for this key.".to_string());
                cx.notify();
                return;
            }
            None => {}
        }

        let result = self.vault_state.update(cx, |vs, _| {
            vs.manager_mut()
                .map(|vm| vm.add_key(name.trim().to_string(), pem, passphrase))
        });

        match result {
            Some(Ok(new_id)) => {
                self.selected_key_id = Some(new_id);
                self.form_state = FormState::Main;
                self.addkey_error = None;
                self.addkey_file_path = None;
                self.addkey_pem_source = PemSource::File;
                self.addkey_encrypted = false;
                self.addkey_name
                    .update(cx, |s, cx| s.set_value("", window, cx));
                self.addkey_paste
                    .update(cx, |s, cx| s.set_value("", window, cx));
                self.addkey_passphrase
                    .update(cx, |s, cx| s.set_value("", window, cx));
            }
            Some(Err(VaultError::InvalidKey(msg))) => {
                self.addkey_error = Some(format!("Invalid key: {msg}"));
            }
            Some(Err(e)) => {
                tracing::error!(?e, "failed to add key");
                self.addkey_error = Some(format!("Could not save the key: {e}"));
            }
            None => {}
        }
        cx.notify();
    }
}

impl Render for ServerFormView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if matches!(self.form_state, FormState::AddingKey) {
            return self.render_add_key_inline(window, cx).into_any_element();
        }
        if matches!(self.form_state, FormState::EditingForward) {
            return self.render_forward_editor(window, cx).into_any_element();
        }

        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let danger = theme.danger;
        let list_hover = crate::theme::field();
        let inner_radius = px(6.);

        let is_edit = matches!(self.mode, FormMode::Edit { .. });
        let is_valid = self.is_valid(cx);
        let footer_message = self
            .error
            .clone()
            .map(|msg| (msg, danger))
            .or_else(|| self.warning.clone().map(|msg| (msg, theme.warning)));

        let keys = self
            .vault_state
            .read(cx)
            .manager()
            .and_then(|vm| vm.list_keys().ok())
            .unwrap_or_default();
        let keys_empty = keys.is_empty();
        let is_missing = self
            .selected_key_id
            .map(|id| !keys.iter().any(|k| k.id == id))
            .unwrap_or(false);

        // Where the key dropdown lands: a probe over the trigger writes the
        // field's bottom-left corner here at prepaint, and the panel, which
        // sits later in the tree, reads it at its own prepaint.
        let key_anchor: Rc<Cell<Point<Pixels>>> = Rc::new(Cell::new(Point::default()));
        let key_trigger: Rc<Cell<Bounds<Pixels>>> = Rc::new(Cell::new(Bounds::default()));

        let popup = if matches!(self.auth_mode, AuthMode::PrivateKey) && self.key_dropdown_open {
            let items: Vec<AnyElement> = keys
                .iter()
                .enumerate()
                .map(|(ix, key)| {
                    let id = key.id;
                    dropdown_option(
                        ("key-option", ix).into(),
                        format!("{} · {}", key.name, key.key_type),
                        fg,
                        cx,
                        move |this, cx| {
                            this.selected_key_id = Some(id);
                            this.key_dropdown_open = false;
                            cx.notify();
                        },
                    )
                })
                .collect();

            Some(dropdown_panel(
                "key-dropdown-popup",
                &key_anchor,
                &key_trigger,
                items,
                cx,
                |this, cx| {
                    this.key_dropdown_open = false;
                    cx.notify();
                },
            ))
        } else {
            None
        };

        let (jump_field, jump_popup) = self.render_jump_picker(cx);
        let (startup_field, startup_popup) = self.render_startup_snippet_picker(cx);

        let group_focused = self
            .group_input
            .read(cx)
            .focus_handle(cx)
            .is_focused(window);
        let group_suggestions = self.group_suggestions(cx);
        let group_highlight = self.group_highlight;

        let group_field = v_flex()
            .gap(px(6.))
            .capture_key_down(cx.listener(Self::on_group_key))
            .child(field_label("Group", cx))
            .child(Input::new(&self.group_input))
            .when(group_focused && !group_suggestions.is_empty(), |el| {
                el.child(
                    v_flex()
                        .p(px(4.))
                        .rounded(px(9.))
                        .border_1()
                        .border_color(crate::theme::float_border())
                        .bg(crate::theme::pop_surface())
                        .children(group_suggestions.into_iter().enumerate().map(|(ix, path)| {
                            let value = path.clone();
                            h_flex()
                                .id(("group-suggestion", ix))
                                .h(px(30.))
                                .px(px(8.))
                                .gap(px(8.))
                                .items_center()
                                .rounded(inner_radius)
                                .text_size(px(13.))
                                .text_color(fg)
                                .cursor_pointer()
                                .hover(move |s| s.bg(list_hover))
                                .when(group_highlight == Some(ix), |s| s.bg(list_hover))
                                // mouse-down, not click: must run before the
                                // input loses focus and the list unmounts.
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                                        this.set_group(value.clone(), window, cx);
                                    }),
                                )
                                .child(
                                    svg()
                                        .path("icons/folder.svg")
                                        .size(px(14.))
                                        .text_color(muted_fg),
                                )
                                .child(path)
                        })),
                )
            });

        let tag_focused = self.tag_input.read(cx).focus_handle(cx).is_focused(window);
        let tag_text = self.tag_input.read(cx).value().trim().to_string();
        let tag_suggestions = self.tag_suggestions(cx);
        let tag_highlight = self.tag_highlight;
        // A tag already on this host is not in the suggestions, but it is not new.
        let exact_match = tag_suggestions
            .iter()
            .chain(self.tag_names.iter())
            .any(|t| t.eq_ignore_ascii_case(&tag_text));

        let chips =
            h_flex()
                .flex_wrap()
                .gap(px(5.))
                .children(self.tag_names.iter().enumerate().map(|(ix, name)| {
                    h_flex()
                        .h(px(26.))
                        .pl(px(9.))
                        .pr(px(4.))
                        .gap(px(4.))
                        .items_center()
                        .rounded(px(6.))
                        .border_1()
                        .border_color(crate::theme::border())
                        .bg(crate::theme::field())
                        .text_size(px(12.5))
                        .text_color(fg)
                        .child(format!("#{name}"))
                        .child(
                            div()
                                .id(("tag-remove", ix))
                                .size(px(18.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(4.))
                                .text_color(muted_fg)
                                .cursor_pointer()
                                .hover(move |s| s.bg(list_hover).text_color(fg))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.tag_names.remove(ix);
                                    cx.notify();
                                }))
                                .child(Icon::new(IconName::Close).size(px(12.))),
                        )
                }));

        let tags_field = v_flex()
            .gap(px(6.))
            .capture_key_down(cx.listener(Self::on_tag_key))
            .child(field_label("Tags", cx))
            .when(!self.tag_names.is_empty(), |el| el.child(chips))
            .child(Input::new(&self.tag_input))
            .when(
                tag_focused
                    && (!tag_suggestions.is_empty() || (!tag_text.is_empty() && !exact_match)),
                |el| {
                    el.child(
                        v_flex()
                            .p(px(4.))
                            .rounded(px(9.))
                            .border_1()
                            .border_color(crate::theme::float_border())
                            .bg(crate::theme::pop_surface())
                            // "Create" row first when the text is a new tag.
                            .when(!tag_text.is_empty() && !exact_match, |el| {
                                el.child(
                                    h_flex()
                                        .id("tag-create")
                                        .h(px(30.))
                                        .px(px(8.))
                                        .items_center()
                                        .rounded(inner_radius)
                                        .text_size(px(13.))
                                        .text_color(fg)
                                        .cursor_pointer()
                                        .hover(move |s| s.bg(list_hover))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(|this, _: &MouseDownEvent, window, cx| {
                                                this.commit_tag(window, cx);
                                            }),
                                        )
                                        .child(format!("Create \"#{tag_text}\"")),
                                )
                            })
                            .children(tag_suggestions.into_iter().enumerate().map(|(ix, name)| {
                                let value = name.clone();
                                h_flex()
                                    .id(("tag-suggestion", ix))
                                    .h(px(30.))
                                    .px(px(8.))
                                    .items_center()
                                    .rounded(inner_radius)
                                    .text_size(px(13.))
                                    .text_color(fg)
                                    .cursor_pointer()
                                    .hover(move |s| s.bg(list_hover))
                                    .when(tag_highlight == Some(ix), |s| s.bg(list_hover))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                                            this.add_tag(value.clone(), window, cx);
                                        }),
                                    )
                                    .child(format!("#{name}"))
                            })),
                    )
                },
            );

        let auth_body: gpui::AnyElement = match self.auth_mode {
            AuthMode::Password => {
                labeled("Password", cx, Input::new(&self.pass_input).mask_toggle())
                    .into_any_element()
            }

            AuthMode::PrivateKey => {
                let display_label = match self.selected_key_id {
                    None if keys_empty => "No keys yet".to_string(),
                    None => "Select a key…".to_string(),
                    Some(id) => keys
                        .iter()
                        .find(|k| k.id == id)
                        .map(|k| format!("{} · {}", k.name, k.key_type))
                        .unwrap_or_else(|| "(missing key)".to_string()),
                };

                let label_color = if self.selected_key_id.is_some() {
                    fg
                } else {
                    muted_fg
                };

                v_flex()
                    .gap(px(6.))
                    .child(field_label("SSH key", cx))
                    .child(dropdown_trigger(
                        "key-dropdown-btn",
                        display_label,
                        label_color,
                        &key_anchor,
                        &key_trigger,
                        cx,
                        move |this, cx| {
                            if keys_empty {
                                return;
                            }
                            this.key_dropdown_open = !this.key_dropdown_open;
                            cx.notify();
                        },
                    ))
                    .when(is_missing, |el| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(danger)
                                .child("This host's key was deleted. Pick another one."),
                        )
                    })
                    .child(
                        h_flex().child(
                            Button::new("add-new-key-link")
                                .ghost()
                                .small()
                                .icon(IconName::Plus)
                                .child("New key")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.form_state = FormState::AddingKey;
                                    this.addkey_error = None;
                                    cx.notify();
                                })),
                        ),
                    )
                    .into_any_element()
            }

            AuthMode::Agent => div()
                .text_size(px(12.))
                .text_color(muted_fg)
                .child(if cfg!(windows) {
                    "Keys are provided by the system SSH agent. The \
                     'OpenSSH Authentication Agent' service must be running \
                     and hold your keys (ssh-add)."
                } else {
                    "Keys are provided by the system SSH agent (SSH_AUTH_SOCK). \
                     Load them with ssh-add."
                })
                .into_any_element(),
        };

        let auth_switch = segmented(
            "auth-mode",
            &[
                (AuthMode::Password, "Password"),
                (AuthMode::PrivateKey, "SSH key"),
                (AuthMode::Agent, "Agent"),
            ],
            self.auth_mode,
            cx,
            |this, mode, cx| {
                this.auth_mode = mode;
                this.key_dropdown_open = false;
                cx.notify();
            },
        );

        // User @ host : port, the order ssh writes it in. A pasted
        // `user@host:port` in the host field is split across the three.
        let address_row = h_flex()
            .gap(px(8.))
            .items_end()
            .child(
                labeled("User", cx, Input::new(&self.user_input))
                    .w(px(150.))
                    .flex_shrink(1.),
            )
            .child(address_separator("@"))
            .child(
                labeled("Host", cx, Input::new(&self.host_input))
                    .flex_1()
                    .min_w(px(120.)),
            )
            .child(address_separator(":"))
            .child(
                labeled("Port", cx, Input::new(&self.port_input))
                    .w(px(76.))
                    .flex_none(),
            );

        let advanced_summary = self.advanced_summary(cx);
        let is_advanced_open = self.is_advanced_open;
        let advanced_toggle = h_flex()
            .id("advanced-toggle")
            .w_full()
            .h(px(32.))
            .gap(px(8.))
            .items_center()
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| {
                this.is_advanced_open = !this.is_advanced_open;
                // A panel left open would hang where its field used to be.
                this.close_dropdowns(cx);
            }))
            .child(
                Icon::new(if is_advanced_open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size(px(14.))
                .text_color(muted_fg),
            )
            .child(section_title("Advanced", cx))
            .when(!is_advanced_open, |el| {
                el.child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(12.))
                        .text_color(crate::theme::text_dim())
                        .child(advanced_summary),
                )
            });

        let fields = v_flex()
            .id("server-form-fields")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            // An open panel stays where its field was and is not clipped by
            // the dialog, so it closes rather than float over the header.
            .on_scroll_wheel(cx.listener(|this, _, _, cx| this.close_dropdowns(cx)))
            .px(px(16.))
            .pt(px(4.))
            .pb(px(16.))
            .gap(px(20.))
            .child(
                v_flex()
                    .gap(px(12.))
                    .child(section_title("Connection", cx))
                    .child(address_row)
                    .child(labeled("Name", cx, Input::new(&self.name_input))),
            )
            .child(
                v_flex()
                    .gap(px(12.))
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .child(section_title("Authentication", cx))
                            .child(auth_switch),
                    )
                    .child(auth_body),
            )
            .child(
                v_flex()
                    .gap(px(12.))
                    .child(section_title("Organize", cx))
                    .child(
                        h_flex()
                            .gap(px(12.))
                            .items_start()
                            .child(group_field.flex_1().min_w_0())
                            .child(tags_field.flex_1().min_w_0()),
                    ),
            )
            .child(
                v_flex()
                    .gap(px(12.))
                    .child(advanced_toggle)
                    .when(is_advanced_open, |el| {
                        el.child(
                            h_flex()
                                .gap(px(12.))
                                .items_start()
                                .child(div().flex_1().min_w_0().child(jump_field))
                                .child(div().flex_1().min_w_0().child(startup_field)),
                        )
                        .when(self.startup == Startup::Command, |el| {
                            el.child(self.render_startup_command(cx))
                        })
                        .child(self.render_forwards_section(cx))
                    }),
            );

        let footer_note = footer_message.or_else(|| {
            self.missing_hint(cx)
                .map(|hint| (hint.to_string(), crate::theme::text_dim()))
        });

        v_flex()
            .max_h(dialog_max_height(window))
            .child(
                div().flex_none().px(px(16.)).pt(px(14.)).pb(px(12.)).child(
                    DialogHeader::new().child(
                        div()
                            .text_size(px(15.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(fg)
                            .child(if is_edit { "Edit host" } else { "New host" }),
                    ),
                ),
            )
            .child(fields)
            .child(
                // TODO security: input widget internals not zeroizable, see SECURITY.md
                h_flex()
                    .flex_none()
                    .gap(px(8.))
                    .justify_between()
                    .items_center()
                    .px(px(16.))
                    .py(px(12.))
                    .border_t_1()
                    .border_color(crate::theme::border())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(12.))
                            .when_some(footer_note, |el, (msg, color)| {
                                el.text_color(color).child(msg)
                            }),
                    )
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .child(Button::new("form-cancel").ghost().child("Cancel").on_click(
                                cx.listener(|_, _, _, cx| {
                                    cx.emit(FormEvent::Cancelled);
                                }),
                            ))
                            .child(
                                Button::new("form-submit")
                                    .primary()
                                    .child(if is_edit { "Save" } else { "Add host" })
                                    .disabled(!is_valid)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.submit(cx);
                                    })),
                            ),
                    ),
            )
            .children(popup)
            .children(jump_popup)
            .children(startup_popup)
            .into_any_element()
    }
}

impl ServerFormView {
    fn close_dropdowns(&mut self, cx: &mut Context<Self>) {
        if self.key_dropdown_open || self.jump_dropdown_open || self.startup_dropdown_open {
            self.key_dropdown_open = false;
            self.jump_dropdown_open = false;
            self.startup_dropdown_open = false;
            cx.notify();
        }
    }

    /// What the folded "Advanced" section holds, in one line.
    fn advanced_summary(&self, cx: &App) -> String {
        let vault = self.vault_state.read(cx).manager();
        // The same lookup as the jump field, so the two never disagree.
        let edited_id = match self.mode {
            FormMode::Edit { id, .. } => Some(id),
            FormMode::Add => None,
        };
        let jump = match self.selected_jump_id {
            None => "Direct connection".to_string(),
            Some(id) => vault
                .map(|vm| vm.list_jump_candidates(edited_id))
                .and_then(|candidates| candidates.into_iter().find(|c| c.id == id))
                .map(|candidate| {
                    let name = if candidate.label.is_empty() {
                        candidate.host
                    } else {
                        candidate.label
                    };
                    format!("Through {name}")
                })
                .unwrap_or_else(|| "Through a removed host".to_string()),
        };
        let forwards = match self.forwards.len() {
            0 => "no forwards".to_string(),
            1 => "1 forward".to_string(),
            n => format!("{n} forwards"),
        };
        let startup = match self.startup {
            Startup::None => "no startup snippet".to_string(),
            Startup::Command => "runs a command".to_string(),
            Startup::Shared(id) => vault
                .and_then(|vm| vm.list_snippets().ok())
                .and_then(|snippets| snippets.into_iter().find(|s| s.id == id))
                .map(|snippet| format!("runs {}", snippet.name))
                .unwrap_or_else(|| "a removed snippet".to_string()),
        };
        format!("{jump} · {forwards} · {startup}")
    }

    /// Why "Add host" is greyed out, while it is.
    fn missing_hint(&self, cx: &App) -> Option<&'static str> {
        if strip_whitespace(&self.host_input.read(cx).value()).is_empty() {
            return Some("Enter the host.");
        }
        if strip_whitespace(&self.user_input.read(cx).value()).is_empty() {
            return Some("Enter the user.");
        }
        let port = strip_whitespace(&self.port_input.read(cx).value());
        if port.parse::<u16>().map(|p| p < 1).unwrap_or(true) {
            return Some("The port is a number from 1 to 65535.");
        }
        match self.auth_mode {
            AuthMode::Password if self.pass_input.read(cx).value().is_empty() => {
                Some("Enter the password.")
            }
            AuthMode::PrivateKey if self.selected_key_id.is_none() => {
                Some("Choose a key or add one.")
            }
            _ => None,
        }
    }

    // Mirrors `SettingsView::render_add_key`.
    fn render_add_key_inline(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let muted_fg = theme.muted_foreground;

        let is_file = self.addkey_pem_source == PemSource::File;
        let source_idx = if is_file { 0usize } else { 1usize };

        let file_label = self
            .addkey_file_path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "No file selected".to_string());

        let pem_section: gpui::AnyElement = if is_file {
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    Button::new("addkey-choose-file")
                        .child("Choose file…")
                        .on_click(cx.listener(|_this, _, window, cx| {
                            cx.spawn_in(window, async move |this, cx| {
                                let handle = rfd::AsyncFileDialog::new().pick_file().await;
                                let Some(handle) = handle else { return };
                                let path = handle.path().to_path_buf();
                                let stem = path
                                    .file_stem()
                                    .and_then(|s| s.to_str())
                                    .unwrap_or("")
                                    .to_string();
                                this.update_in(cx, |this, window, cx| {
                                    let current_name =
                                        this.addkey_name.read(cx).value().to_string();
                                    this.addkey_file_path = Some(path);
                                    this.refresh_addkey_encrypted(cx);
                                    if current_name.trim().is_empty() && !stem.is_empty() {
                                        this.addkey_name.update(cx, |s, cx| {
                                            s.set_value(SharedString::from(stem), window, cx);
                                        });
                                    }
                                    cx.notify();
                                })
                                .ok();
                            })
                            .detach();
                        })),
                )
                .child(div().text_xs().text_color(muted_fg).child(file_label))
                .into_any_element()
        } else {
            Textarea::new(&self.addkey_paste)
                .h(px(120.))
                .into_any_element()
        };

        v_flex()
            .id("addkey-form")
            .max_h(dialog_max_height(window))
            .overflow_y_scroll()
            .p(px(16.))
            .gap(px(8.))
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(crate::ui::settings::view::back_button(
                        "addkey-back",
                        cx.listener(|this, _, _, cx| {
                            this.form_state = FormState::Main;
                            this.addkey_error = None;
                            cx.notify();
                        }),
                        cx,
                    ))
                    .child(sub_view_title("Add SSH key", cx)),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(field_label("Name", cx))
                    .child(Input::new(&self.addkey_name)),
            )
            .child(
                v_flex().gap_1().child(field_label("Key source", cx)).child(
                    RadioGroup::horizontal("addkey-source")
                        .selected_index(Some(source_idx))
                        .on_click(cx.listener(|this, ix: &usize, window, cx| {
                            if *ix == 0 && this.addkey_pem_source != PemSource::File {
                                this.addkey_pem_source = PemSource::File;
                                this.addkey_paste
                                    .update(cx, |s, cx| s.set_value("", window, cx));
                                this.refresh_addkey_encrypted(cx);
                                cx.notify();
                            }
                            if *ix == 1 && this.addkey_pem_source != PemSource::Paste {
                                this.addkey_pem_source = PemSource::Paste;
                                this.addkey_file_path = None;
                                this.refresh_addkey_encrypted(cx);
                                cx.notify();
                            }
                        }))
                        .child(Radio::new("addkey-src-file").label("From file"))
                        .child(Radio::new("addkey-src-paste").label("Paste")),
                ),
            )
            .child(pem_section)
            .when(self.addkey_encrypted, |el| {
                el.child(
                    v_flex()
                        .gap_1()
                        .child(field_label("Passphrase (key is encrypted)", cx))
                        .child(Input::new(&self.addkey_passphrase).mask_toggle()),
                )
            })
            .children(
                self.addkey_error
                    .as_ref()
                    .map(|msg| div().text_xs().text_color(theme.danger).child(msg.clone())),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .mt_2()
                    .child(
                        Button::new("addkey-cancel")
                            .ghost()
                            .child("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.form_state = FormState::Main;
                                this.addkey_error = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("addkey-save")
                            .primary()
                            .child("Save key")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.save_inline_key(window, cx);
                            })),
                    ),
            )
    }
}

impl ServerFormView {
    fn enter_add_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing_forward = None;
        self.forward_kind = ForwardKind::Local;
        self.forward_bind_public = false;
        self.forward_auto_start = false;
        self.forward_error = None;
        self.fill_forward_fields("", "", "", "", window, cx);
        self.form_state = FormState::EditingForward;
        cx.notify();
    }

    fn enter_edit_forward(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(forward) = self.forwards.get(index).cloned() else {
            return;
        };
        self.editing_forward = Some(index);
        self.forward_kind = forward.kind;
        self.forward_bind_public = !is_loopback_bind(&forward.bind_address);
        self.forward_auto_start = forward.auto_start;
        self.forward_error = None;
        self.fill_forward_fields(
            &forward.name,
            &forward.bind_port.to_string(),
            &forward.target_host,
            &forward.target_port.to_string(),
            window,
            cx,
        );
        self.form_state = FormState::EditingForward;
        cx.notify();
    }

    fn fill_forward_fields(
        &self,
        name: &str,
        bind_port: &str,
        target_host: &str,
        target_port: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let fields = [
            (&self.forward_name, name),
            (&self.forward_bind_port, bind_port),
            (&self.forward_target_host, target_host),
            (&self.forward_target_port, target_port),
        ];
        for (input, value) in fields {
            let value = SharedString::from(value.to_string());
            input.update(cx, |state, cx| state.set_value(value, window, cx));
        }
    }

    fn delete_forward(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.forwards.len() {
            self.forwards.remove(index);
            cx.notify();
        }
    }

    fn save_forward(&mut self, cx: &mut Context<Self>) {
        let bind_address = bind_address_for(self.forward_kind, self.forward_bind_public);
        let name = self.forward_name.read(cx).value().trim().to_string();
        let bind_port = strip_whitespace(&self.forward_bind_port.read(cx).value());
        let target_host = strip_whitespace(&self.forward_target_host.read(cx).value());
        let target_port = strip_whitespace(&self.forward_target_port.read(cx).value());

        // A row cannot clash with itself while it is being edited.
        let others: Vec<PortForward> = self
            .forwards
            .iter()
            .enumerate()
            .filter(|(index, _)| Some(*index) != self.editing_forward)
            .map(|(_, forward)| forward.clone())
            .collect();

        let ports = validate_forward(
            self.forward_kind,
            &name,
            bind_address,
            &bind_port,
            &target_host,
            &target_port,
            &others,
        );
        let (bind_port, target_port) = match ports {
            Ok(ports) => ports,
            Err(message) => {
                self.forward_error = Some(message);
                cx.notify();
                return;
            }
        };

        // A SOCKS5 forward has no fixed target: every client names its own.
        let (target_host, target_port) = match self.forward_kind {
            ForwardKind::Dynamic => (String::new(), 0),
            _ => (target_host, target_port),
        };

        let edited = self
            .editing_forward
            .and_then(|index| self.forwards.get_mut(index));
        match edited {
            Some(forward) => {
                forward.name = name;
                forward.kind = self.forward_kind;
                forward.bind_address = bind_address.to_string();
                forward.bind_port = bind_port;
                forward.target_host = target_host;
                forward.target_port = target_port;
                forward.auto_start = self.forward_auto_start;
            }
            None => self.forwards.push(PortForward {
                id: Uuid::new_v4(),
                name,
                kind: self.forward_kind,
                bind_address: bind_address.to_string(),
                bind_port,
                target_host,
                target_port,
                auto_start: self.forward_auto_start,
            }),
        }

        self.form_state = FormState::Main;
        self.forward_error = None;
        cx.notify();
    }

    /// The jump-host field and its dropdown. The dropdown comes back on its
    /// own so the caller can hang it off the form's root, where nothing clips
    /// it.
    fn render_jump_picker(&self, cx: &Context<Self>) -> (AnyElement, Option<AnyElement>) {
        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let danger = theme.danger;

        // Every host that can sit in front of this one. The vault drops the
        // candidates that would close a loop or push the chain past the depth
        // cap, so anything offered here saves cleanly.
        let edited_id = match self.mode {
            FormMode::Edit { id, .. } => Some(id),
            FormMode::Add => None,
        };
        let jump_candidates = self
            .vault_state
            .read(cx)
            .manager()
            .map(|vm| vm.list_jump_candidates(edited_id))
            .unwrap_or_default();

        let selected_jump = self
            .selected_jump_id
            .map(|id| jump_candidates.iter().find(|c| c.id == id));
        let jump_label = match selected_jump {
            None => "Direct connection".to_string(),
            Some(Some(c)) => jump_option_label(c),
            Some(None) => "(missing host)".to_string(),
        };
        // The saved jump host is gone from the vault: the link is dead and
        // the host would fail to connect, so say it in the danger colour.
        let is_jump_missing = matches!(selected_jump, Some(None));
        let jump_label_color = if is_jump_missing {
            danger
        } else if self.selected_jump_id.is_some() {
            fg
        } else {
            muted_fg
        };
        // A dead link must stay clearable even when the vault offers nothing
        // else: the dropdown still carries its "Direct connection" row.
        let can_pick_jump = !jump_candidates.is_empty() || self.selected_jump_id.is_some();

        // Same trigger-probe arrangement as `key_anchor`.
        let jump_anchor: Rc<Cell<Point<Pixels>>> = Rc::new(Cell::new(Point::default()));
        let jump_trigger: Rc<Cell<Bounds<Pixels>>> = Rc::new(Cell::new(Bounds::default()));

        let jump_field = v_flex()
            .gap(px(6.))
            .child(field_label("Jump host", cx))
            .child(dropdown_trigger(
                "jump-dropdown-btn",
                jump_label,
                jump_label_color,
                &jump_anchor,
                &jump_trigger,
                cx,
                move |this, cx| {
                    if !can_pick_jump {
                        return;
                    }
                    this.jump_dropdown_open = !this.jump_dropdown_open;
                    cx.notify();
                },
            ))
            .when(is_jump_missing, |el| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(danger)
                        .child("That jump host was removed. Pick another, or go direct."),
                )
            })
            .when(jump_candidates.is_empty() && !is_jump_missing, |el| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(muted_fg)
                        .child("No other host can be used as a jump host yet."),
                )
            });

        let jump_popup = if self.jump_dropdown_open {
            let direct = dropdown_option(
                "jump-option-direct".into(),
                "Direct connection".to_string(),
                muted_fg,
                cx,
                |this, cx| {
                    this.selected_jump_id = None;
                    this.jump_dropdown_open = false;
                    cx.notify();
                },
            );

            let items: Vec<AnyElement> = std::iter::once(direct)
                .chain(jump_candidates.iter().enumerate().map(|(ix, candidate)| {
                    let id = candidate.id;
                    dropdown_option(
                        ("jump-option", ix).into(),
                        jump_option_label(candidate),
                        fg,
                        cx,
                        move |this, cx| {
                            this.selected_jump_id = Some(id);
                            this.jump_dropdown_open = false;
                            cx.notify();
                        },
                    )
                }))
                .collect();

            Some(dropdown_panel(
                "jump-dropdown-popup",
                &jump_anchor,
                &jump_trigger,
                items,
                cx,
                |this, cx| {
                    this.jump_dropdown_open = false;
                    cx.notify();
                },
            ))
        } else {
            None
        };

        (jump_field.into_any_element(), jump_popup)
    }

    /// The run-on-connect field and its dropdown, arranged like the jump
    /// picker: nothing, a command for this host, or a shared snippet.
    fn render_startup_snippet_picker(
        &self,
        cx: &Context<Self>,
    ) -> (AnyElement, Option<AnyElement>) {
        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let danger = theme.danger;

        let mut snippets = self
            .vault_state
            .read(cx)
            .manager()
            .and_then(|vm| vm.list_snippets().ok())
            .unwrap_or_default();
        snippets.sort_by_key(|s| s.name.to_lowercase());

        let (label, label_color, is_missing) = match self.startup {
            Startup::None => ("None".to_string(), muted_fg, false),
            Startup::Command => (COMMAND_OPTION.to_string(), fg, false),
            Startup::Shared(id) => match snippets.iter().find(|s| s.id == id) {
                Some(snippet) => (snippet.name.clone(), fg, false),
                None => ("(missing snippet)".to_string(), danger, true),
            },
        };

        let anchor: Rc<Cell<Point<Pixels>>> = Rc::new(Cell::new(Point::default()));
        let trigger: Rc<Cell<Bounds<Pixels>>> = Rc::new(Cell::new(Bounds::default()));

        let field = v_flex()
            .gap(px(6.))
            .child(field_label("Run on connect", cx))
            .child(dropdown_trigger(
                "startup-dropdown-btn",
                label,
                label_color,
                &anchor,
                &trigger,
                cx,
                move |this, cx| {
                    this.startup_dropdown_open = !this.startup_dropdown_open;
                    cx.notify();
                },
            ))
            .when(is_missing, |el| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(danger)
                        .child("That snippet was deleted. Pick another, or none."),
                )
            })
            .when(
                matches!(self.startup, Startup::Shared(_)) && !is_missing,
                |el| {
                    el.child(
                        div()
                            .text_xs()
                            .text_color(muted_fg)
                            .child("Sent to the terminal after every connect."),
                    )
                },
            );

        let popup = if self.startup_dropdown_open {
            let pick = |startup: Startup| {
                move |this: &mut Self, cx: &mut Context<Self>| {
                    this.startup = startup;
                    this.startup_dropdown_open = false;
                    cx.notify();
                }
            };
            let fixed = [
                dropdown_option(
                    "startup-option-none".into(),
                    "None".to_string(),
                    muted_fg,
                    cx,
                    pick(Startup::None),
                ),
                dropdown_option(
                    "startup-option-command".into(),
                    COMMAND_OPTION.to_string(),
                    fg,
                    cx,
                    pick(Startup::Command),
                ),
            ];
            let items: Vec<AnyElement> = fixed
                .into_iter()
                .chain(snippets.iter().enumerate().map(|(ix, snippet)| {
                    dropdown_option(
                        ("startup-option", ix).into(),
                        snippet.name.clone(),
                        fg,
                        cx,
                        pick(Startup::Shared(snippet.id)),
                    )
                }))
                .collect();

            Some(dropdown_panel(
                "startup-dropdown-popup",
                &anchor,
                &trigger,
                items,
                cx,
                |this, cx| {
                    this.startup_dropdown_open = false;
                    cx.notify();
                },
            ))
        } else {
            None
        };

        (field.into_any_element(), popup)
    }

    /// The command box under the run-on-connect field.
    fn render_startup_command(&self, cx: &Context<Self>) -> impl IntoElement {
        v_flex()
            .gap(px(6.))
            .child(Textarea::new(&self.startup_command).h(px(76.)))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        "Sent to the terminal after every connect. It belongs to this host \
                         and also shows under Snippets in the server panel.",
                    ),
            )
    }

    /// The list of saved forwards on the main form, with an add button.
    fn render_forwards_section(&self, cx: &Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let count = self.forwards.len();
        let inner_radius = px(8.);

        let rows = self.forwards.iter().enumerate().map(|(index, forward)| {
            let route = forward.spec().route_label();
            let is_first = index == 0;
            let is_last = index + 1 == count;
            h_flex()
                .id(("forward-row", index))
                .relative()
                .h(px(48.))
                .items_center()
                .gap(px(10.))
                .px(px(12.))
                .when(is_first, |row| row.rounded_t(inner_radius))
                .when(is_last, |row| row.rounded_b(inner_radius))
                .hover(|s| s.bg(crate::theme::field()))
                .when(!is_first, |row| {
                    row.child(
                        div()
                            .absolute()
                            .top_0()
                            .left(px(12.))
                            .right_0()
                            .h(px(1.))
                            .bg(crate::theme::border()),
                    )
                })
                .child(forward_kind_pill(forward.kind, cx))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(px(2.))
                        .child(
                            h_flex()
                                .gap(px(6.))
                                .items_center()
                                .child(
                                    div()
                                        .text_size(px(13.))
                                        .text_color(fg)
                                        .truncate()
                                        .child(SharedString::from(forward.name.clone())),
                                )
                                .when(!is_loopback_bind(&forward.bind_address), |el| {
                                    el.child(
                                        div()
                                            .text_size(px(11.5))
                                            .text_color(crate::theme::warning())
                                            .child("public"),
                                    )
                                })
                                .when(forward.auto_start, |el| {
                                    el.child(
                                        div()
                                            .text_size(px(11.5))
                                            .text_color(muted_fg)
                                            .child("auto"),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .font_family(crate::theme::MONO_FONT)
                                .text_size(px(11.5))
                                .text_color(crate::theme::text_dim())
                                .truncate()
                                .child(SharedString::from(route)),
                        ),
                )
                .child(
                    Button::new(("forward-edit", index))
                        .ghost()
                        .small()
                        .child("Edit")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.enter_edit_forward(index, window, cx);
                        })),
                )
                .child(
                    Button::new(("forward-delete", index))
                        .ghost()
                        .small()
                        .child("Delete")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.delete_forward(index, cx);
                        })),
                )
        });

        v_flex()
            .gap(px(6.))
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(field_label("Port forwarding", cx))
                    .child(
                        Button::new("forward-add-link")
                            .ghost()
                            .small()
                            .icon(IconName::Plus)
                            .child("Add forward")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.enter_add_forward(window, cx);
                            })),
                    ),
            )
            .when(count > 0, |el| {
                el.child(
                    v_flex()
                        .rounded(px(9.))
                        .border_1()
                        .border_color(crate::theme::border())
                        .bg(crate::theme::card())
                        .children(rows),
                )
            })
            .when(count == 0, |el| {
                el.child(
                    div()
                        .text_size(px(12.))
                        .text_color(crate::theme::text_dim())
                        .child("None. A forward opens a port here or on the host through this connection."),
                )
            })
            .into_any_element()
    }

    /// Full-panel editor for one forward, in place of the main form.
    fn render_forward_editor(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let muted_fg = theme.muted_foreground;
        let danger = theme.danger;

        let kind = self.forward_kind;
        let is_dynamic = kind == ForwardKind::Dynamic;
        let is_remote = kind == ForwardKind::Remote;
        let is_public = self.forward_bind_public;
        let title = if self.editing_forward.is_some() {
            "Edit forward"
        } else {
            "Add forward"
        };

        let (private_label, public_label, public_warning) = if is_remote {
            (
                "Host loopback only (localhost)",
                "All host interfaces (0.0.0.0)",
                "Most hosts refuse this unless GatewayPorts is on in sshd_config.",
            )
        } else {
            (
                "This computer only (127.0.0.1)",
                "Any network interface (0.0.0.0)",
                "Anyone who can reach this machine on the network can use this tunnel.",
            )
        };

        const KINDS: [ForwardKind; 3] = [
            ForwardKind::Local,
            ForwardKind::Remote,
            ForwardKind::Dynamic,
        ];
        let kind_row = RadioGroup::horizontal("forward-kind")
            .selected_index(KINDS.iter().position(|option| *option == kind))
            .on_click(cx.listener(|this, ix: &usize, _, cx| {
                if let Some(option) = KINDS.get(*ix) {
                    this.forward_kind = *option;
                    cx.notify();
                }
            }))
            .child(Radio::new("forward-kind-local").label("Local (-L)"))
            .child(Radio::new("forward-kind-remote").label("Remote (-R)"))
            .child(Radio::new("forward-kind-dynamic").label("SOCKS5 (-D)"));

        let bind_row = RadioGroup::vertical("forward-bind")
            .selected_index(Some(is_public as usize))
            .on_click(cx.listener(|this, ix: &usize, _, cx| {
                this.forward_bind_public = *ix == 1;
                cx.notify();
            }))
            .child(Radio::new("forward-bind-private").label(private_label))
            .child(Radio::new("forward-bind-public").label(public_label));

        v_flex()
            .id("forward-form")
            .max_h(dialog_max_height(window))
            .overflow_y_scroll()
            .p(px(16.))
            .gap(px(8.))
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(crate::ui::settings::view::back_button(
                        "forward-back",
                        cx.listener(|this, _, _, cx| {
                            this.form_state = FormState::Main;
                            this.forward_error = None;
                            cx.notify();
                        }),
                        cx,
                    ))
                    .child(sub_view_title(title, cx)),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(field_label("Name", cx))
                    .child(Input::new(&self.forward_name)),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(field_label("Direction", cx))
                    .child(kind_row),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(field_label(
                        if is_remote {
                            "Host listens on"
                        } else {
                            "Listen on"
                        },
                        cx,
                    ))
                    .child(bind_row)
                    .when(is_public, |el| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(crate::theme::warning())
                                .child(public_warning),
                        )
                    }),
            )
            .child(
                v_flex()
                    .w(px(80.))
                    .gap_1()
                    .child(field_label("Port", cx))
                    .child(Input::new(&self.forward_bind_port)),
            )
            .when(!is_dynamic, |el| {
                el.child(
                    h_flex()
                        .gap_2()
                        .child(
                            v_flex()
                                .flex_1()
                                .gap_1()
                                .child(field_label(
                                    if is_remote {
                                        "Forward to (on this computer)"
                                    } else {
                                        "Forward to (from the host)"
                                    },
                                    cx,
                                ))
                                .child(Input::new(&self.forward_target_host)),
                        )
                        .child(
                            v_flex()
                                .w(px(80.))
                                .gap_1()
                                .child(field_label("Port", cx))
                                .child(Input::new(&self.forward_target_port)),
                        ),
                )
            })
            .when(is_dynamic, |el| {
                el.child(div().text_xs().text_color(muted_fg).child(
                    "A SOCKS5 proxy has no fixed target: every client picks its own, \
                             and names are resolved on the host.",
                ))
            })
            .child(
                Checkbox::new("forward-auto-start")
                    .checked(self.forward_auto_start)
                    .label("Start automatically on connect")
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.forward_auto_start = *checked;
                        cx.notify();
                    })),
            )
            .children(
                self.forward_error
                    .as_ref()
                    .map(|msg| div().text_xs().text_color(danger).child(msg.clone())),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .mt_2()
                    .child(
                        Button::new("forward-cancel")
                            .ghost()
                            .child("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.form_state = FormState::Main;
                                this.forward_error = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("forward-save")
                            .primary()
                            .child("Save forward")
                            .on_click(cx.listener(|this, _, _, cx| this.save_forward(cx))),
                    ),
            )
    }
}

/// Where a forward listens. Loopback unless the user explicitly picked every
/// interface, which lets anyone on the network through the tunnel.
fn bind_address_for(kind: ForwardKind, is_public: bool) -> &'static str {
    match (kind, is_public) {
        (_, true) => "0.0.0.0",
        (ForwardKind::Remote, false) => "localhost",
        (_, false) => "127.0.0.1",
    }
}

/// Check one forward before it joins the list. Returns the parsed bind and
/// target ports. `others` is every other forward on this host, so two cannot
/// claim the same listening port.
fn validate_forward(
    kind: ForwardKind,
    name: &str,
    bind_address: &str,
    bind_port: &str,
    target_host: &str,
    target_port: &str,
    others: &[PortForward],
) -> Result<(u16, u16), String> {
    if name.is_empty() {
        return Err("Give the forward a name.".to_string());
    }

    let bind_port: u16 = bind_port
        .parse()
        .map_err(|_| "The listen port must be a number from 1 to 65535.".to_string())?;
    // 0 asks the server to pick a port, which only makes sense for -R: a local
    // listener nobody knows the port of is useless.
    if bind_port == 0 && kind != ForwardKind::Remote {
        return Err("The listen port must be a number from 1 to 65535.".to_string());
    }

    let target_port = match kind {
        // Every SOCKS5 client names its own target, so there is nothing here.
        ForwardKind::Dynamic => 0,
        _ => {
            if target_host.is_empty() {
                return Err("Enter the host and port to forward to.".to_string());
            }
            match target_port.parse::<u16>() {
                Ok(port) if port > 0 => port,
                _ => return Err("The target port must be a number from 1 to 65535.".to_string()),
            }
        }
    };

    // A -R forward listens on the server; the other two listen here, so only
    // forwards on the same side can collide.
    let is_local_side = kind != ForwardKind::Remote;
    let clash = bind_port != 0
        && others.iter().any(|other| {
            (other.kind != ForwardKind::Remote) == is_local_side
                && other.bind_address == bind_address
                && other.bind_port == bind_port
        });
    if clash {
        return Err(format!(
            "Another forward already listens on {bind_address}:{bind_port}."
        ));
    }

    Ok((bind_port, target_port))
}

/// Small neutral pill showing a forward's direction. Mirrors the settings
/// screen's proxy pill.
fn forward_kind_pill(kind: ForwardKind, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    div()
        .flex_none()
        .h(px(16.))
        .px(px(5.))
        .flex()
        .items_center()
        .rounded(px(3.))
        .bg(theme.input)
        .border_1()
        .border_color(theme.title_bar_border)
        .text_size(px(10.))
        .text_color(theme.foreground)
        .child(kind.display_label())
}

/// One row of the jump-host picker. An imported host can carry an empty
/// label, so the address stands in for it rather than leaving a bare
/// separator with no name in front of it.
fn jump_option_label(candidate: &ServerEntrySummary) -> String {
    if candidate.label.is_empty() {
        return format!("{}:{}", candidate.host, candidate.port);
    }
    format!(
        "{} · {}:{}",
        candidate.label, candidate.host, candidate.port
    )
}

/// Remove every whitespace character (spaces, tabs, and stray \r / \n from
/// pasted text). Used for fields that can never contain whitespace.
fn strip_whitespace(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn field_label(text: &'static str, cx: &App) -> impl IntoElement {
    div()
        .text_size(px(12.))
        .text_color(cx.theme().muted_foreground)
        .child(text)
}

const COMMAND_OPTION: &str = "Command for this host";

/// A field with its label above it.
fn labeled(label: &'static str, cx: &App, field: impl IntoElement) -> gpui::Div {
    v_flex()
        .gap(px(6.))
        .child(field_label(label, cx))
        .child(field)
}

/// Title of a view that takes the form's place, after its Back button.
fn sub_view_title(text: &'static str, cx: &App) -> impl IntoElement {
    div()
        .text_size(px(15.))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(cx.theme().foreground)
        .child(text)
}

/// Heading of one part of the form.
fn section_title(text: &'static str, cx: &App) -> impl IntoElement {
    div()
        .text_size(px(13.))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(cx.theme().foreground)
        .child(text)
}

/// The `@` and `:` between user, host and port, level with the fields.
fn address_separator(text: &'static str) -> impl IntoElement {
    div()
        .flex_none()
        .h(px(32.))
        .flex()
        .items_center()
        .text_size(px(14.))
        .text_color(crate::theme::text_dim())
        .child(text)
}

/// A row of options of which one is chosen, drawn as one segmented control.
fn segmented<T: Copy + PartialEq + 'static>(
    id: &'static str,
    options: &[(T, &'static str)],
    selected: T,
    cx: &Context<ServerFormView>,
    on_pick: impl Fn(&mut ServerFormView, T, &mut Context<ServerFormView>) + Clone + 'static,
) -> impl IntoElement {
    let fg = cx.theme().foreground;
    h_flex()
        .flex_none()
        .p(px(2.))
        .gap(px(2.))
        .rounded(px(7.))
        .bg(crate::theme::field())
        .children(options.iter().enumerate().map(|(ix, &(value, label))| {
            let is_selected = value == selected;
            let on_pick = on_pick.clone();
            div()
                .id((id, ix))
                // A tab stop: Enter or Space on it picks it, like a radio.
                .focusable()
                .tab_stop(true)
                .h(px(26.))
                .px(px(12.))
                .flex()
                .items_center()
                .rounded(px(5.))
                .border_1()
                .border_color(gpui::transparent_black())
                .focus_visible(|s| s.border_color(crate::theme::accent()))
                .text_size(px(12.5))
                .cursor_pointer()
                .when(is_selected, |el| {
                    el.bg(crate::theme::element()).text_color(fg)
                })
                .when(!is_selected, |el| {
                    el.text_color(crate::theme::text_dim())
                        .hover(move |s| s.text_color(fg))
                })
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| on_pick(this, value, cx)))
        }))
}

/// How many characters one edit put in, going from `before` to `after`.
/// Typing gives 0 or 1; a paste gives more, also when it replaced a selection.
fn inserted_char_count(before: &str, after: &str) -> usize {
    let before: Vec<char> = before.chars().collect();
    let after: Vec<char> = after.chars().collect();
    let shorter = before.len().min(after.len());
    let prefix = before
        .iter()
        .zip(&after)
        .take_while(|(a, b)| a == b)
        .count();
    let suffix = before
        .iter()
        .rev()
        .zip(after.iter().rev())
        .take(shorter - prefix)
        .take_while(|(a, b)| a == b)
        .count();
    after.len() - prefix - suffix
}

/// Private keys are a few kilobytes. The cap keeps a file picked by mistake
/// (a disk image) from being read into memory on the UI thread.
const MAX_KEY_FILE_BYTES: u64 = 1024 * 1024;

pub(crate) fn read_key_file(path: &Path) -> Result<String, String> {
    let size = std::fs::metadata(path)
        .map_err(|e| format!("Failed to read file: {e}"))?
        .len();
    if size > MAX_KEY_FILE_BYTES {
        return Err("That file is too large to be a key.".to_string());
    }
    std::fs::read_to_string(path).map_err(|e| format!("Failed to read file: {e}"))
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The button that opens a dropdown: the current value on the left, a caret on
/// the right, and the probe that tells the panel where to sit.
fn dropdown_trigger(
    id: &'static str,
    label: String,
    label_color: Hsla,
    anchor: &Rc<Cell<Point<Pixels>>>,
    trigger: &Rc<Cell<Bounds<Pixels>>>,
    cx: &Context<ServerFormView>,
    on_toggle: impl Fn(&mut ServerFormView, &mut Context<ServerFormView>) + 'static,
) -> AnyElement {
    let theme = cx.theme();
    let muted_fg = theme.muted_foreground;

    div()
        .id(id)
        .relative()
        .w_full()
        .h_8()
        .flex()
        .items_center()
        .px(px(10.))
        .rounded(px(7.))
        .border_1()
        .border_color(crate::theme::border())
        .bg(crate::theme::field())
        .cursor_pointer()
        .hover(|s| s.bg(crate::theme::element_hover()))
        .text_size(px(13.))
        .text_color(label_color)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseDownEvent, _, cx| on_toggle(this, cx)),
        )
        .child(trigger_probe(anchor.clone(), trigger.clone()))
        .child(
            h_flex()
                .w_full()
                .justify_between()
                .items_center()
                .child(div().min_w_0().truncate().child(label))
                .child(
                    Icon::new(IconName::ChevronDown)
                        .size(px(14.))
                        .text_color(muted_fg),
                ),
        )
        .into_any_element()
}

/// One row inside a dropdown panel.
fn dropdown_option(
    id: ElementId,
    label: String,
    color: Hsla,
    cx: &Context<ServerFormView>,
    on_pick: impl Fn(&mut ServerFormView, &mut Context<ServerFormView>) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .h(px(30.))
        .flex()
        .items_center()
        .px(px(8.))
        .rounded(px(6.))
        .text_size(px(13.))
        .truncate()
        .text_color(color)
        .cursor_pointer()
        .hover(|s| s.bg(crate::theme::field()))
        .active(|s| s.bg(crate::theme::element_active()))
        .on_click(cx.listener(move |this, _, _, cx| on_pick(this, cx)))
        .child(label)
        .into_any_element()
}

/// Where a dropdown panel stacks: with gpui-component's own popups
/// (`POPUP_PRIORITY`), above dialogs and below tooltips.
const DROPDOWN_PRIORITY: usize = 100;

/// The panel a dropdown opens. It hangs off the form's root, where nothing
/// clips it, and finds its place from the trigger's probe.
fn dropdown_panel(
    id: &'static str,
    anchor: &Rc<Cell<Point<Pixels>>>,
    trigger: &Rc<Cell<Bounds<Pixels>>>,
    items: Vec<AnyElement>,
    cx: &Context<ServerFormView>,
    on_close: impl Fn(&mut ServerFormView, &mut Context<ServerFormView>) + 'static,
) -> AnyElement {
    let trigger = trigger.clone();

    // Deferred so the panel is not cut off at the dialog's edge. Dialogs are
    // deferred too, at priority 10 and up, so the panel needs a higher one.
    let panel = AnchoredToTrigger {
        anchor: Anchor::TopLeft,
        anchor_point: anchor.clone(),
        margin: px(8.),
        child: div()
            .id(id)
            .occlude()
            .on_mouse_down_out(cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                // This fires in the capture phase, before the trigger's own
                // toggle. Letting it close the panel on a trigger click would
                // just let the toggle reopen it.
                if trigger.get().contains(&event.position) {
                    return;
                }
                on_close(this, cx);
            }))
            .bg(crate::theme::pop_surface())
            .border_1()
            .border_color(crate::theme::float_border())
            .rounded(px(9.))
            .shadow(crate::ui::modal_shadow())
            .p(px(4.))
            .min_w(px(220.))
            .max_w(px(360.))
            .max_h(px(260.))
            .overflow_y_scroll()
            .children(items)
            .into_any_element(),
    };
    deferred(panel)
        .with_priority(DROPDOWN_PRIORITY)
        .into_any_element()
}

/// An overlay that reports its host element's bottom-left corner into
/// `anchor_point` at prepaint, so a dropdown can drop from under the field
/// instead of from wherever the click landed. `absolute` + `size_full` keeps it
/// out of layout, so the field's own size is untouched.
fn trigger_probe(
    anchor_point: Rc<Cell<Point<Pixels>>>,
    trigger_bounds: Rc<Cell<Bounds<Pixels>>>,
) -> impl IntoElement {
    canvas(
        move |bounds: Bounds<Pixels>, _, _| {
            let corner = bounds.corner(Anchor::BottomLeft);
            anchor_point.set(point(corner.x, corner.y + px(4.)));
            trigger_bounds.set(bounds);
        },
        |_, _, _, _| {},
    )
    .absolute()
    .size_full()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forward(kind: ForwardKind, bind_address: &str, bind_port: u16) -> PortForward {
        PortForward {
            id: Uuid::new_v4(),
            name: "existing".into(),
            kind,
            bind_address: bind_address.into(),
            bind_port,
            target_host: "127.0.0.1".into(),
            target_port: 3000,
            auto_start: false,
        }
    }

    fn validate(
        kind: ForwardKind,
        bind_port: &str,
        target_host: &str,
        target_port: &str,
    ) -> Result<(u16, u16), String> {
        validate_forward(
            kind,
            "db",
            "127.0.0.1",
            bind_port,
            target_host,
            target_port,
            &[],
        )
    }

    #[test]
    fn local_and_remote_forwards_need_a_target() {
        assert!(validate(ForwardKind::Local, "8080", "", "5432").is_err());
        assert!(validate(ForwardKind::Remote, "8080", "", "5432").is_err());
        assert!(validate(ForwardKind::Local, "8080", "db.internal", "").is_err());
        assert!(validate(ForwardKind::Local, "8080", "db.internal", "0").is_err());
        assert_eq!(
            validate(ForwardKind::Local, "8080", "db.internal", "5432"),
            Ok((8080, 5432))
        );
    }

    #[test]
    fn typing_inserts_at_most_one_character() {
        assert_eq!(inserted_char_count("", "w"), 1);
        assert_eq!(inserted_char_count("web", "web:"), 1);
        assert_eq!(inserted_char_count("wb", "web"), 1);
        assert_eq!(inserted_char_count("web", "we"), 0);
        assert_eq!(inserted_char_count("web", "web"), 0);
        // One key over a selection.
        assert_eq!(inserted_char_count("web.example.com", "w"), 0);
        assert_eq!(inserted_char_count("aaa", "aaaa"), 1);
    }

    #[test]
    fn a_paste_inserts_more_than_one_character() {
        assert_eq!(inserted_char_count("", "deploy@web:2222"), 15);
        assert_eq!(inserted_char_count("old.example.com", "root@new"), 8);
        assert_eq!(inserted_char_count("web", "web:2222"), 5);
    }

    #[test]
    fn a_socks5_forward_needs_no_target() {
        assert_eq!(
            validate(ForwardKind::Dynamic, "1080", "", ""),
            Ok((1080, 0))
        );
    }

    #[test]
    fn the_listen_port_must_be_a_real_port() {
        assert!(validate(ForwardKind::Local, "65536", "db", "5432").is_err());
        assert!(validate(ForwardKind::Local, "eighty", "db", "5432").is_err());
        assert!(validate(ForwardKind::Local, "", "db", "5432").is_err());
        // 0 means "you pick one", which only a server can do.
        assert!(validate(ForwardKind::Local, "0", "db", "5432").is_err());
        assert_eq!(
            validate(ForwardKind::Remote, "0", "127.0.0.1", "3000"),
            Ok((0, 3000))
        );
    }

    #[test]
    fn a_key_file_over_the_size_cap_is_refused_without_reading_it() {
        let dir = tempfile::tempdir().unwrap();
        let small = dir.path().join("id_ed25519");
        std::fs::write(&small, "-----BEGIN OPENSSH PRIVATE KEY-----\n").unwrap();
        assert_eq!(
            read_key_file(&small).as_deref(),
            Ok("-----BEGIN OPENSSH PRIVATE KEY-----\n"),
        );

        let large = dir.path().join("disk.img");
        std::fs::File::create(&large)
            .unwrap()
            .set_len(MAX_KEY_FILE_BYTES + 1)
            .unwrap();
        assert_eq!(
            read_key_file(&large),
            Err("That file is too large to be a key.".to_string()),
        );
    }

    #[test]
    fn a_forward_needs_a_name() {
        let unnamed = validate_forward(ForwardKind::Dynamic, "", "127.0.0.1", "1080", "", "", &[]);
        assert!(unnamed.is_err());
    }

    #[test]
    fn two_forwards_cannot_listen_on_the_same_address_and_port() {
        let taken = [forward(ForwardKind::Local, "127.0.0.1", 8080)];
        let clash = validate_forward(
            ForwardKind::Dynamic,
            "socks",
            "127.0.0.1",
            "8080",
            "",
            "",
            &taken,
        );
        assert!(clash.is_err());

        // A different port, a different address, and the server side are all fine.
        let free_port = validate_forward(
            ForwardKind::Local,
            "other",
            "127.0.0.1",
            "8081",
            "db",
            "5432",
            &taken,
        );
        assert!(free_port.is_ok());
        let other_address = validate_forward(
            ForwardKind::Local,
            "other",
            "0.0.0.0",
            "8080",
            "db",
            "5432",
            &taken,
        );
        assert!(other_address.is_ok());
        let server_side = validate_forward(
            ForwardKind::Remote,
            "other",
            "127.0.0.1",
            "8080",
            "db",
            "5432",
            &taken,
        );
        assert!(server_side.is_ok());
    }

    #[test]
    fn the_bind_address_is_loopback_unless_the_user_asks_otherwise() {
        assert_eq!(bind_address_for(ForwardKind::Local, false), "127.0.0.1");
        assert_eq!(bind_address_for(ForwardKind::Dynamic, false), "127.0.0.1");
        // -R binds on the server, where loopback is spelled the OpenSSH way.
        assert_eq!(bind_address_for(ForwardKind::Remote, false), "localhost");
        assert_eq!(bind_address_for(ForwardKind::Local, true), "0.0.0.0");
        assert_eq!(bind_address_for(ForwardKind::Remote, true), "0.0.0.0");
        for kind in [
            ForwardKind::Local,
            ForwardKind::Remote,
            ForwardKind::Dynamic,
        ] {
            assert!(is_loopback_bind(bind_address_for(kind, false)));
            assert!(!is_loopback_bind(bind_address_for(kind, true)));
        }
    }
}
