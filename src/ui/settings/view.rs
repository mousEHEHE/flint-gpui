use std::path::PathBuf;

use gpui::prelude::*;
use gpui::{
    actions, div, px, App, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable,
    Hsla, KeyBinding, MouseButton, SharedString, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    dialog::{DialogAction, DialogFooter, DialogHeader, DialogTitle},
    h_flex,
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    v_flex, ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _,
    WindowExt as _,
};
use uuid::Uuid;

use crate::preferences::{
    Preferences, TerminalFont, TerminalScheme, ThemePreference, UpdateChecks,
    DEFAULT_TERMINAL_FONT_SIZE,
};
use crate::ui::server_form::read_key_file;
use crate::ui::sync_service::SyncService;
use crate::ui::terminal_palette::TerminalColors;
use crate::ui::update_service::UpdateService;
use crate::ui::{dialog_chrome, VaultState};
use crate::vault::{
    ProxyAuth, ProxyChoice, ProxyDefSummary, ProxyKind, ProxyMode, ProxyRouting, Snippet,
    SnippetBody, SshKey, SshKeySummary, VaultError,
};

use super::sync_pane::SyncPane;
use super::teams_pane::TeamsPane;
use super::vault_pane::{VaultPane, VaultPaneEvent};
use super::SettingsSection;

actions!(settings, [CloseSettings]);

pub fn init(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", CloseSettings, Some("Settings"))]);
}

pub enum SettingsViewEvent {
    Close,
    LockRequested,
    RunSnippet(Uuid),
}

#[derive(Clone, PartialEq)]
enum PemSource {
    File,
    Paste,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SnippetEditMode {
    View,
    Edit,
}

#[derive(Clone, Copy)]
enum SettingsMode {
    Vault,
    Sync,
    Teams,
    Appearance,
    Terminal,
    KeysList,
    AddKey,
    SnippetsList,
    ProxyList,
    ProxyEditor,
    About,
}

/// Whether the proxy editor is creating a new profile or editing an existing one.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ProxyEditorMode {
    Add,
    Edit(Uuid),
}

pub struct SettingsView {
    focus_handle: FocusHandle,
    vault_state: Entity<VaultState>,
    active_section: SettingsSection,
    mode: SettingsMode,
    // keys list and detail
    selected_key_id: Option<Uuid>,
    key_search: Entity<InputState>,
    // (key_id, Result<ssh-public-key-line, error>) cached for selected key
    pubkey_cache: Option<(Uuid, Result<String, String>)>,
    // add key form
    add_name: Entity<InputState>,
    add_pem_source: PemSource,
    add_file_path: Option<PathBuf>,
    add_paste: Entity<TextareaState>,
    add_passphrase: Entity<InputState>,
    /// The chosen or pasted key is encrypted, so the passphrase field is
    /// shown and required.
    add_encrypted: bool,
    add_error: Option<String>,
    // snippet editor
    editing_snippet_id: Option<Uuid>,
    edit_name: Entity<InputState>,
    edit_description: Entity<InputState>,
    edit_content: Entity<TextareaState>,
    edit_error: Option<String>,
    // snippets list and detail
    selected_snippet_id: Option<Uuid>,
    /// True while the user is writing a snippet that is not in the vault yet.
    /// Never true together with a selected snippet.
    creating_snippet: bool,
    snippet_search: Entity<InputState>,
    snippet_view_mode: SnippetEditMode,
    // proxy editor
    proxy_editor_mode: ProxyEditorMode,
    proxy_kind: ProxyKind,
    proxy_name: Entity<InputState>,
    proxy_host: Entity<InputState>,
    proxy_port: Entity<InputState>,
    proxy_user: Entity<InputState>,
    proxy_pass: Entity<InputState>,
    proxy_error: Option<String>,
    /// The terminal fonts this machine has. Listing system fonts is slow, so
    /// it is done once when the dialog opens.
    terminal_fonts: Vec<TerminalFont>,
    vault_pane: Entity<VaultPane>,
    sync_pane: Entity<SyncPane>,
    teams_pane: Entity<TeamsPane>,
}

impl EventEmitter<SettingsViewEvent> for SettingsView {}

impl Focusable for SettingsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl SettingsView {
    pub fn new(
        vault_state: Entity<VaultState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&vault_state, |_, _, cx| cx.notify()).detach();
        // The About page shows how the last update check ended.
        cx.observe(&UpdateService::global(cx), |_, _, cx| cx.notify())
            .detach();

        let add_paste = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(6)
                .placeholder("-----BEGIN OPENSSH PRIVATE KEY-----")
        });
        // Pasting an encrypted key should reveal the passphrase field at once.
        cx.subscribe(&add_paste, |this: &mut Self, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) && this.add_pem_source == PemSource::Paste {
                this.refresh_add_encrypted(cx);
                cx.notify();
            }
        })
        .detach();

        let edit_content = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(12)
                .placeholder("echo 'hello from snippet'")
        });

        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);

        let key_search = cx.new(|cx| InputState::new(window, cx).placeholder("filter by name…"));

        let snippet_search =
            cx.new(|cx| InputState::new(window, cx).placeholder("filter snippets…"));

        let sync_pane = cx.new(|cx| SyncPane::new(vault_state.clone(), window, cx));
        let teams_pane = cx.new(|cx| TeamsPane::new(vault_state.clone(), window, cx));
        let vault_pane = cx.new(|cx| VaultPane::new(vault_state.clone(), window, cx));
        cx.subscribe(
            &vault_pane,
            |_, _, event: &VaultPaneEvent, cx| match event {
                VaultPaneEvent::LockRequested => cx.emit(SettingsViewEvent::LockRequested),
            },
        )
        .detach();

        Self {
            focus_handle,
            vault_state,
            active_section: SettingsSection::Keys,
            mode: SettingsMode::KeysList,
            selected_key_id: None,
            key_search,
            pubkey_cache: None,
            add_name: cx.new(|cx| InputState::new(window, cx).placeholder("e.g. Production key")),
            add_pem_source: PemSource::File,
            add_file_path: None,
            add_paste,
            add_passphrase: cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder("Passphrase")
            }),
            add_encrypted: false,
            add_error: None,
            editing_snippet_id: None,
            edit_name: cx.new(|cx| InputState::new(window, cx).placeholder("e.g. Restart nginx")),
            edit_description: cx
                .new(|cx| InputState::new(window, cx).placeholder("Optional one-line description")),
            edit_content,
            edit_error: None,
            selected_snippet_id: None,
            creating_snippet: false,
            snippet_search,
            snippet_view_mode: SnippetEditMode::View,
            proxy_editor_mode: ProxyEditorMode::Add,
            proxy_kind: ProxyKind::Socks5,
            proxy_name: cx.new(|cx| InputState::new(window, cx).placeholder("e.g. Office SOCKS")),
            proxy_host: cx.new(|cx| InputState::new(window, cx).placeholder("127.0.0.1")),
            proxy_port: cx.new(|cx| InputState::new(window, cx).placeholder("1080")),
            proxy_user: cx.new(|cx| InputState::new(window, cx).placeholder("optional")),
            proxy_pass: cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder("optional")
            }),
            proxy_error: None,
            terminal_fonts: TerminalFont::available(&cx.text_system().all_font_names()),
            vault_pane,
            sync_pane,
            teams_pane,
        }
    }

    fn enter_add_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.add_name
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.add_pem_source = PemSource::File;
        self.add_file_path = None;
        self.add_paste
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.add_passphrase
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.add_encrypted = false;
        self.add_error = None;
        self.mode = SettingsMode::AddKey;
        cx.notify();
    }

    /// Re-detect whether the chosen/pasted key is encrypted, which controls
    /// the passphrase field's visibility.
    fn refresh_add_encrypted(&mut self, cx: &Context<Self>) {
        let pem = match self.add_pem_source {
            PemSource::File => self
                .add_file_path
                .as_ref()
                .and_then(|p| read_key_file(p).ok()),
            PemSource::Paste => Some(self.add_paste.read(cx).value().to_string()),
        };
        self.add_encrypted = pem
            .as_deref()
            .map(crate::ssh::key_is_encrypted)
            .unwrap_or(false);
    }

    fn save_key(&mut self, cx: &mut Context<Self>) {
        let name = self.add_name.read(cx).value().to_string();
        if name.trim().is_empty() {
            self.add_error = Some("Name is required.".to_string());
            cx.notify();
            return;
        }

        let pem = match self.add_pem_source {
            PemSource::File => {
                let Some(ref path) = self.add_file_path else {
                    self.add_error = Some("Choose a key file first.".to_string());
                    cx.notify();
                    return;
                };
                match read_key_file(path) {
                    Ok(content) => content,
                    Err(message) => {
                        self.add_error = Some(message);
                        cx.notify();
                        return;
                    }
                }
            }
            PemSource::Paste => {
                let pem = self.add_paste.read(cx).value().to_string();
                if pem.trim().is_empty() {
                    self.add_error = Some("Paste the PEM key content.".to_string());
                    cx.notify();
                    return;
                }
                pem
            }
        };

        let passphrase = {
            let v = self.add_passphrase.read(cx).value().to_string();
            if v.is_empty() {
                None
            } else {
                Some(v)
            }
        };

        // Checked here because the vault would only say "invalid key".
        match crate::ssh::check_key_passphrase(&pem, passphrase.as_deref()) {
            Some(crate::ssh::KeyIssue::MissingPassphrase) => {
                self.add_encrypted = true;
                self.add_error = Some("This key is encrypted. Enter its passphrase.".to_string());
                cx.notify();
                return;
            }
            Some(crate::ssh::KeyIssue::WrongPassphrase) => {
                self.add_error = Some("Wrong passphrase for this key.".to_string());
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
            Some(Ok(_)) => {
                self.mode = SettingsMode::KeysList;
                self.add_error = None;
            }
            Some(Err(VaultError::InvalidKey(msg))) => {
                self.add_error = Some(format!("Invalid key: {msg}"));
            }
            Some(Err(e)) => {
                tracing::error!(?e, "failed to add key");
                self.add_error = Some(format!("Could not save the key: {e}"));
            }
            None => {}
        }
        cx.notify();
    }

    fn apply_rename(&mut self, id: Uuid, new_name: String, cx: &mut Context<Self>) {
        let result = self.vault_state.update(cx, |vs, _| {
            vs.manager_mut().map(|vm| vm.rename_key(id, new_name))
        });
        if let Some(Err(e)) = result {
            tracing::error!(?e, "failed to rename key");
        }
        cx.notify();
    }

    fn confirm_delete(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let info = self.vault_state.read(cx).manager().map(|vm| {
            let name = vm.get_key(id).map(|k| k.name.clone()).unwrap_or_default();
            let usage = vm.servers_using_key(id);
            (name, usage)
        });
        let (key_name, usage) = info.unwrap_or_default();

        let msg = if usage.is_empty() {
            format!("Delete key \"{}\"? This cannot be undone.", key_name)
        } else {
            let names: Vec<_> = usage.iter().map(|user| user.label.as_str()).collect();
            let is_single = usage.len() == 1;
            format!(
                "Key \"{}\" is used by {} host{}: {}.\n\
                 {}\n\
                 Delete anyway?",
                key_name,
                usage.len(),
                if is_single { "" } else { "s" },
                names.join(", "),
                if is_single {
                    "That host won't connect until it gets another key."
                } else {
                    "Those hosts won't connect until they get another key."
                }
            )
        };

        let weak = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let (w2, m2) = (weak.clone(), msg.clone());
            dialog_chrome(dialog).w(px(380.)).child(
                v_flex()
                    .p(px(12.))
                    .gap(px(8.))
                    .child(DialogHeader::new().child(DialogTitle::new().child("Delete key")))
                    .child(div().text_sm().child(m2))
                    .child(
                        DialogFooter::new().child(
                            DialogAction::new().child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        Button::new("del-cancel")
                                            .ghost()
                                            .child("Cancel")
                                            .on_click(|_, window, cx| window.close_dialog(cx)),
                                    )
                                    .child(
                                        Button::new("del-confirm")
                                            .danger()
                                            .child("Delete")
                                            .on_click(move |_, window, cx| {
                                                w2.update(cx, |this, cx| {
                                                    let result =
                                                        this.vault_state.update(cx, |vs, _| {
                                                            vs.manager_mut()
                                                                .map(|vm| vm.delete_key(id))
                                                        });
                                                    if let Some(Err(e)) = result {
                                                        tracing::error!(?e, "failed to delete key");
                                                    }
                                                    cx.notify();
                                                })
                                                .ok();
                                                window.close_dialog(cx);
                                            }),
                                    ),
                            ),
                        ),
                    ),
            )
        });
    }

    fn open_rename_dialog(
        &mut self,
        id: Uuid,
        current_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rename_input = cx
            .new(|cx| InputState::new(window, cx).default_value(SharedString::from(current_name)));
        let weak = cx.weak_entity();
        let ri = rename_input.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let (ri2, w2) = (ri.clone(), weak.clone());
            dialog_chrome(dialog).w(px(320.)).child(
                v_flex()
                    .p(px(12.))
                    .gap(px(8.))
                    .child(DialogHeader::new().child(DialogTitle::new().child("Rename key")))
                    .child(Input::new(&ri2))
                    .child(
                        DialogFooter::new().child(
                            DialogAction::new().child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        Button::new("rename-cancel")
                                            .ghost()
                                            .child("Cancel")
                                            .on_click(|_, window, cx| window.close_dialog(cx)),
                                    )
                                    .child(
                                        Button::new("rename-save")
                                            .primary()
                                            .child("Save")
                                            .on_click(move |_, window, cx| {
                                                let new_name =
                                                    ri2.read(cx).value().trim().to_string();
                                                if new_name.is_empty() {
                                                    return;
                                                }
                                                w2.update(cx, |this, cx| {
                                                    this.apply_rename(id, new_name, cx);
                                                })
                                                .ok();
                                                window.close_dialog(cx);
                                            }),
                                    ),
                            ),
                        ),
                    ),
            )
        });
    }

    fn enter_add_proxy(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.proxy_editor_mode = ProxyEditorMode::Add;
        self.proxy_kind = ProxyKind::Socks5;
        for input in [
            &self.proxy_name,
            &self.proxy_host,
            &self.proxy_port,
            &self.proxy_user,
            &self.proxy_pass,
        ] {
            input.update(cx, |s, cx| s.set_value("", window, cx));
        }
        self.proxy_error = None;
        self.mode = SettingsMode::ProxyEditor;
        cx.notify();
    }

    fn enter_edit_proxy(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let data = self
            .vault_state
            .read(cx)
            .manager()
            .and_then(|vm| vm.get_proxy(id).ok())
            .map(|p| {
                (
                    p.name.clone(),
                    p.kind,
                    p.host.clone(),
                    p.port,
                    p.auth.clone(),
                )
            });
        let Some((name, kind, host, port, auth)) = data else {
            return;
        };
        self.proxy_editor_mode = ProxyEditorMode::Edit(id);
        self.proxy_kind = kind;
        self.proxy_name.update(cx, |s, cx| {
            s.set_value(SharedString::from(name), window, cx)
        });
        self.proxy_host.update(cx, |s, cx| {
            s.set_value(SharedString::from(host), window, cx)
        });
        self.proxy_port.update(cx, |s, cx| {
            s.set_value(SharedString::from(port.to_string()), window, cx)
        });
        let (user, pass) = auth.map(|a| (a.username, a.password)).unwrap_or_default();
        self.proxy_user.update(cx, |s, cx| {
            s.set_value(SharedString::from(user), window, cx)
        });
        self.proxy_pass.update(cx, |s, cx| {
            s.set_value(SharedString::from(pass), window, cx)
        });
        self.proxy_error = None;
        self.mode = SettingsMode::ProxyEditor;
        cx.notify();
    }

    fn save_proxy(&mut self, cx: &mut Context<Self>) {
        let name = self.proxy_name.read(cx).value().trim().to_string();
        let host = self.proxy_host.read(cx).value().trim().to_string();
        let port_str = self.proxy_port.read(cx).value().trim().to_string();
        let user = self.proxy_user.read(cx).value().to_string();
        let pass = self.proxy_pass.read(cx).value().to_string();

        if name.is_empty() {
            self.proxy_error = Some("Name is required.".to_string());
            cx.notify();
            return;
        }
        if host.is_empty() {
            self.proxy_error = Some("Host is required.".to_string());
            cx.notify();
            return;
        }
        let port: u16 = match port_str.parse() {
            Ok(p) if p >= 1 => p,
            _ => {
                self.proxy_error = Some("Port must be a number between 1 and 65535.".to_string());
                cx.notify();
                return;
            }
        };
        let auth = if user.is_empty() && pass.is_empty() {
            None
        } else {
            Some(ProxyAuth {
                username: user,
                password: pass,
            })
        };
        let kind = self.proxy_kind;
        let mode = self.proxy_editor_mode;

        let result = self.vault_state.update(cx, |vs, _| {
            vs.manager_mut().map(|vm| match mode {
                ProxyEditorMode::Add => vm.add_proxy(name, kind, host, port, auth).map(|_| ()),
                ProxyEditorMode::Edit(id) => vm.update_proxy(id, name, kind, host, port, auth),
            })
        });
        match result {
            Some(Ok(())) => {
                self.mode = SettingsMode::ProxyList;
                self.proxy_error = None;
            }
            Some(Err(e)) => self.proxy_error = Some(format!("Failed to save: {e}")),
            None => self.proxy_error = Some("Vault is locked.".to_string()),
        }
        cx.notify();
    }

    fn confirm_delete_proxy(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let name = self
            .vault_state
            .read(cx)
            .manager()
            .and_then(|vm| vm.get_proxy(id).ok())
            .map(|p| p.name.clone())
            .unwrap_or_default();
        let msg = format!(
            "Delete proxy \"{name}\"? Hosts routed through it will fall back to a direct \
             connection. This cannot be undone."
        );
        let weak = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let (w2, m2) = (weak.clone(), msg.clone());
            dialog_chrome(dialog).w(px(380.)).child(
                v_flex()
                    .p(px(12.))
                    .gap(px(8.))
                    .child(DialogHeader::new().child(DialogTitle::new().child("Delete proxy")))
                    .child(div().text_sm().child(m2))
                    .child(
                        DialogFooter::new().child(
                            DialogAction::new().child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        Button::new("proxy-del-cancel")
                                            .ghost()
                                            .child("Cancel")
                                            .on_click(|_, window, cx| window.close_dialog(cx)),
                                    )
                                    .child(
                                        Button::new("proxy-del-confirm")
                                            .danger()
                                            .child("Delete")
                                            .on_click(move |_, window, cx| {
                                                w2.update(cx, |this, cx| {
                                                    let result =
                                                        this.vault_state.update(cx, |vs, _| {
                                                            vs.manager_mut()
                                                                .map(|vm| vm.delete_proxy(id))
                                                        });
                                                    if let Some(Err(e)) = result {
                                                        tracing::error!(
                                                            ?e,
                                                            "failed to delete proxy"
                                                        );
                                                    }
                                                    cx.notify();
                                                })
                                                .ok();
                                                window.close_dialog(cx);
                                            }),
                                    ),
                            ),
                        ),
                    ),
            )
        });
    }

    /// Read the current routing, apply `f`, and persist it.
    fn update_routing(&mut self, f: impl FnOnce(&mut ProxyRouting), cx: &mut Context<Self>) {
        let Some(mut routing) = self
            .vault_state
            .read(cx)
            .manager()
            .map(|vm| vm.proxy_routing().clone())
        else {
            return;
        };
        f(&mut routing);
        let result = self.vault_state.update(cx, |vs, _| {
            vs.manager_mut().map(|vm| vm.set_proxy_routing(routing))
        });
        if let Some(Err(e)) = result {
            tracing::error!(?e, "failed to save proxy routing");
        }
        cx.notify();
    }

    fn set_server_proxy_choice(&mut self, id: Uuid, choice: ProxyChoice, cx: &mut Context<Self>) {
        let result = self.vault_state.update(cx, |vs, _| {
            vs.manager_mut()
                .map(|vm| vm.set_entry_proxy_choice(id, choice))
        });
        if let Some(Err(e)) = result {
            tracing::error!(?e, ?id, "failed to save the proxy choice of a host");
        }
        cx.notify();
    }

    fn render_proxy(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let accent = theme.primary;

        let (proxies, routing, mut servers) = {
            let vs = self.vault_state.read(cx);
            match vs.manager() {
                Some(vm) => (
                    vm.list_proxies(),
                    vm.proxy_routing().clone(),
                    vm.list_entries().unwrap_or_default(),
                ),
                None => (Vec::new(), ProxyRouting::default(), Vec::new()),
            }
        };
        servers.sort_by_key(|a| a.label.to_lowercase());

        let header = page_head("Proxies", Some(proxies.len()))
            .px_0()
            .child(div().flex_1())
            .child(
                Button::new("add-proxy-btn")
                    .primary()
                    .small()
                    .icon(IconName::Plus)
                    .child("Add proxy")
                    .on_click(cx.listener(|this, _, window, cx| this.enter_add_proxy(window, cx))),
            );

        let profile_rows: gpui::AnyElement = if proxies.is_empty() {
            div()
                .py(px(18.))
                .text_size(px(13.))
                .text_color(muted_fg)
                .child("No proxies yet. Add one to route hosts through it.")
                .into_any_element()
        } else {
            let count = proxies.len();
            list_card()
                .mx_0()
                .mb_0()
                .children(proxies.iter().enumerate().map(|(ix, p)| {
                    let id = p.id;
                    let meta = format!(
                        "{}:{}{}",
                        p.host,
                        p.port,
                        if p.has_auth { " · auth" } else { "" }
                    );
                    card_row(("proxy-row", ix), ix == 0, ix + 1 == count)
                        .cursor_default()
                        .child(row_tile(
                            Icon::new(IconName::Network),
                            crate::theme::accent(),
                        ))
                        .child(
                            div()
                                .w(px(180.))
                                .flex_shrink(1.)
                                .min_w(px(80.))
                                .text_size(px(13.))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(fg)
                                .truncate()
                                .child(SharedString::from(p.name.clone())),
                        )
                        .child(proxy_kind_pill(p.kind, cx))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .font_family(crate::theme::MONO_FONT)
                                .text_size(px(12.))
                                .text_color(crate::theme::text_dim())
                                .truncate()
                                .child(meta),
                        )
                        .child(icon_button(
                            SharedString::from(format!("proxy-edit-{id}")),
                            Icon::empty().path("icons/pen-line.svg").size(px(12.)),
                            false,
                            cx.listener(move |this, _, window, cx| {
                                this.enter_edit_proxy(id, window, cx)
                            }),
                            cx,
                        ))
                        .child(icon_button(
                            SharedString::from(format!("proxy-del-{id}")),
                            Icon::empty().path("icons/trash.svg").size(px(12.)),
                            true,
                            cx.listener(move |this, _, window, cx| {
                                this.confirm_delete_proxy(id, window, cx)
                            }),
                            cx,
                        ))
                }))
                .into_any_element()
        };

        // Routing controls only make sense once a proxy exists.
        let routing_section: gpui::AnyElement = if proxies.is_empty() {
            div().into_any_element()
        } else {
            let mode_all = routing.mode == ProxyMode::AllExcept;

            let default_chips = h_flex()
                .gap(px(6.))
                .flex_wrap()
                .child(
                    select_chip(
                        "proxy-default-none",
                        "None",
                        routing.default_proxy.is_none(),
                        cx,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.update_routing(|r| r.default_proxy = None, cx)
                    })),
                )
                .children(proxies.iter().map(|p| {
                    let pid = p.id;
                    let active = routing.default_proxy == Some(pid);
                    select_chip(
                        SharedString::from(format!("proxy-def-{pid}")),
                        p.name.clone(),
                        active,
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.update_routing(move |r| r.default_proxy = Some(pid), cx)
                    }))
                }));

            let mode_rows = v_flex()
                .gap(px(8.))
                .child(
                    radio_row(
                        "proxy-mode-all",
                        "Proxy all hosts except the selected ones",
                        mode_all,
                        cx,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.update_routing(|r| r.mode = ProxyMode::AllExcept, cx)
                    })),
                )
                .child(
                    radio_row(
                        "proxy-mode-only",
                        "Proxy only the selected hosts",
                        !mode_all,
                        cx,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.update_routing(|r| r.mode = ProxyMode::OnlySelected, cx)
                    })),
                );

            let server_rows: gpui::AnyElement = if servers.is_empty() {
                div()
                    .py(px(12.))
                    .text_size(px(13.))
                    .text_color(muted_fg)
                    .child("No hosts saved yet.")
                    .into_any_element()
            } else {
                v_flex()
                    .children(servers.iter().enumerate().map(|(ix, s)| {
                        let id = s.id;
                        let choice = s.proxy;
                        let in_set = routing.server_set.contains(&id);
                        let is_rule = choice == ProxyChoice::Default;

                        // Effective proxy after the override + global rule.
                        let eff: Option<&ProxyDefSummary> = match choice {
                            ProxyChoice::Direct => None,
                            ProxyChoice::Use(pid) => proxies.iter().find(|p| p.id == pid),
                            ProxyChoice::Default => {
                                let applies = match routing.mode {
                                    ProxyMode::AllExcept => !in_set,
                                    ProxyMode::OnlySelected => in_set,
                                };
                                if applies {
                                    routing
                                        .default_proxy
                                        .and_then(|pid| proxies.iter().find(|p| p.id == pid))
                                } else {
                                    None
                                }
                            }
                        };
                        let badge_text = match eff {
                            Some(p) => format!("via {}", p.name),
                            None => "direct".to_string(),
                        };

                        // The checkbox only matters while the row follows the
                        // rule: an override wins over it.
                        let checkbox = div()
                            .id(("srv-chk", ix))
                            .w(px(16.))
                            .h(px(16.))
                            .flex_none()
                            .rounded(px(4.))
                            .border_1()
                            .border_color(if in_set && is_rule {
                                accent
                            } else {
                                crate::theme::border_focused()
                            })
                            .bg(if in_set && is_rule {
                                accent
                            } else {
                                crate::theme::modal_rail()
                            })
                            .flex()
                            .items_center()
                            .justify_center()
                            .when(in_set, |el| {
                                el.child(Icon::new(IconName::Check).size(px(11.)))
                            })
                            .when(is_rule, |el| {
                                el.cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.update_routing(
                                            move |r| {
                                                if let Some(pos) =
                                                    r.server_set.iter().position(|x| *x == id)
                                                {
                                                    r.server_set.remove(pos);
                                                } else {
                                                    r.server_set.push(id);
                                                }
                                            },
                                            cx,
                                        )
                                    }))
                            });

                        // Wraps onto a second line rather than run past the
                        // page when there are many proxies or little room.
                        let mut overrides = h_flex()
                            .gap(px(4.))
                            .flex_wrap()
                            .justify_end()
                            .flex_shrink(1.)
                            .min_w_0();
                        overrides = overrides.child(
                            select_chip(("ovr-rule", ix), "Rule", is_rule, cx).on_click(
                                cx.listener(move |this, _, _, cx| {
                                    this.set_server_proxy_choice(id, ProxyChoice::Default, cx)
                                }),
                            ),
                        );
                        overrides = overrides.child(
                            select_chip(
                                ("ovr-direct", ix),
                                "Direct",
                                choice == ProxyChoice::Direct,
                                cx,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.set_server_proxy_choice(id, ProxyChoice::Direct, cx)
                                },
                            )),
                        );
                        for p in &proxies {
                            let pid = p.id;
                            overrides = overrides.child(
                                select_chip(
                                    SharedString::from(format!("ovr-{id}-{pid}")),
                                    p.name.clone(),
                                    choice == ProxyChoice::Use(pid),
                                    cx,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.set_server_proxy_choice(id, ProxyChoice::Use(pid), cx)
                                    },
                                )),
                            );
                        }

                        h_flex()
                            .id(("srv-row", ix))
                            .min_h(px(36.))
                            .py(px(4.))
                            .items_center()
                            .gap(px(10.))
                            .px(px(8.))
                            .rounded(px(7.))
                            .hover(|s| s.bg(crate::theme::field()))
                            .child(checkbox)
                            .child(
                                div()
                                    .flex_shrink(1.)
                                    .min_w(px(60.))
                                    .max_w(px(160.))
                                    .text_size(px(13.))
                                    .text_color(fg)
                                    .truncate()
                                    // A host without a name goes by its address.
                                    .child(SharedString::from(if s.label.is_empty() {
                                        s.host.clone()
                                    } else {
                                        s.label.clone()
                                    })),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(11.5))
                                    .text_color(muted_fg)
                                    .truncate()
                                    .child(SharedString::from(format!(
                                        "{}@{}",
                                        s.username, s.host
                                    ))),
                            )
                            .when(is_rule, |el| {
                                el.child(
                                    div()
                                        .flex_shrink(1.)
                                        .min_w_0()
                                        .truncate()
                                        .text_size(px(11.5))
                                        .text_color(muted_fg)
                                        .child(badge_text),
                                )
                            })
                            .child(overrides)
                    }))
                    .into_any_element()
            };

            v_flex()
                .gap(px(8.))
                .child(section_header("Routing", None))
                .child(
                    v_flex()
                        .gap(px(6.))
                        .child(field_label("Default proxy", cx))
                        .child(default_chips),
                )
                .child(mode_rows)
                .child(section_header("Hosts", Some(servers.len())))
                .child(server_rows)
                .into_any_element()
        };

        v_flex()
            .id("proxy-scroll")
            .size_full()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(20.))
            .pb(px(20.))
            .gap(px(12.))
            .child(header)
            .child(profile_rows)
            .child(routing_section)
    }

    fn render_proxy_editor(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let fg = theme.foreground;

        let title = match self.proxy_editor_mode {
            ProxyEditorMode::Add => "Add proxy",
            ProxyEditorMode::Edit(_) => "Edit proxy",
        };
        let is_socks = self.proxy_kind == ProxyKind::Socks5;

        let fields = v_flex()
            .max_w(px(420.))
            .gap(px(8.))
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(field_label("Name", cx))
                    .child(Input::new(&self.proxy_name).small()),
            )
            .child(
                v_flex().gap(px(6.)).child(field_label("Type", cx)).child(
                    h_flex()
                        .gap(px(12.))
                        .child(
                            radio_row("proxy-kind-socks", "SOCKS5", is_socks, cx).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.proxy_kind = ProxyKind::Socks5;
                                    cx.notify();
                                }),
                            ),
                        )
                        .child(
                            radio_row("proxy-kind-http", "HTTP", !is_socks, cx).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.proxy_kind = ProxyKind::Http;
                                    cx.notify();
                                }),
                            ),
                        ),
                ),
            )
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        v_flex()
                            .flex_1()
                            .gap(px(6.))
                            .child(field_label("Host", cx))
                            .child(Input::new(&self.proxy_host).small()),
                    )
                    .child(
                        v_flex()
                            .w(px(110.))
                            .gap(px(6.))
                            .child(field_label("Port", cx))
                            .child(Input::new(&self.proxy_port).small()),
                    ),
            )
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(field_label("Username (optional)", cx))
                    .child(Input::new(&self.proxy_user).small()),
            )
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(field_label("Password (optional)", cx))
                    .child(Input::new(&self.proxy_pass).small().mask_toggle()),
            )
            .children(
                self.proxy_error
                    .as_ref()
                    .map(|msg| div().text_xs().text_color(theme.danger).child(msg.clone())),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .mt_2()
                    .child(
                        Button::new("proxy-cancel")
                            .ghost()
                            .child("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.mode = SettingsMode::ProxyList;
                                this.proxy_error = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("proxy-save")
                            .primary()
                            .child("Save")
                            .on_click(cx.listener(|this, _, _, cx| this.save_proxy(cx))),
                    ),
            );

        v_flex()
            .id("proxy-editor")
            .size_full()
            .px(px(20.))
            .pt(px(14.))
            .pb(px(20.))
            .gap(px(12.))
            .overflow_y_scroll()
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(back_button(
                        "proxy-back",
                        cx.listener(|this, _, _, cx| {
                            this.mode = SettingsMode::ProxyList;
                            this.proxy_error = None;
                            cx.notify();
                        }),
                        cx,
                    ))
                    .child(
                        div()
                            .text_size(px(17.))
                            .font_semibold()
                            .text_color(fg)
                            .child(title),
                    ),
            )
            .child(fields)
    }

    fn render_appearance(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = cx.global::<Preferences>().theme;

        let choices = [
            ThemePreference::Dark,
            ThemePreference::Light,
            ThemePreference::System,
        ];

        v_flex()
            .id("appearance-scroll")
            .size_full()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(20.))
            .pb(px(20.))
            .gap(px(12.))
            .child(group_title("Theme", cx))
            .child(
                v_flex()
                    .gap(px(4.))
                    .children(choices.into_iter().enumerate().map(|(ix, pref)| {
                        radio_row(("theme-choice", ix), pref.label(), pref == current, cx).on_click(
                            cx.listener(move |_, _, window, cx| {
                                set_theme_preference(pref, window, cx);
                                cx.notify();
                            }),
                        )
                    })),
            )
    }

    fn render_terminal(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("terminal-settings-scroll")
            .size_full()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(20.))
            .pb(px(20.))
            .gap(px(12.))
            .child(group_title("Font size", cx))
            .child(self.render_terminal_font_size(cx))
            .child(group_title("Font", cx).mt(px(6.)))
            .child(self.render_terminal_fonts(cx))
            .child(group_title("Color scheme", cx).mt(px(6.)))
            .child(self.render_terminal_schemes(cx))
            .child(group_title("Typing", cx).mt(px(6.)))
            .child(self.render_terminal_typing(cx))
    }

    fn render_terminal_font_size(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let size = Preferences::terminal_font_size(cx);

        h_flex()
            .gap(px(4.))
            .items_center()
            .child(icon_button(
                "terminal-font-smaller",
                Icon::new(IconName::Minus).size(px(12.)),
                false,
                cx.listener(move |_, _, _, cx| {
                    Preferences::set_terminal_font_size(size - 1.0, cx);
                    cx.notify();
                }),
                cx,
            ))
            .child(
                div()
                    .w(px(40.))
                    .text_center()
                    .text_size(px(13.))
                    .text_color(fg)
                    .child(format!("{size} px")),
            )
            .child(icon_button(
                "terminal-font-larger",
                Icon::new(IconName::Plus).size(px(12.)),
                false,
                cx.listener(move |_, _, _, cx| {
                    Preferences::set_terminal_font_size(size + 1.0, cx);
                    cx.notify();
                }),
                cx,
            ))
            .when(size != DEFAULT_TERMINAL_FONT_SIZE, |el| {
                el.child(
                    select_chip("terminal-font-reset", "Reset", false, cx)
                        .ml(px(4.))
                        .on_click(cx.listener(|_, _, _, cx| {
                            Preferences::set_terminal_font_size(DEFAULT_TERMINAL_FONT_SIZE, cx);
                            cx.notify();
                        })),
                )
            })
            .child(
                div()
                    .ml(px(6.))
                    .text_size(px(11.5))
                    .text_color(muted_fg)
                    .child("Shared by every terminal. The zoom shortcuts change it too."),
            )
    }

    fn render_terminal_fonts(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = Preferences::terminal_font(cx);

        h_flex().flex_wrap().gap(px(4.)).children(
            self.terminal_fonts
                .clone()
                .into_iter()
                .enumerate()
                .map(|(ix, font)| {
                    select_chip(("terminal-font", ix), font.family(), font == current, cx)
                        .font_family(font.family())
                        .on_click(cx.listener(move |_, _, _, cx| {
                            Preferences::update(cx, |prefs| prefs.terminal_font = font);
                            cx.notify();
                        }))
                }),
        )
    }

    fn render_terminal_schemes(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = Preferences::terminal_scheme(cx);
        let border = cx.theme().title_bar_border;

        v_flex()
            .gap(px(4.))
            .children(
                TerminalScheme::ALL
                    .into_iter()
                    .enumerate()
                    .map(|(ix, scheme)| {
                        let (swatch_bg, dots) = TerminalColors::swatch(scheme);
                        h_flex()
                            .gap(px(8.))
                            .items_center()
                            .child(
                                radio_row(
                                    ("terminal-scheme", ix),
                                    scheme.label(),
                                    scheme == current,
                                    cx,
                                )
                                .w(px(150.))
                                .on_click(cx.listener(
                                    move |_, _, _, cx| {
                                        Preferences::update(cx, |prefs| {
                                            prefs.terminal_scheme = scheme
                                        });
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                h_flex()
                                    .h(px(14.))
                                    .px(px(5.))
                                    .gap(px(3.))
                                    .items_center()
                                    .rounded(px(3.))
                                    .border_1()
                                    .border_color(border)
                                    .bg(swatch_bg)
                                    .children(
                                        dots.into_iter()
                                            .map(|dot| div().size(px(6.)).rounded_full().bg(dot)),
                                    ),
                            )
                    }),
            )
    }

    fn render_terminal_typing(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted_fg = cx.theme().muted_foreground;
        let caption = move |text: &'static str| {
            div()
                .pl(px(22.))
                .text_size(px(11.5))
                .text_color(muted_fg)
                .child(text)
        };

        v_flex()
            .gap(px(4.))
            .child(
                Checkbox::new("terminal-suggestions")
                    .checked(Preferences::is_suggestions_enabled(cx))
                    .label("Suggest commands from history")
                    .on_click(cx.listener(|_, checked: &bool, _, cx| {
                        let is_enabled = *checked;
                        Preferences::update(cx, |prefs| prefs.is_suggestions_enabled = is_enabled);
                        cx.notify();
                    })),
            )
            .child(caption(
                "Shows matching commands as you type. Reads the host's shell history \
                 when a session connects.",
            ))
            .when(cfg!(target_os = "macos"), |el| {
                el.child(
                    Checkbox::new("terminal-option-meta")
                        .checked(Preferences::is_option_meta(cx))
                        .label("Use Option as Meta")
                        .on_click(cx.listener(|_, checked: &bool, _, cx| {
                            let is_meta = *checked;
                            Preferences::update(cx, |prefs| prefs.is_option_meta = is_meta);
                            cx.notify();
                        })),
                )
                .child(caption(
                    "Option+key sends Esc+key instead of typing the character your \
                     keyboard layout gives.",
                ))
            })
    }

    fn render_about(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;

        let version = crate::version::label();
        let updates = crate::update::platform().map(|_| self.render_update_check(cx));
        let log_file = crate::logging::log_file_path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "not available".to_string());

        // min_h, not h: a long log path wraps onto a second line.
        let kv = |label: &'static str, value: String| {
            h_flex()
                .min_h(px(22.))
                .items_center()
                .gap(px(12.))
                .child(
                    div()
                        .w(px(80.))
                        .flex_none()
                        .text_size(px(12.))
                        .text_color(muted_fg)
                        .child(label),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(13.))
                        .text_color(fg)
                        .child(value),
                )
        };

        v_flex()
            .px(px(20.))
            .pb(px(20.))
            .gap(px(12.))
            .child(group_title("About flint", cx))
            .child(
                v_flex()
                    .child(kv("Version", version))
                    .child(kv("License", "GPL-3.0-only".to_string()))
                    .child(kv("Log file", log_file)),
            )
            .children(updates)
            .child(
                h_flex()
                    .gap(px(6.))
                    .flex_wrap()
                    .child(
                        Button::new("about-repository")
                            .small()
                            .child("Repository")
                            .on_click(|_, _, cx| cx.open_url(crate::logging::REPOSITORY_URL)),
                    )
                    .child(
                        Button::new("about-report-issue")
                            .small()
                            .child("Report an issue")
                            .on_click(|_, _, cx| cx.open_url(crate::logging::ISSUES_URL)),
                    )
                    .child(
                        Button::new("about-copy-diagnostics")
                            .small()
                            .child("Copy diagnostics")
                            .on_click(|_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    crate::logging::diagnostics_text(),
                                ));
                            }),
                    ),
            )
            .child(div().text_size(px(12.)).text_color(muted_fg).child(
                "Copy diagnostics copies the version and the system only. \
                         The log file holds host names, user names and errors: \
                         read it before you attach it to an issue.",
            ))
            .child(div().text_size(px(12.)).text_color(muted_fg).child(
                "flint is free software under the GNU GPL version 3 and comes \
                         with no warranty. The source is at the Repository link.",
            ))
    }

    /// The "Check for updates on start" preference, with a button to check
    /// right now and how the last check ended.
    fn render_update_check(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted_fg = cx.theme().muted_foreground;
        let is_automatic = Preferences::update_checks(cx) == Some(UpdateChecks::OnStart);
        let service = UpdateService::global(cx);
        let (is_checking, result) = {
            let service = service.read(cx);
            (service.is_checking, service.result_line(is_automatic))
        };

        v_flex()
            .gap(px(4.))
            .child(
                Checkbox::new("about-check-for-updates")
                    .checked(is_automatic)
                    .label("Check for updates on start")
                    .on_click(|checked: &bool, _, cx| {
                        let answer = if *checked {
                            UpdateChecks::OnStart
                        } else {
                            UpdateChecks::Manual
                        };
                        UpdateService::global(cx)
                            .update(cx, |service, cx| service.set_update_checks(answer, cx));
                    }),
            )
            .child(
                div()
                    .pl(px(22.))
                    .text_size(px(11.5))
                    .text_color(muted_fg)
                    .child(
                    "When on, flint asks the flint server for the newest version when it starts \
                     and every six hours after. When off, it asks only when you press Check now. \
                     The request says macOS or Windows and nothing else.",
                ),
            )
            .child(
                h_flex()
                    .pl(px(22.))
                    .gap(px(8.))
                    .items_center()
                    .child(
                        Button::new("about-check-now")
                            .small()
                            .child("Check now")
                            .loading(is_checking)
                            .disabled(is_checking)
                            .on_click(|_, _, cx| {
                                UpdateService::global(cx)
                                    .update(cx, |service, cx| service.check(cx));
                            }),
                    )
                    // Lets a long error wrap instead of running past the card.
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(12.))
                            .text_color(muted_fg)
                            .child(result),
                    ),
            )
    }

    fn render_keys_list(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut all_keys: Vec<SshKeySummary> = self
            .vault_state
            .read(cx)
            .manager()
            .and_then(|vm| vm.list_keys().ok())
            .unwrap_or_default();
        all_keys.sort_by_key(|k| std::cmp::Reverse(k.created_at));

        let query = self
            .key_search
            .read(cx)
            .value()
            .to_string()
            .trim()
            .to_lowercase();
        let filtered: Vec<SshKeySummary> = if query.is_empty() {
            all_keys.clone()
        } else {
            all_keys
                .iter()
                .filter(|k| k.name.to_lowercase().contains(&query))
                .cloned()
                .collect()
        };

        // Drop the selection if the key no longer exists.
        if let Some(id) = self.selected_key_id {
            if !all_keys.iter().any(|k| k.id == id) {
                self.selected_key_id = None;
                self.pubkey_cache = None;
            }
        }

        let content: gpui::AnyElement = match self.selected_key_id {
            Some(id) => self.render_keys_detail_pane(id, cx).into_any_element(),
            None => self
                .render_keys_list_pane(&all_keys, &filtered, cx)
                .into_any_element(),
        };

        v_flex()
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .child(content)
    }

    fn render_keys_list_pane(
        &self,
        all_keys: &[SshKeySummary],
        filtered: &[SshKeySummary],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;

        let head = page_head("SSH keys", Some(all_keys.len()))
            .child(div().flex_1())
            .child(filter_field(&self.key_search))
            .child(
                Button::new("add-key-btn")
                    .primary()
                    .small()
                    .icon(IconName::Plus)
                    .child("Add key")
                    .on_click(cx.listener(|this, _, window, cx| this.enter_add_key(window, cx))),
            );

        let rows: gpui::AnyElement = if filtered.is_empty() {
            let msg = if all_keys.is_empty() {
                "No keys yet."
            } else {
                "No keys match the filter."
            };
            empty_note(msg).into_any_element()
        } else {
            let count = filtered.len();
            list_card()
                .children(filtered.iter().enumerate().map(|(ix, key)| {
                    let id = key.id;
                    let in_use_count = self
                        .vault_state
                        .read(cx)
                        .manager()
                        .map(|vm| vm.servers_using_key(id).len())
                        .unwrap_or(0);
                    let meta_line =
                        format!("{} · added {}", key.key_type, fmt_relative(key.created_at));
                    let usage = div()
                        .flex_none()
                        .text_size(px(12.))
                        .text_color(if in_use_count > 0 {
                            muted_fg
                        } else {
                            crate::theme::text_placeholder()
                        })
                        .child(match in_use_count {
                            0 => "unused".to_string(),
                            1 => "1 host".to_string(),
                            n => format!("{n} hosts"),
                        });

                    card_row(("key-row", ix), ix == 0, ix + 1 == count)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.selected_key_id = Some(id);
                            this.pubkey_cache = None;
                            cx.notify();
                        }))
                        .child(row_tile(
                            Icon::empty().path("icons/key-round.svg"),
                            crate::theme::yellow(),
                        ))
                        .child(
                            div()
                                .w(px(200.))
                                .flex_shrink(1.)
                                .min_w(px(80.))
                                .text_size(px(13.))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(fg)
                                .truncate()
                                .child(SharedString::from(key.name.clone())),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(12.))
                                .text_color(crate::theme::text_dim())
                                .truncate()
                                .child(meta_line),
                        )
                        .child(usage)
                }))
                .into_any_element()
        };

        v_flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .h_full()
            .child(head)
            .child(
                div()
                    .id("keys-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .child(rows),
            )
    }

    fn render_keys_detail_pane(&mut self, id: Uuid, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted_fg = theme.muted_foreground;

        let key_clone: Option<SshKey> = self
            .vault_state
            .read(cx)
            .manager()
            .and_then(|vm| vm.get_key(id).ok())
            .cloned();

        let body: gpui::AnyElement = match key_clone {
            Some(key) => self.render_key_detail_body(key, cx).into_any_element(),
            None => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(muted_fg)
                        .child("This key is no longer in the vault."),
                )
                .into_any_element(),
        };

        v_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .child(body)
    }

    fn render_key_detail_body(&mut self, key: SshKey, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let border = theme.title_bar_border;
        let bg_deep = crate::theme::surface_sunken();
        let accent = theme.primary;
        let accent_soft = crate::theme::accent_soft();
        let accent_border = accent.opacity(0.302);
        let text_editor = crate::theme::text_editor();
        let terminal_font = Preferences::terminal_font(cx).family();
        let in_use = self
            .vault_state
            .read(cx)
            .manager()
            .map(|vm| vm.servers_using_key(key.id))
            .unwrap_or_default();

        let key_id = key.id;
        let added_relative = fmt_relative(key.created_at);

        let det_head = h_flex()
            .flex_none()
            .h(px(52.))
            .items_center()
            .gap(px(10.))
            .px(px(20.))
            .border_b_1()
            .border_color(border)
            .child(back_button(
                "key-back",
                cx.listener(|this, _, _, cx| {
                    this.selected_key_id = None;
                    this.pubkey_cache = None;
                    cx.notify();
                }),
                cx,
            ))
            .child(type_pill(&key.key_type, accent, accent_soft, accent_border))
            .child(
                div()
                    .text_size(px(13.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(fg)
                    .truncate()
                    .child(SharedString::from(key.name.clone())),
            )
            .child(div().flex_1())
            .child(icon_button(
                "key-rename",
                Icon::empty().path("icons/pen-line.svg").size(px(13.)),
                false,
                cx.listener(move |this, _, window, cx| {
                    let name = this
                        .vault_state
                        .read(cx)
                        .manager()
                        .and_then(|vm| vm.get_key(key_id).ok())
                        .map(|k| k.name.clone())
                        .unwrap_or_default();
                    this.open_rename_dialog(key_id, name, window, cx);
                }),
                cx,
            ))
            .child(icon_button(
                "key-delete",
                Icon::empty().path("icons/trash.svg").size(px(13.)),
                true,
                cx.listener(move |this, _, window, cx| {
                    this.confirm_delete(key_id, window, cx);
                }),
                cx,
            ));

        let fingerprint = v_flex()
            .gap(px(2.))
            .child(
                div()
                    .text_size(px(12.5))
                    .text_color(text_editor)
                    .font_family(terminal_font)
                    .child(SharedString::from(key.fingerprint.clone())),
            )
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(muted_fg)
                    .child(format!("added {added_relative}")),
            );

        let in_use_section_h = section_header("In use by", Some(in_use.len()));
        let in_use_body: gpui::AnyElement = if in_use.is_empty() {
            div()
                .px(px(2.))
                .py(px(4.))
                .text_size(px(13.))
                .text_color(muted_fg)
                .child("Not bound to any host yet.")
                .into_any_element()
        } else {
            let pills = in_use.iter().enumerate().map(|(ix, user)| {
                let label = SharedString::from(user.label.clone());
                let sub = SharedString::from(format!("{}@{}", user.username, user.host));
                host_pill(ix, label, sub, cx)
            });
            h_flex()
                .flex_wrap()
                .gap(px(6.))
                .children(pills)
                .into_any_element()
        };

        let pubkey_result = self.derive_pubkey_line(&key);
        let pub_line: SharedString = match &pubkey_result {
            Ok(line) => SharedString::from(line.clone()),
            Err(msg) => SharedString::from(format!("(could not derive: {msg})")),
        };
        let pub_line_for_copy = match &pubkey_result {
            Ok(line) => line.clone(),
            Err(_) => String::new(),
        };

        let pubkey_section_h = h_flex()
            .items_center()
            .child(section_header("Public key", None).flex_1())
            .child(
                Button::new("pub-copy")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Copy)
                    .child("Copy")
                    .on_click(cx.listener(move |_, _, _, cx| {
                        if !pub_line_for_copy.is_empty() {
                            cx.write_to_clipboard(ClipboardItem::new_string(
                                pub_line_for_copy.clone(),
                            ));
                        }
                    })),
            );
        let pubkey_block = div()
            .id("pubkey-body")
            .p(px(12.))
            .rounded(px(8.))
            .bg(bg_deep)
            .border_1()
            .border_color(border)
            .font_family(terminal_font)
            .text_size(px(12.5))
            .text_color(text_editor)
            .child(pub_line);

        v_flex()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .child(det_head)
            .child(
                v_flex()
                    .id("key-detail-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .gap(px(12.))
                    .px(px(20.))
                    .py(px(16.))
                    .child(fingerprint)
                    .child(in_use_section_h)
                    .child(in_use_body)
                    .child(pubkey_section_h)
                    .child(pubkey_block)
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(muted_fg)
                            .child("The private key stays encrypted in the vault."),
                    ),
            )
    }

    fn derive_pubkey_line(&mut self, key: &SshKey) -> Result<String, String> {
        if let Some((cached_id, cached)) = &self.pubkey_cache {
            if *cached_id == key.id {
                return cached.clone();
            }
        }
        let result: Result<String, String> = (|| {
            use russh::keys;
            use russh::keys::PublicKeyBase64;
            let pass = key.passphrase.as_deref().filter(|p| !p.is_empty());
            let kp = keys::decode_secret_key(&key.pem, pass)
                .map_err(|e| format!("decode failed: {e}"))?;
            let pk = kp
                .clone_public_key()
                .map_err(|e| format!("derive public key failed: {e}"))?;
            let comment = format!("{}@flint", key.name);
            Ok(format!(
                "{} {} {}",
                pk.name(),
                pk.public_key_base64(),
                comment
            ))
        })();
        self.pubkey_cache = Some((key.id, result.clone()));
        result
    }

    fn render_add_key(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;

        let file_label = self
            .add_file_path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "No file selected".to_string());

        let is_file = self.add_pem_source == PemSource::File;

        let pem_section: gpui::AnyElement = if is_file {
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    Button::new("choose-file")
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
                                    let current_name = this.add_name.read(cx).value().to_string();
                                    this.add_file_path = Some(path);
                                    this.refresh_add_encrypted(cx);
                                    if current_name.trim().is_empty() && !stem.is_empty() {
                                        let name = SharedString::from(stem);
                                        this.add_name.update(cx, |s, cx| {
                                            s.set_value(name, window, cx);
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
            Textarea::new(&self.add_paste)
                .h(px(120.))
                .into_any_element()
        };

        let fields = v_flex()
            .max_w(px(420.))
            .gap(px(8.))
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(field_label("Name", cx))
                    .child(Input::new(&self.add_name).small()),
            )
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(field_label("Key source", cx))
                    .child(
                        h_flex()
                            .gap(px(12.))
                            .child(
                                radio_row("pem-src-file", "From file", is_file, cx).on_click(
                                    cx.listener(|this, _, window, cx| {
                                        if this.add_pem_source != PemSource::File {
                                            this.add_pem_source = PemSource::File;
                                            this.add_paste
                                                .update(cx, |s, cx| s.set_value("", window, cx));
                                            this.refresh_add_encrypted(cx);
                                            cx.notify();
                                        }
                                    }),
                                ),
                            )
                            .child(radio_row("pem-src-paste", "Paste", !is_file, cx).on_click(
                                cx.listener(|this, _, _, cx| {
                                    if this.add_pem_source != PemSource::Paste {
                                        this.add_pem_source = PemSource::Paste;
                                        this.add_file_path = None;
                                        this.refresh_add_encrypted(cx);
                                        cx.notify();
                                    }
                                }),
                            )),
                    ),
            )
            .child(pem_section)
            // Most keys have no passphrase, so the field stays hidden until
            // the chosen key turns out to be encrypted.
            .when(self.add_encrypted, |el| {
                el.child(
                    v_flex()
                        .gap(px(6.))
                        .child(field_label("Passphrase (key is encrypted)", cx))
                        .child(Input::new(&self.add_passphrase).small().mask_toggle()),
                )
            })
            .children(
                self.add_error
                    .as_ref()
                    .map(|msg| div().text_xs().text_color(theme.danger).child(msg.clone())),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .mt_2()
                    .child(
                        Button::new("add-key-cancel")
                            .ghost()
                            .child("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.mode = SettingsMode::KeysList;
                                this.add_error = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("add-key-save")
                            .primary()
                            .child("Save key")
                            .on_click(cx.listener(|this, _, _, cx| this.save_key(cx))),
                    ),
            );

        v_flex()
            .id("add-key-form")
            .size_full()
            .px(px(20.))
            .pt(px(14.))
            .pb(px(20.))
            .gap(px(12.))
            .overflow_y_scroll()
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(back_button(
                        "add-key-back",
                        cx.listener(|this, _, _, cx| {
                            this.mode = SettingsMode::KeysList;
                            this.add_error = None;
                            cx.notify();
                        }),
                        cx,
                    ))
                    .child(
                        div()
                            .text_size(px(17.))
                            .font_semibold()
                            .text_color(fg)
                            .child("Add key"),
                    ),
            )
            .child(fields)
    }

    fn render_snippets_list(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut snippets: Vec<Snippet> = self
            .vault_state
            .read(cx)
            .manager()
            .and_then(|vm| vm.list_snippets().ok())
            .unwrap_or_default();
        snippets.sort_by_key(|s| std::cmp::Reverse(s.updated_at));

        let total = snippets.len();

        let query = self
            .snippet_search
            .read(cx)
            .value()
            .to_string()
            .trim()
            .to_lowercase();

        let filtered: Vec<Snippet> = snippets
            .iter()
            .filter(|s| {
                if query.is_empty() {
                    true
                } else {
                    s.name.to_lowercase().contains(&query)
                        || s.description
                            .as_deref()
                            .map(|d| d.to_lowercase().contains(&query))
                            .unwrap_or(false)
                }
            })
            .cloned()
            .collect();

        // Drop selection if the snippet no longer exists.
        if let Some(id) = self.selected_snippet_id {
            if !snippets.iter().any(|s| s.id == id) {
                self.selected_snippet_id = None;
                self.snippet_view_mode = SnippetEditMode::View;
            }
        }

        let content: gpui::AnyElement = if self.creating_snippet {
            self.render_snippet_create_pane(cx).into_any_element()
        } else if let Some(id) = self.selected_snippet_id {
            self.render_snippets_detail_pane(id, cx).into_any_element()
        } else {
            self.render_snippets_list_pane(&filtered, total, cx)
                .into_any_element()
        };

        v_flex()
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .child(content)
    }

    fn render_snippets_list_pane(
        &self,
        snippets: &[Snippet],
        total: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let head = page_head("Snippets", Some(total))
            .child(div().flex_1())
            .child(filter_field(&self.snippet_search))
            .child(
                Button::new("add-snippet-btn")
                    .primary()
                    .small()
                    .icon(IconName::Plus)
                    .child("New snippet")
                    .on_click(
                        cx.listener(|this, _, window, cx| this.enter_add_snippet(window, cx)),
                    ),
            );

        let rows: gpui::AnyElement = if snippets.is_empty() {
            empty_note(if total == 0 {
                "No snippets yet."
            } else {
                "No snippets match the filter."
            })
            .into_any_element()
        } else {
            let count = snippets.len();
            list_card()
                .children(
                    snippets
                        .iter()
                        .enumerate()
                        .map(|(ix, snippet)| self.render_snip_row(snippet, ix, count, cx)),
                )
                .into_any_element()
        };

        v_flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .h_full()
            .child(head)
            .child(
                div()
                    .id("snippets-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .child(rows),
            )
    }

    fn render_snip_row(
        &self,
        snippet: &Snippet,
        ix: usize,
        count: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let row_id: SharedString = format!("snip-row-{}", snippet.id).into();
        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let id = snippet.id;

        let SnippetBody::Script { content } = &snippet.body;
        let preview: SharedString = content.lines().next().unwrap_or("").to_string().into();
        let updated_label = fmt_relative(snippet.updated_at.max(0) as u64);

        card_row(row_id, ix == 0, ix + 1 == count)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.selected_snippet_id = Some(id);
                this.snippet_view_mode = SnippetEditMode::View;
                cx.notify();
            }))
            .child(row_tile(
                Icon::new(IconName::SquareTerminal),
                crate::theme::cyan(),
            ))
            .child(
                div()
                    .w(px(180.))
                    .flex_shrink(1.)
                    .min_w(px(80.))
                    .text_size(px(13.))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(fg)
                    .truncate()
                    .child(SharedString::from(snippet.name.clone())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .font_family(crate::theme::MONO_FONT)
                    .text_size(px(12.))
                    .text_color(crate::theme::text_dim())
                    .truncate()
                    .child(preview),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(12.))
                    .text_color(muted_fg)
                    .child(updated_label),
            )
    }

    fn render_snippets_detail_pane(
        &mut self,
        id: Uuid,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let muted_fg = cx.theme().muted_foreground;

        let snippet: Option<Snippet> = self
            .vault_state
            .read(cx)
            .manager()
            .and_then(|vm| vm.get_snippet(id).ok())
            .cloned();

        match snippet {
            None => v_flex()
                .flex_1()
                .h_full()
                .min_w_0()
                .min_h_0()
                .overflow_hidden()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(muted_fg)
                        .child("This snippet is no longer in the vault."),
                )
                .into_any_element(),
            Some(s) => self.render_script_detail(s, false, cx).into_any_element(),
        }
    }

    /// Detail pane for a snippet that is not saved yet. It is drawn from a
    /// draft built out of the `edit_*` inputs.
    fn render_snippet_create_pane(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let draft = self.draft_snippet(cx);
        self.render_script_detail(draft, true, cx)
    }

    /// A throwaway `Snippet` for the create form. Only the header reads it:
    /// the editable fields come straight from the `edit_*` inputs.
    fn draft_snippet(&self, cx: &App) -> Snippet {
        let name = self.edit_name.read(cx).value().to_string();
        let description = {
            let d = self
                .edit_description
                .read(cx)
                .value()
                .to_string()
                .trim()
                .to_string();
            if d.is_empty() {
                None
            } else {
                Some(d)
            }
        };
        Snippet {
            id: Uuid::nil(),
            name,
            description,
            created_at: 0,
            updated_at: 0,
            body: SnippetBody::Script {
                content: self.edit_content.read(cx).value().to_string(),
            },
            usages: Vec::new(),
        }
    }

    fn render_script_detail(
        &mut self,
        snippet: Snippet,
        creating: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let SnippetBody::Script { content } = &snippet.body;
        let content = content.clone();
        let in_edit = creating || self.snippet_view_mode == SnippetEditMode::Edit;

        let det_head = self.render_detail_head(&snippet, creating, cx);

        let description_row: Option<gpui::AnyElement> = if in_edit {
            Some(
                v_flex()
                    .gap(px(6.))
                    .child(field_label("Description", cx))
                    .child(Input::new(&self.edit_description))
                    .into_any_element(),
            )
        } else {
            snippet
                .description
                .clone()
                .filter(|desc| !desc.is_empty())
                .map(|desc| {
                    div()
                        .text_size(px(13.))
                        .text_color(cx.theme().foreground)
                        .child(desc)
                        .into_any_element()
                })
        };

        let content_block: gpui::AnyElement = if in_edit {
            v_flex()
                .gap(px(6.))
                .child(field_label("Content", cx))
                .child(Textarea::new(&self.edit_content).h(px(220.)))
                .into_any_element()
        } else {
            v_flex()
                .gap(px(6.))
                .child(field_label("Content", cx))
                .child(code_block(&content, cx))
                .into_any_element()
        };

        let det_body = v_flex()
            .id("script-detail-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .gap(px(12.))
            .px(px(20.))
            .py(px(16.))
            .children(description_row)
            .child(content_block);

        let foot = self.render_detail_foot(snippet.id, creating, cx);

        v_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .child(det_head)
            .child(det_body)
            .child(foot)
    }

    fn render_detail_head(
        &mut self,
        snippet: &Snippet,
        creating: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let border = theme.title_bar_border;

        let sid = snippet.id;
        let in_view = !creating && self.snippet_view_mode == SnippetEditMode::View;
        let in_edit = !in_view;

        let edit_button = in_view.then(|| {
            Button::new("snip-edit")
                .ghost()
                .xsmall()
                .icon(Icon::empty().path("icons/pen-line.svg"))
                .child("Edit")
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.enter_edit_snippet(sid, window, cx);
                    this.snippet_view_mode = SnippetEditMode::Edit;
                    cx.notify();
                }))
        });
        let updated = (!creating).then(|| {
            div()
                .flex_none()
                .text_size(px(11.5))
                .text_color(muted_fg)
                .child(format!(
                    "updated {}",
                    fmt_relative(snippet.updated_at.max(0) as u64)
                ))
        });

        let name_el: gpui::AnyElement = if in_edit {
            div()
                .flex_1()
                .min_w(px(0.))
                .child(Input::new(&self.edit_name).small())
                .into_any_element()
        } else {
            h_flex()
                .flex_1()
                .min_w(px(0.))
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(fg)
                        .truncate()
                        .child(SharedString::from(snippet.name.clone())),
                )
                .into_any_element()
        };

        h_flex()
            .flex_none()
            .items_center()
            .h(px(52.))
            .gap(px(10.))
            .px(px(20.))
            .border_b_1()
            .border_color(border)
            .child(back_button(
                "snip-back",
                cx.listener(move |this, _, _, cx| {
                    if creating {
                        this.cancel_create_snippet();
                    } else {
                        this.selected_snippet_id = None;
                        this.snippet_view_mode = SnippetEditMode::View;
                        this.editing_snippet_id = None;
                        this.edit_error = None;
                    }
                    cx.notify();
                }),
                cx,
            ))
            .child(name_el)
            .children(updated)
            .children(edit_button)
    }

    fn render_detail_foot(
        &mut self,
        snippet_id: Uuid,
        creating: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let border = theme.title_bar_border;
        let danger = theme.danger;

        let in_view = !creating && self.snippet_view_mode == SnippetEditMode::View;

        let error_el: Option<gpui::AnyElement> = (!in_view)
            .then(|| self.edit_error.clone())
            .flatten()
            .map(|msg| {
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_size(px(12.))
                    .text_color(danger)
                    .truncate()
                    .child(msg)
                    .into_any_element()
            });
        let spacer: Option<gpui::AnyElement> = error_el
            .is_none()
            .then(|| div().flex_1().into_any_element());

        let bar = h_flex()
            .flex_none()
            .h(px(52.))
            .px(px(20.))
            .items_center()
            .gap(px(8.))
            .border_t_1()
            .border_color(border);

        if creating {
            bar.child(
                Button::new("foot-cancel-create")
                    .ghost()
                    .small()
                    .child("Cancel")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.cancel_create_snippet();
                        cx.notify();
                    })),
            )
            .children(error_el)
            .children(spacer)
            .child(
                Button::new("foot-create")
                    .primary()
                    .small()
                    .child("Create snippet")
                    .on_click(cx.listener(|this, _, _, cx| this.save_snippet(cx))),
            )
        } else if in_view {
            bar.child(
                Button::new("foot-duplicate")
                    .ghost()
                    .small()
                    .child("Duplicate")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.duplicate_snippet(snippet_id, cx);
                    })),
            )
            .child(div().flex_1())
            .child(
                Button::new("foot-run")
                    .primary()
                    .small()
                    .child("Run")
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(SettingsViewEvent::RunSnippet(snippet_id));
                    })),
            )
        } else {
            bar.child(
                Button::new("foot-delete")
                    .ghost()
                    .danger()
                    .small()
                    .child("Delete")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.confirm_delete_snippet(snippet_id, window, cx);
                    })),
            )
            .children(error_el)
            .children(spacer)
            .child(
                Button::new("foot-discard")
                    .ghost()
                    .small()
                    .child("Discard")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.snippet_view_mode = SnippetEditMode::View;
                        this.editing_snippet_id = None;
                        this.edit_error = None;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("foot-save")
                    .primary()
                    .small()
                    .child("Save changes")
                    .on_click(cx.listener(|this, _, _, cx| this.save_snippet(cx))),
            )
        }
    }

    fn enter_add_snippet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing_snippet_id = None;
        self.edit_name
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.edit_description
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.edit_content
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.edit_error = None;
        self.selected_snippet_id = None;
        self.creating_snippet = true;
        self.snippet_view_mode = SnippetEditMode::Edit;
        self.mode = SettingsMode::SnippetsList;
        cx.notify();
    }

    /// Abandon the in-progress new snippet and return to the snippets list.
    fn cancel_create_snippet(&mut self) {
        self.creating_snippet = false;
        self.editing_snippet_id = None;
        self.edit_error = None;
        self.snippet_view_mode = SnippetEditMode::View;
    }

    fn enter_edit_snippet(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snippet) = self
            .vault_state
            .read(cx)
            .manager()
            .and_then(|vm| vm.get_snippet(id).ok().cloned())
        else {
            tracing::warn!(?id, "snippet not found when entering edit");
            return;
        };

        self.editing_snippet_id = Some(id);
        let name = SharedString::from(snippet.name.clone());
        let desc = SharedString::from(snippet.description.clone().unwrap_or_default());
        self.edit_name
            .update(cx, |s, cx| s.set_value(name, window, cx));
        self.edit_description
            .update(cx, |s, cx| s.set_value(desc, window, cx));

        let SnippetBody::Script { content } = snippet.body;
        let content_ss = SharedString::from(content);
        self.edit_content
            .update(cx, |s, cx| s.set_value(content_ss, window, cx));

        self.edit_error = None;
        self.mode = SettingsMode::SnippetsList;
        cx.notify();
    }

    fn save_snippet(&mut self, cx: &mut Context<Self>) {
        let name = self.edit_name.read(cx).value().to_string();
        let name_trimmed = name.trim().to_string();
        if name_trimmed.is_empty() {
            self.edit_error = Some("Name is required.".into());
            cx.notify();
            return;
        }

        let description_raw = self.edit_description.read(cx).value().to_string();
        let description = {
            let t = description_raw.trim().to_string();
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        };

        let content = self.edit_content.read(cx).value().to_string();
        if content.trim().is_empty() {
            self.edit_error = Some("Content is required.".into());
            cx.notify();
            return;
        }
        let body = SnippetBody::Script { content };

        let editing_id = self.editing_snippet_id;
        // For a create, carry back the new id (Some); for an update, None.
        let result = self.vault_state.update(cx, |vs, _| {
            vs.manager_mut().map(|vm| match editing_id {
                Some(id) => vm
                    .update_snippet(id, name_trimmed, description, body)
                    .map(|_| None),
                None => vm.add_snippet(name_trimmed, description, body).map(Some),
            })
        });

        match result {
            Some(Ok(new_id)) => {
                self.editing_snippet_id = None;
                self.edit_error = None;
                self.snippet_view_mode = SnippetEditMode::View;
                self.mode = SettingsMode::SnippetsList;
                if self.creating_snippet {
                    // Show the snippet that was just created.
                    self.creating_snippet = false;
                    self.selected_snippet_id = new_id;
                }
                cx.notify();
            }
            Some(Err(e)) => {
                self.edit_error = Some(format!("Failed to save: {}", e));
                cx.notify();
            }
            None => {
                self.edit_error = Some("Vault is locked.".into());
                cx.notify();
            }
        }
    }

    fn duplicate_snippet(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let result = self.vault_state.update(cx, |vs, _| {
            let vm = vs.manager_mut()?;
            let original = vm.get_snippet(id).ok()?.clone();
            let new_name = format!("{} (copy)", original.name);
            Some(vm.add_snippet(new_name, original.description, original.body))
        });

        if let Some(Err(e)) = result {
            tracing::error!(?e, ?id, "failed to duplicate snippet");
        }
        cx.notify();
    }

    fn confirm_delete_snippet(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let snippet_name = self
            .vault_state
            .read(cx)
            .manager()
            .and_then(|vm| vm.get_snippet(id).ok())
            .map(|s| s.name.clone())
            .unwrap_or_default();

        let msg = format!(
            "Delete snippet \"{}\"? This cannot be undone.",
            snippet_name
        );
        let weak = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let (w2, m2) = (weak.clone(), msg.clone());
            dialog_chrome(dialog).w(px(380.)).child(
                v_flex()
                    .p(px(12.))
                    .gap(px(8.))
                    .child(DialogHeader::new().child(DialogTitle::new().child("Delete snippet")))
                    .child(div().text_sm().child(m2))
                    .child(
                        DialogFooter::new().child(
                            DialogAction::new().child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        Button::new("snip-del-cancel")
                                            .ghost()
                                            .child("Cancel")
                                            .on_click(|_, window, cx| window.close_dialog(cx)),
                                    )
                                    .child(
                                        Button::new("snip-del-confirm")
                                            .danger()
                                            .child("Delete")
                                            .on_click(move |_, window, cx| {
                                                w2.update(cx, |this, cx| {
                                                    let result =
                                                        this.vault_state.update(cx, |vs, _| {
                                                            vs.manager_mut()
                                                                .map(|vm| vm.delete_snippet(id))
                                                        });
                                                    if let Some(Err(e)) = result {
                                                        tracing::error!(
                                                            ?e,
                                                            "failed to delete snippet"
                                                        );
                                                    }
                                                    cx.notify();
                                                })
                                                .ok();
                                                window.close_dialog(cx);
                                            }),
                                    ),
                            ),
                        ),
                    ),
            )
        });
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        // gpui does not clip children to rounded corners, so the rail rounds
        // its own outer corners to sit inside the card's.
        let inner_radius = px(CARD_RADIUS - 1.);

        let rail_items = SettingsSection::all()
            .iter()
            .enumerate()
            .map(|(ix, &section)| {
                let is_active = self.active_section == section;
                // Marked like "All hosts" in the sidebar: a raised fill and
                // an accent icon.
                let icon_color = if is_active {
                    crate::theme::accent()
                } else {
                    muted_fg
                };
                h_flex()
                    .id(("settings-section", ix))
                    .w_full()
                    .h(px(30.))
                    .items_center()
                    .gap(px(10.))
                    .px(px(10.))
                    .rounded(px(6.))
                    .text_size(px(13.))
                    .text_color(if is_active { fg } else { muted_fg })
                    .cursor_pointer()
                    .when(is_active, |el| el.bg(crate::theme::element()))
                    .when(!is_active, |el| {
                        el.hover(|s| s.bg(crate::theme::field()).text_color(fg))
                            .active(|s| s.bg(crate::theme::element_active()))
                    })
                    .child(section.icon().size(px(15.)).text_color(icon_color))
                    .child(section.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.active_section = section;
                        this.mode = match section {
                            SettingsSection::Vault => SettingsMode::Vault,
                            SettingsSection::Sync => SettingsMode::Sync,
                            SettingsSection::Teams => SettingsMode::Teams,
                            SettingsSection::Appearance => SettingsMode::Appearance,
                            SettingsSection::Terminal => SettingsMode::Terminal,
                            SettingsSection::Keys => SettingsMode::KeysList,
                            SettingsSection::Snippets => SettingsMode::SnippetsList,
                            SettingsSection::Network => SettingsMode::ProxyList,
                            SettingsSection::About => SettingsMode::About,
                        };
                        if matches!(section, SettingsSection::Sync | SettingsSection::Teams) {
                            SyncService::global(cx)
                                .update(cx, |service, cx| service.refresh_if_stale(cx));
                        }
                        this.add_error = None;
                        this.edit_error = None;
                        cx.notify();
                    }))
            });

        let close_button = div()
            .id("settings-close")
            .size(px(26.))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .cursor_pointer()
            .text_color(muted_fg)
            .hover(move |s| s.bg(crate::theme::field()).text_color(fg))
            .active(|s| s.bg(crate::theme::element_active()))
            .tooltip(|window, cx| {
                gpui_component::tooltip::Tooltip::new("Close settings (Esc)").build(window, cx)
            })
            .on_click(cx.listener(|_, _, _, cx| {
                cx.emit(SettingsViewEvent::Close);
            }))
            .child(gpui_component::Icon::new(IconName::Close).size(px(14.)));

        let rail = v_flex()
            .w(px(200.))
            .h_full()
            .flex_none()
            .rounded_l(inner_radius)
            .bg(crate::theme::modal_rail())
            .border_r_1()
            .border_color(crate::theme::border())
            .px(px(8.))
            .pb(px(8.))
            .gap(px(2.))
            .child(
                h_flex()
                    .h(px(52.))
                    .pl(px(10.))
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(fg)
                            .child("Settings"),
                    )
                    .child(close_button),
            )
            .children(rail_items);

        // List pages carry their own title row with a filter and a button;
        // the others get the section's name here.
        let page_title = match self.mode {
            SettingsMode::Vault
            | SettingsMode::Sync
            | SettingsMode::Teams
            | SettingsMode::Appearance
            | SettingsMode::Terminal
            | SettingsMode::About => Some(self.active_section.label()),
            _ => None,
        };

        let content_pane: gpui::AnyElement = match self.mode {
            SettingsMode::Vault => self.vault_pane.clone().into_any_element(),
            SettingsMode::Sync => self.sync_pane.clone().into_any_element(),
            SettingsMode::Teams => self.teams_pane.clone().into_any_element(),
            SettingsMode::Appearance => self.render_appearance(cx).into_any_element(),
            SettingsMode::Terminal => self.render_terminal(cx).into_any_element(),
            SettingsMode::KeysList => self.render_keys_list(cx).into_any_element(),
            SettingsMode::AddKey => self.render_add_key(cx).into_any_element(),
            SettingsMode::SnippetsList => self.render_snippets_list(cx).into_any_element(),
            SettingsMode::ProxyList => self.render_proxy(cx).into_any_element(),
            SettingsMode::ProxyEditor => self.render_proxy_editor(cx).into_any_element(),
            SettingsMode::About => self.render_about(cx).into_any_element(),
        };

        let content = v_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .when_some(page_title, |el, title| el.child(page_head(title, None)))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(content_pane),
            );

        div()
            .id("settings-overlay")
            .absolute()
            .top_0()
            .left_0()
            .w_full()
            .h_full()
            .bg(cx.theme().overlay)
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .p(px(32.))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_this, _, _, cx| {
                    cx.emit(SettingsViewEvent::Close);
                }),
            )
            .key_context("Settings")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_this, _: &CloseSettings, _, cx| {
                cx.emit(SettingsViewEvent::Close);
            }))
            .child(
                h_flex()
                    .id("settings-card")
                    .w_full()
                    .h_full()
                    .max_w(px(980.))
                    .max_h(px(680.))
                    .items_stretch()
                    .bg(crate::theme::modal_surface())
                    .border_1()
                    .border_color(crate::theme::float_border())
                    .rounded(px(CARD_RADIUS))
                    .shadow(crate::ui::modal_shadow())
                    .overflow_hidden()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(rail)
                    .child(content),
            )
    }
}

/// Applies a theme choice everywhere and remembers it. The switch has already
/// taken effect in memory, so a failed write is logged and not surfaced.
pub(crate) fn set_theme_preference(pref: ThemePreference, window: &mut Window, cx: &mut App) {
    cx.global_mut::<Preferences>().theme = pref;
    crate::theme::apply_preference(pref, Some(window), cx);
    if let Err(e) = cx.global::<Preferences>().save() {
        tracing::warn!(?e, "failed to save theme preference");
    }
}

const CARD_RADIUS: f32 = 12.;

/// The top of a settings page: its name, and the count of what it lists.
/// List pages add their filter and button to the right.
fn page_head(title: &'static str, count: Option<usize>) -> gpui::Div {
    h_flex()
        .flex_none()
        .h(px(60.))
        .px(px(20.))
        .gap(px(10.))
        .items_center()
        .child(
            div()
                .text_size(px(17.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(crate::theme::text())
                .child(title),
        )
        .when_some(count, |el, n| el.child(count_chip(n)))
}

/// The filter field of a list page: a fill, an outline and a search icon.
fn filter_field(input: &Entity<InputState>) -> impl IntoElement {
    h_flex()
        .w(px(220.))
        .flex_shrink(1.)
        .min_w(px(120.))
        .h(px(28.))
        .pl(px(10.))
        .pr(px(6.))
        .gap(px(7.))
        .items_center()
        .rounded(px(7.))
        .bg(crate::theme::field())
        .border_1()
        .border_color(crate::theme::border())
        .child(
            Icon::new(IconName::Search)
                .size(px(13.))
                .text_color(crate::theme::text_dim()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(Input::new(input).appearance(false)),
        )
}

/// The rows of a list page, as one card a step lighter than the page.
fn list_card() -> gpui::Div {
    v_flex()
        .mx(px(20.))
        .mb(px(20.))
        .rounded(px(CARD_RADIUS - 2.))
        .border_1()
        .border_color(crate::theme::border())
        .bg(crate::theme::card())
}

/// One row of a list card. The end rows round their own corners, since a
/// hover fill is not clipped to the card's, and every row after the first
/// has a line above it that starts past the tile.
fn card_row(
    id: impl Into<gpui::ElementId>,
    is_first: bool,
    is_last: bool,
) -> gpui::Stateful<gpui::Div> {
    let inner_radius = px(CARD_RADIUS - 3.);
    h_flex()
        .id(id)
        .relative()
        .w_full()
        .h(px(44.))
        .px(px(12.))
        .gap(px(12.))
        .items_center()
        .when(is_first, |row| row.rounded_t(inner_radius))
        .when(is_last, |row| row.rounded_b(inner_radius))
        .cursor_pointer()
        .hover(|s| s.bg(crate::theme::field()))
        .active(|s| s.bg(crate::theme::element_active()))
        .when(!is_first, |row| {
            row.child(
                div()
                    .absolute()
                    .top_0()
                    .left(px(52.))
                    .right_0()
                    .h(px(1.))
                    .bg(crate::theme::border()),
            )
        })
}

/// The tile at the start of a list row, tinted with `color`.
fn row_tile(icon: Icon, color: Hsla) -> impl IntoElement {
    div()
        .flex_none()
        .size(px(28.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(7.))
        .bg(color.opacity(0.16))
        .child(icon.size(px(15.)).text_color(color))
}

/// Heading of one group of controls on a settings page.
fn group_title(text: &'static str, cx: &App) -> gpui::Div {
    div()
        .text_size(px(13.))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(cx.theme().foreground)
        .child(text)
}

fn field_label(text: &'static str, cx: &App) -> impl IntoElement {
    div()
        .text_size(px(12.))
        .text_color(cx.theme().muted_foreground)
        .child(text)
}

fn radio_dot(selected: bool, cx: &App) -> impl IntoElement {
    let accent = cx.theme().primary;
    div()
        .flex_none()
        .size(px(16.))
        .rounded_full()
        .border_1()
        .border_color(if selected {
            accent
        } else {
            crate::theme::border_focused()
        })
        .bg(crate::theme::field())
        .flex()
        .items_center()
        .justify_center()
        .when(selected, |el| {
            el.child(div().size(px(8.)).rounded_full().bg(accent))
        })
}

fn format_age(timestamp: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    if now < timestamp {
        return "added just now".to_string();
    }

    let days = (now - timestamp) / 86_400;

    if days == 0 {
        "added today".to_string()
    } else if days == 1 {
        "added yesterday".to_string()
    } else if days < 7 {
        format!("added {} days ago", days)
    } else {
        let (y, m, d) = unix_to_ymd(timestamp as i64);
        let month = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ]
        .get(m.saturating_sub(1) as usize)
        .copied()
        .unwrap_or("?");
        format!("added {} {}, {}", month, d, y)
    }
}

/// Like format_age but without the leading "added " prefix.
fn fmt_relative(timestamp: u64) -> String {
    let raw = format_age(timestamp);
    raw.strip_prefix("added ")
        .map(|s| s.to_string())
        .unwrap_or(raw)
}

/// What a list page says when it has no rows to show.
fn empty_note(text: &'static str) -> impl IntoElement {
    div()
        .mx(px(20.))
        .py(px(24.))
        .text_size(px(13.))
        .text_color(crate::theme::text_dim())
        .child(text)
}

/// Plain "N" count shown next to a page title.
fn count_chip(n: usize) -> impl IntoElement {
    div()
        .text_size(px(13.))
        .text_color(crate::theme::text_dim())
        .child(n.to_string())
}

/// Uppercase key-type pill for the detail header.
fn type_pill(text: &str, fg: Hsla, bg: Hsla, border: Hsla) -> impl IntoElement {
    let t = text.to_uppercase();
    h_flex()
        .flex_none()
        .h(px(22.))
        .px(px(8.))
        .items_center()
        .gap(px(6.))
        .rounded(px(6.))
        .bg(bg)
        .border_1()
        .border_color(border)
        .text_size(px(11.5))
        .text_color(fg)
        .child(Icon::new(IconName::Asterisk).size(px(11.)))
        .child(SharedString::from(t))
}

/// Heading over a part of a page, with an optional count.
fn section_header(label: &'static str, count: Option<usize>) -> gpui::Div {
    h_flex()
        .items_center()
        .gap(px(8.))
        .mt(px(8.))
        .child(
            div()
                .text_size(px(13.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(crate::theme::text())
                .child(label),
        )
        .when_some(count, |el, n| {
            el.child(
                div()
                    .text_size(px(12.))
                    .text_color(crate::theme::text_dim())
                    .child(n.to_string()),
            )
        })
}

/// Small icon-only button.
fn icon_button<F>(
    id: impl Into<gpui::ElementId>,
    icon: Icon,
    is_danger: bool,
    on_click: F,
    cx: &App,
) -> impl IntoElement
where
    F: Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
{
    let theme = cx.theme();
    let muted_fg = theme.muted_foreground;
    let fg = if is_danger {
        theme.danger
    } else {
        theme.foreground
    };
    let hover_bg = crate::theme::field();
    div()
        .id(id)
        .w(px(26.))
        .h(px(26.))
        .flex_none()
        .rounded(px(6.))
        .flex()
        .items_center()
        .justify_center()
        .text_color(muted_fg)
        .cursor_pointer()
        .hover(move |s| s.bg(hover_bg).text_color(fg))
        .active(move |s| s.bg(crate::theme::element_active()).text_color(fg))
        .on_click(on_click)
        .child(icon)
}

/// A host chip used in "In use by".
fn host_pill(ix: usize, label: SharedString, sub: SharedString, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    h_flex()
        .id(("host-pill", ix))
        .flex_none()
        .h(px(26.))
        .px(px(9.))
        .gap(px(6.))
        .items_center()
        .rounded(px(6.))
        .bg(crate::theme::field())
        .border_1()
        .border_color(crate::theme::border())
        .text_size(px(13.))
        .text_color(theme.foreground)
        .child(div().child(label))
        .child(
            div()
                .text_size(px(12.))
                .text_color(theme.muted_foreground)
                .child(sub),
        )
}

/// Compact "Back" button used in detail headers.
pub(crate) fn back_button<F>(
    id: impl Into<gpui::ElementId>,
    on_click: F,
    cx: &App,
) -> impl IntoElement
where
    F: Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
{
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let fg = theme.foreground;
    let hover_bg = crate::theme::field();
    h_flex()
        .id(id)
        .flex_none()
        .h(px(26.))
        .px(px(8.))
        .gap(px(6.))
        .items_center()
        .rounded(px(6.))
        .text_size(px(12.5))
        .text_color(muted)
        .cursor_pointer()
        .hover(move |s| s.bg(hover_bg).text_color(fg))
        .active(move |s| s.bg(crate::theme::element_active()).text_color(fg))
        .on_click(on_click)
        .child(Icon::new(IconName::ArrowLeft).size(px(14.)))
        .child("Back")
}

fn code_block(content: &str, cx: &App) -> impl IntoElement {
    let text_editor = crate::theme::text_editor();
    let yellow = crate::theme::yellow();
    let yellow_bg = yellow.opacity(0.08);

    let lines = content.split('\n').map(|line| {
        h_flex()
            .flex_wrap()
            .items_baseline()
            .min_h(px(18.))
            .children(split_variables(line).into_iter().map(|(is_var, text)| {
                if is_var {
                    div()
                        .px(px(3.))
                        .rounded(px(3.))
                        .bg(yellow_bg)
                        .text_color(yellow)
                        .child(SharedString::from(text))
                } else {
                    div()
                        .text_color(text_editor)
                        .child(SharedString::from(text))
                }
            }))
    });

    v_flex()
        .p(px(12.))
        .rounded(px(8.))
        .bg(crate::theme::surface_sunken())
        .border_1()
        .border_color(crate::theme::border())
        .text_size(px(13.))
        .line_height(px(18.))
        .font_family(Preferences::terminal_font(cx).family())
        .children(lines)
}

/// Split `s` into (is_variable, text) segments. Variables look like `${UPPER_SNAKE}`.
fn split_variables(s: &str) -> Vec<(bool, String)> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut buf = String::new();
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'$' && bytes[i + 1] == b'{' {
            if let Some(end_off) = s[i + 2..].find('}') {
                let end = i + 2 + end_off;
                let name = &s[i + 2..end];
                if !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                {
                    if !buf.is_empty() {
                        out.push((false, std::mem::take(&mut buf)));
                    }
                    out.push((true, format!("${{{}}}", name)));
                    i = end + 1;
                    continue;
                }
            }
        }
        let ch = s[i..]
            .chars()
            .next()
            .expect("i stays inside s while the loop runs");
        buf.push(ch);
        i += ch.len_utf8();
    }
    if !buf.is_empty() {
        out.push((false, buf));
    }
    out
}

/// Pill-shaped selectable chip (used for proxy default + per-server override).
fn select_chip(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    active: bool,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let theme = cx.theme();
    let accent = theme.primary;
    h_flex()
        .id(id)
        .h(px(26.))
        .px(px(9.))
        .items_center()
        .rounded(px(6.))
        .border_1()
        .border_color(if active {
            accent
        } else {
            crate::theme::border()
        })
        .bg(if active {
            crate::theme::accent_soft()
        } else {
            crate::theme::field()
        })
        .when(!active, |el| {
            el.hover(|s| s.bg(crate::theme::element_hover()))
        })
        .text_size(px(12.5))
        .text_color(if active {
            theme.foreground
        } else {
            theme.muted_foreground
        })
        .cursor_pointer()
        .child(label.into())
}

/// A clickable radio option: dot + label.
fn radio_row(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    active: bool,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    h_flex()
        .id(id)
        .h(px(28.))
        .gap(px(10.))
        .items_center()
        .cursor_pointer()
        .child(radio_dot(active, cx))
        .child(
            div()
                .text_size(px(13.))
                .text_color(cx.theme().foreground)
                .child(label.into()),
        )
}

/// Small neutral pill showing a proxy's type (SOCKS5 / HTTP).
fn proxy_kind_pill(kind: ProxyKind, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    div()
        .flex_none()
        .h(px(20.))
        .px(px(6.))
        .flex()
        .items_center()
        .rounded(px(5.))
        .bg(crate::theme::field())
        .border_1()
        .border_color(crate::theme::border())
        .text_size(px(11.5))
        .text_color(theme.foreground)
        .child(kind.display_label())
}

/// Minimal Gregorian calendar date from Unix timestamp (ignores leap-seconds).
fn unix_to_ymd(ts: i64) -> (i64, i64, i64) {
    // Shift epoch to 1 Mar 0000 to simplify leap-year arithmetic.
    let z = ts / 86_400 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}
