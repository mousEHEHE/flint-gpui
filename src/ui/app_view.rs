use std::path::PathBuf;

use gpui::prelude::*;
use gpui::{
    actions, div, px, AnyElement, AnyWindowHandle, App, Context, Entity, EntityId, FocusHandle,
    Focusable, ModifiersChangedEvent, StyleRefinement, SystemNotification, Task, WeakEntity,
    Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    dialog::{DialogAction, DialogFooter, DialogHeader, DialogTitle},
    h_flex, v_flex, ActiveTheme as _, Icon, IconName, WindowExt as _,
};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::connection::Connection;
use crate::file_associations::{FileAssociations, SharedFileAssociations};
use crate::ssh::{AuthCredential, ForwardState, SshRoute, TransferOutcome};
use crate::ssh_target::{find_saved_host, find_saved_jump_host, SshTarget};
use crate::ui::app_picker::{AppPicker, AppPickerEvent};
use crate::ui::command_palette::{CommandPalette, CommandPaletteEvent, PaletteCommand};
use crate::ui::feedback_view::{FeedbackEvent, FeedbackView};
use crate::ui::import_view::{plural, ImportView, ImportViewEvent};
use crate::ui::keyboard_layout::bind_chord;
use crate::ui::passphrase_prompt::{PassphrasePromptEvent, PassphrasePromptView};
use crate::ui::server_form::{FormEvent, FormMode, ServerFormView};
use crate::ui::server_session::{
    connect_time_message, ForwardRuntime, ServerSession, ServerSessionEvent,
};
use crate::ui::servers_view::{ServersView, ServersViewEvent};
use crate::ui::settings::{SettingsView, SettingsViewEvent};
use crate::ui::sftp_browser::{RestorePaths, TransferKind};
use crate::ui::shortcut_hints;
use crate::ui::sidebar::{HostStatus, Sidebar, SidebarEvent, SIDEBAR_WIDTH};
use crate::ui::sync_service::SyncService;
use crate::ui::transfer_conflict::transfer_conflict_dialog;
use crate::ui::update_consent_view::{UpdateConsentEvent, UpdateConsentView};
use crate::ui::update_service::{Download, UpdateService};
use crate::ui::update_view::{UpdateView, UpdateViewEvent};
use crate::ui::window_chrome;
use crate::ui::{dialog_chrome, VaultState, DIALOG_FORM, DIALOG_NARROW, DIALOG_WIDE};
use crate::vault::{AuthMethod, ServerEntry, SnippetBody};
use flint_core::route::{self, CredentialIssue};

actions!(
    app_view,
    [
        ToggleSettings,
        ToggleCommandPalette,
        LockVault,
        NewConnection,
        ToggleFilesFocus,
        NewTerminal,
        ToggleSftpPane,
        CloseTab,
        NextTab,
        PrevTab,
        ActivateTab1,
        ActivateTab2,
        ActivateTab3,
        ActivateTab4,
        ActivateTab5,
        ActivateTab6,
        ActivateTab7,
        ActivateTab8,
        ActivateTab9,
        Quit,
    ]
);

pub fn init(cx: &mut App) {
    crate::ui::command_palette::init(cx);

    // A global handler, so it also works on the lock screens.
    cx.on_action(|_: &Quit, cx| {
        tracing::info!("quit from the keyboard");
        cx.quit();
    });

    // `bind_chord` also registers each chord as it arrives on the Russian
    // layout; chords with no letter in them come out as a single binding.
    #[cfg(target_os = "macos")]
    {
        bind_chord(cx, "cmd-q", Quit, None);
        bind_chord(cx, "cmd-,", ToggleSettings, None);
        bind_chord(cx, "cmd-p", ToggleCommandPalette, None);
        bind_chord(cx, "cmd-l", LockVault, None);
        bind_chord(cx, "cmd-t", NewConnection, None);
        bind_chord(cx, "cmd-shift-t", NewTerminal, None);
        bind_chord(cx, "cmd-b", ToggleSftpPane, None);
        bind_chord(cx, "cmd-w", CloseTab, None);
        bind_chord(cx, "cmd-shift-e", ToggleFilesFocus, None);
        bind_chord(cx, "cmd-shift-]", NextTab, None);
        bind_chord(cx, "cmd-shift-[", PrevTab, None);
        bind_chord(cx, "cmd-1", ActivateTab1, None);
        bind_chord(cx, "cmd-2", ActivateTab2, None);
        bind_chord(cx, "cmd-3", ActivateTab3, None);
        bind_chord(cx, "cmd-4", ActivateTab4, None);
        bind_chord(cx, "cmd-5", ActivateTab5, None);
        bind_chord(cx, "cmd-6", ActivateTab6, None);
        bind_chord(cx, "cmd-7", ActivateTab7, None);
        bind_chord(cx, "cmd-8", ActivateTab8, None);
        bind_chord(cx, "cmd-9", ActivateTab9, None);
    }

    // Windows/Linux equivalents. Plain Ctrl+P/T/W/L belong to readline at a shell
    // prompt, so the chords that would collide take Ctrl+Shift, the same
    // convention Windows Terminal uses. Tab cycling goes through Ctrl+Tab:
    // bracket keys are shifted on most layouts, so "ctrl-shift-]" would arrive
    // as "ctrl-}" and never match.
    #[cfg(not(target_os = "macos"))]
    {
        bind_chord(cx, "ctrl-,", ToggleSettings, None);
        bind_chord(cx, "ctrl-shift-p", ToggleCommandPalette, None);
        bind_chord(cx, "ctrl-shift-l", LockVault, None);
        bind_chord(cx, "ctrl-shift-t", NewConnection, None);
        bind_chord(cx, "ctrl-shift-n", NewTerminal, None);
        bind_chord(cx, "ctrl-shift-b", ToggleSftpPane, None);
        bind_chord(cx, "ctrl-shift-w", CloseTab, None);
        bind_chord(cx, "ctrl-shift-e", ToggleFilesFocus, None);
        bind_chord(cx, "ctrl-tab", NextTab, None);
        bind_chord(cx, "ctrl-shift-tab", PrevTab, None);
        bind_chord(cx, "ctrl-1", ActivateTab1, None);
        bind_chord(cx, "ctrl-2", ActivateTab2, None);
        bind_chord(cx, "ctrl-3", ActivateTab3, None);
        bind_chord(cx, "ctrl-4", ActivateTab4, None);
        bind_chord(cx, "ctrl-5", ActivateTab5, None);
        bind_chord(cx, "ctrl-6", ActivateTab6, None);
        bind_chord(cx, "ctrl-7", ActivateTab7, None);
        bind_chord(cx, "ctrl-8", ActivateTab8, None);
        bind_chord(cx, "ctrl-9", ActivateTab9, None);
    }
}

/// Label on the title-bar palette trigger. Must track the binding above.
#[cfg(target_os = "macos")]
const PALETTE_SHORTCUT: &str = "\u{2318}P";
#[cfg(not(target_os = "macos"))]
const PALETTE_SHORTCUT: &str = "Ctrl+Shift+P";

/// How long after an unlock the "Check for updates?" question waits.
const UPDATE_QUESTION_DELAY: std::time::Duration = std::time::Duration::from_millis(600);
/// How long small vault changes, like the last folder of a host, wait before
/// they are written. Folder changes come in bursts.
const VAULT_SAVE_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

/// Shown next to "Lock now" and the palette command. Must track the binding above.
#[cfg(target_os = "macos")]
pub const LOCK_SHORTCUT: &str = "\u{2318}L";
#[cfg(not(target_os = "macos"))]
pub const LOCK_SHORTCUT: &str = "Ctrl+Shift+L";

/// What a connect that may first ask for a key passphrase is for.
#[derive(Clone)]
enum ConnectIntent {
    /// Open the host in a tab of its own.
    OpenTab,
    /// Show the host's files in the left pane of this session.
    AttachLeft(WeakEntity<ServerSession>),
}

pub struct AppView {
    focus_handle: FocusHandle,
    /// The tab the keys were last given to. When another tab comes to the
    /// front, `focus_front_tab_if_changed` moves them there.
    focused_tab: Option<EntityId>,
    /// Waits out `shortcut_hints::SHOW_DELAY` while the modifier is held.
    hint_task: Option<Task<()>>,
    /// Writes the vault once small changes stop coming, see `save_vault_soon`.
    vault_save_task: Option<Task<()>>,
    vault_state: Entity<VaultState>,
    servers_view: Entity<ServersView>,
    sidebar: Entity<Sidebar>,
    /// Host and key the open form already warned about; see
    /// `asks_before_sharing_key`.
    key_share_asked: Option<(Uuid, Uuid)>,
    settings_view: Option<Entity<SettingsView>>,
    command_palette: Option<Entity<CommandPalette>>,
    /// Server id and snippet id. When the matching session emits `Connected`,
    /// the snippet runs on it.
    pending_snippet_for_server: Option<(String, Uuid)>,
    /// Sessions that connected while the vault was locked. Their startup
    /// snippet lives in the vault, so it runs once the vault is open again.
    sessions_owed_startup_snippet: Vec<WeakEntity<ServerSession>>,
    /// Dead sessions that asked for a fresh connection while the vault was
    /// locked (a wake from sleep does this).
    sessions_owed_reconnect: Vec<WeakEntity<ServerSession>>,
    /// An "open with" request that arrived while the vault was locked.
    picker_owed_after_unlock: Option<(String, PathBuf)>,
    status: String,
    status_clear_task: Option<Task<()>>,
    window_handle: AnyWindowHandle,
    file_associations: SharedFileAssociations,
    tabs: Vec<Entity<ServerSession>>,
    active_tab: usize,
}

impl Focusable for AppView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl AppView {
    pub fn new(
        vault_state: Entity<VaultState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let servers_view = cx.new(|cx| ServersView::new(vault_state.clone(), window, cx));
        let sidebar = cx.new(|cx| Sidebar::new(vault_state.clone(), window, cx));

        cx.observe(&vault_state, |this, _, cx| this.follow_renamed_hosts(cx))
            .detach();
        // The title bar shows the update mark.
        cx.observe(&UpdateService::global(cx), |_, _, cx| cx.notify())
            .detach();

        cx.subscribe_in(&sidebar, window, |this, _, event, window, cx| match event {
            SidebarEvent::OpenServer(id) => this.open_server_by_id(*id, window, cx),
            SidebarEvent::CloseServer(id) => this.close_server_by_id(*id, cx),
            SidebarEvent::AddServerRequested => this.show_server_form(FormMode::Add, window, cx),
            SidebarEvent::ShowAllServers => this.show_all_servers(cx),
            SidebarEvent::EditServerRequested(id) => this.show_edit_form(*id, window, cx),
            SidebarEvent::DeleteServerRequested(id) => this.show_remove_dialog(*id, window, cx),
            SidebarEvent::SettingsRequested => this.toggle_settings(window, cx),
            SidebarEvent::FeedbackRequested => this.show_feedback_dialog(window, cx),
        })
        .detach();

        cx.subscribe_in(
            &servers_view,
            window,
            |this, _, event, window, cx| match event {
                ServersViewEvent::OpenServer(summary) => {
                    this.open_server_by_id(summary.id, window, cx)
                }
                ServersViewEvent::AddServerRequested => {
                    this.show_server_form(FormMode::Add, window, cx)
                }
                ServersViewEvent::ImportRequested => this.show_import_dialog(window, cx),
                ServersViewEvent::EditServerRequested(id) => this.show_edit_form(*id, window, cx),
                ServersViewEvent::DuplicateServerRequested(id) => this.duplicate_server(*id, cx),
                ServersViewEvent::DeleteServerRequested(id) => {
                    this.show_remove_dialog(*id, window, cx)
                }
            },
        )
        .detach();

        cx.observe_window_activation(window, |this, window, cx| {
            // The modifier may be let go in another app, out of our sight.
            if !window.is_window_active() {
                this.hint_task = None;
                shortcut_hints::set_shown(false, window, cx);
            }
            // Behind a lock screen this view is not on screen; taking focus
            // here would leave the password field without it.
            if this.is_vault_locked(cx) {
                return;
            }
            if !window.is_window_active() {
                return;
            }
            // A host shared while the user was away shows up when they come back.
            SyncService::global(cx).update(cx, |service, cx| service.refresh_if_stale(cx));
            // A field, a file pane or a dialog that has the keys keeps them.
            // Otherwise they go to the terminal, so that coming back to flint
            // and pasting works without a click first.
            let is_unclaimed = window.focused(cx).is_none() || this.focus_handle.is_focused(window);
            if !is_unclaimed {
                return;
            }
            if this.is_overlay_open(window, cx) || !this.focus_active_terminal(window, cx) {
                this.focus_handle.focus(window, cx);
            }
        })
        .detach();

        // Sleep takes the network down with the machine, so every open session
        // comes back to a dead link and a backoff timer that was counting down
        // against a network that no longer existed.
        let weak_self = cx.entity().downgrade();
        cx.on_system_wake(move |cx| {
            weak_self
                .update(cx, |this, cx| this.retry_sessions_after_wake(cx))
                .ok();
        })
        .detach();

        cx.on_app_quit(|this, cx| {
            this.save_vault_now(cx);
            async {}
        })
        .detach();

        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);

        Self {
            focus_handle,
            focused_tab: None,
            hint_task: None,
            vault_save_task: None,
            vault_state,
            servers_view,
            sidebar,
            key_share_asked: None,
            settings_view: None,
            command_palette: None,
            pending_snippet_for_server: None,
            sessions_owed_startup_snippet: Vec::new(),
            sessions_owed_reconnect: Vec::new(),
            picker_owed_after_unlock: None,
            status: String::new(),
            status_clear_task: None,
            window_handle: window.window_handle(),
            file_associations: std::rc::Rc::new(std::cell::RefCell::new(
                FileAssociations::load_or_default(),
            )),
            tabs: vec![],
            active_tab: 0,
        }
    }

    fn is_vault_locked(&self, cx: &App) -> bool {
        self.vault_state.read(cx).manager().is_none()
    }

    /// Lock the vault and leave every open session running behind the lock
    /// screen. Sessions keep the route they connected with, so they stay up
    /// and can reconnect without the vault.
    fn lock_vault(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_vault_locked(cx) {
            return;
        }

        self.close_overlays(window, cx);

        self.vault_state.update(cx, |vault_state, cx| {
            vault_state.lock();
            cx.notify();
        });
        tracing::info!(open_sessions = self.tabs.len(), "vault locked");
        cx.notify();
    }

    /// Close what sits on top of the main view. Anything left open would keep
    /// vault data alive (an edit form holds cleartext credentials) and would
    /// be back on screen after the next unlock.
    fn close_overlays(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_view = None;
        self.command_palette = None;
        window.close_all_dialogs(cx);
        self.status.clear();
        self.status_clear_task = None;
    }

    /// Called by `RootView` when the vault was deleted. Unlike a lock, this
    /// closes the sessions too: each belongs to a host of the old vault, and
    /// the next vault to open here must not find them.
    pub fn close_everything(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_overlays(window, cx);
        let closed_sessions = self.tabs.len();
        for session in std::mem::take(&mut self.tabs) {
            session.update(cx, |session, cx| session.cleanup_before_close(cx));
        }
        self.pending_snippet_for_server = None;
        self.sessions_owed_startup_snippet.clear();
        self.sessions_owed_reconnect.clear();
        self.picker_owed_after_unlock = None;
        self.change_active_tab(0, cx);
        tracing::info!(
            closed_sessions,
            "closed everything that belonged to the deleted vault"
        );
    }

    /// Called by `RootView` each time the vault opens: put focus back where
    /// the user works and catch up on what could not be done while locked.
    pub fn resume_after_unlock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_handle.focus(window, cx);

        // Reconnects first: each swaps a dead tab for a new one and activates it.
        for session in std::mem::take(&mut self.sessions_owed_reconnect) {
            if let Some(session) = session.upgrade() {
                self.reconnect_session(&session, window, cx);
            }
        }

        // The active tab then takes focus for its terminal, if it has one, and
        // brings back a conflict dialog that the lock closed. A reconnect may
        // have opened a passphrase prompt; focus must stay in it.
        if !window.has_active_dialog(cx) {
            self.set_active_tab(self.active_tab, window, cx);
        }

        for session in std::mem::take(&mut self.sessions_owed_startup_snippet) {
            if let Some(session) = session.upgrade() {
                self.run_startup_snippet(&session, cx);
            }
        }
        self.try_drain_pending_snippet(cx);
        if let Some((extension, local)) = self.picker_owed_after_unlock.take() {
            self.show_picker_for(extension, local, cx);
        }
        self.ask_about_update_checks_soon(window, cx);
    }

    /// Put the "Check for updates?" question if it is still open. It waits a
    /// moment: the dialogs an unlock brings back open on a later tick, and
    /// the question must not end up stacked with them.
    fn ask_about_update_checks_soon(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(UPDATE_QUESTION_DELAY).await;
            this.update_in(cx, |this, window, cx| {
                this.ask_about_update_checks(window, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Never over the lock screen or another overlay: that one is waiting
    /// for the user, and the question can wait for the next unlock or start.
    fn ask_about_update_checks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_vault_locked(cx) || self.is_overlay_open(window, cx) {
            return;
        }
        let service = UpdateService::global(cx);
        if !service.update(cx, |service, cx| service.take_question(cx)) {
            return;
        }
        let view = cx.new(|_| UpdateConsentView);
        cx.subscribe_in(
            &view,
            window,
            |_, _, event: &UpdateConsentEvent, window, cx| match event {
                // close_dialog puts focus back where it was, often a terminal
                UpdateConsentEvent::Answered => window.close_dialog(cx),
            },
        )
        .detach();
        window.open_dialog(&mut *cx, move |dialog, _, _| {
            dialog_chrome(dialog).w(DIALOG_NARROW).child(view.clone())
        });
    }

    fn set_status(&mut self, msg: String, cx: &mut Context<Self>) {
        self.status = msg;
        self.status_clear_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(3))
                .await;
            this.update(cx, |app, cx| {
                app.status.clear();
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn toggle_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_view.is_some() {
            self.settings_view = None;
            self.focus_handle.focus(window, cx);
        } else {
            self.command_palette = None;
            let sv = cx.new(|cx| SettingsView::new(self.vault_state.clone(), window, cx));
            cx.subscribe_in(
                &sv,
                window,
                |this, _, event: &SettingsViewEvent, window, cx| match event {
                    SettingsViewEvent::Close => {
                        this.settings_view = None;
                        this.focus_handle.focus(window, cx);
                        cx.notify();
                    }
                    SettingsViewEvent::LockRequested => this.lock_vault(window, cx),
                    SettingsViewEvent::RunSnippet(snippet_id) => {
                        let id = *snippet_id;
                        this.settings_view = None;
                        this.run_snippet_on_active_session(id, cx);
                        this.focus_handle.focus(window, cx);
                        cx.notify();
                    }
                },
            )
            .detach();
            self.settings_view = Some(sv);
        }
        cx.notify();
    }

    fn toggle_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.command_palette.is_some() {
            self.command_palette = None;
            self.focus_handle.focus(window, cx);
        } else {
            self.settings_view = None;
            let has_active_server = self.active_session_shell_handle(cx).is_some();
            let active_server_id: Option<Uuid> = self
                .tabs
                .get(self.active_tab)
                .and_then(|s| Uuid::parse_str(&s.read(cx).server_id).ok());
            let connected_ids: std::collections::HashSet<String> = self
                .tabs
                .iter()
                .map(|s| s.read(cx).server_id.clone())
                .collect();
            let vault_state = self.vault_state.clone();
            let palette = cx.new(|cx| {
                CommandPalette::new(
                    vault_state,
                    has_active_server,
                    active_server_id,
                    connected_ids,
                    window,
                    cx,
                )
            });
            cx.subscribe_in(
                &palette,
                window,
                |this, _, event: &CommandPaletteEvent, window, cx| {
                    match event {
                        CommandPaletteEvent::Close => {
                            this.command_palette = None;
                            this.focus_handle.focus(window, cx);
                            cx.notify();
                        }
                        CommandPaletteEvent::OpenServer(id) => {
                            this.command_palette = None;
                            this.open_server_by_id(*id, window, cx);
                            cx.notify();
                        }
                        CommandPaletteEvent::QuickConnect(target) => {
                            this.command_palette = None;
                            this.quick_connect(target, window, cx);
                            cx.notify();
                        }
                        CommandPaletteEvent::RunSnippet {
                            snippet_id,
                            server_id,
                        } => {
                            let snip = *snippet_id;
                            this.command_palette = None;
                            match server_id {
                                None => {
                                    this.run_snippet_on_active_session(snip, cx);
                                    this.focus_handle.focus(window, cx);
                                }
                                Some(srv) => {
                                    this.pending_snippet_for_server = Some((srv.to_string(), snip));
                                    this.open_server_by_id(*srv, window, cx);
                                    // Runs the snippet if the host was already
                                    // connected. Otherwise the Connected handler does.
                                    this.try_drain_pending_snippet(cx);
                                }
                            }
                            cx.notify();
                        }
                        CommandPaletteEvent::RunCommand(command) => {
                            this.command_palette = None;
                            this.focus_handle.focus(window, cx);
                            this.run_palette_command(*command, window, cx);
                            cx.notify();
                        }
                        // Only the left-source picker emits this.
                        CommandPaletteEvent::LeftSourcePicked(_) => {}
                    }
                },
            )
            .detach();
            self.command_palette = Some(palette);
        }
        cx.notify();
    }

    fn run_palette_command(
        &mut self,
        command: PaletteCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match command {
            PaletteCommand::NewHost => self.show_server_form(FormMode::Add, window, cx),
            PaletteCommand::ImportHosts => self.show_import_dialog(window, cx),
            PaletteCommand::OpenSettings => self.toggle_settings(window, cx),
            PaletteCommand::LockVault => self.lock_vault(window, cx),
            PaletteCommand::SetTheme(pref) => {
                crate::ui::settings::set_theme_preference(pref, window, cx)
            }
            PaletteCommand::CloseSession => {
                self.close_tab(self.active_tab, cx);
                // The palette had the focus; give it to the tab that is now shown.
                self.set_active_tab(self.active_tab, window, cx);
            }
            PaletteCommand::NewTerminal => self.open_extra_terminal(cx),
            PaletteCommand::ToggleSftpPane => self.toggle_sftp_pane(cx),
            PaletteCommand::FocusFiles => {
                let Some(session) = self.tabs.get(self.active_tab) else {
                    return;
                };
                session.update(cx, |session, cx| session.toggle_files_focus(window, cx));
            }
            PaletteCommand::ReconnectSession => {
                let Some(session) = self.tabs.get(self.active_tab) else {
                    return;
                };
                let session = session.clone();
                self.reconnect_session(&session, window, cx);
            }
            PaletteCommand::ChooseLeftSource => {
                let Some(session) = self.tabs.get(self.active_tab) else {
                    return;
                };
                let session = session.clone();
                self.show_left_source_picker(&session, window, cx);
            }
        }
    }

    /// Ask what the left file pane of `session` should show: this computer
    /// or another saved server.
    fn show_left_source_picker(
        &mut self,
        session: &Entity<ServerSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The list of hosts and their credentials are in the vault.
        if self.is_vault_locked(cx) {
            return;
        }
        if let Some(refusal) = session.read(cx).left_switch_refusal(cx) {
            self.set_status(refusal, cx);
            return;
        }
        let Ok(session_server_id) = Uuid::parse_str(&session.read(cx).server_id) else {
            return;
        };
        let connected_ids: std::collections::HashSet<String> = self
            .tabs
            .iter()
            .map(|s| s.read(cx).server_id.clone())
            .collect();

        self.settings_view = None;
        let vault_state = self.vault_state.clone();
        let palette = cx.new(|cx| {
            CommandPalette::new_left_source_picker(
                vault_state,
                session_server_id,
                connected_ids,
                window,
                cx,
            )
        });
        let session = session.downgrade();
        cx.subscribe_in(
            &palette,
            window,
            move |this, _, event: &CommandPaletteEvent, window, cx| {
                let choice = match event {
                    CommandPaletteEvent::Close => None,
                    CommandPaletteEvent::LeftSourcePicked(choice) => Some(*choice),
                    // The picker lists nothing that emits the other events.
                    _ => return,
                };
                this.command_palette = None;
                this.focus_handle.focus(window, cx);
                if let Some(choice) = choice {
                    this.open_peer(&session, choice, window, cx);
                }
                cx.notify();
            },
        )
        .detach();
        self.command_palette = Some(palette);
        cx.notify();
    }

    /// Put `choice` in the left file pane of `session`: this computer
    /// (`None`) or a saved server. The server gets a connection of its own,
    /// so it does not matter whether it also has a tab.
    fn open_peer(
        &mut self,
        session: &WeakEntity<ServerSession>,
        choice: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session_entity) = session.upgrade() else {
            return;
        };
        // The picker had the keys. They go to the pane the choice is about
        // and follow it from there: placeholder first, then the file list.
        session_entity.update(cx, |session, cx| session.focus_left_files(window, cx));
        let entry_id = choice.map(|id| id.to_string());
        if session_entity.read(cx).is_left_source(entry_id.as_deref()) {
            return;
        }
        // The pane may know the chosen host by the id it had before its team
        // vault was lost. It is the same host, and it is already shown.
        let shown = session_entity
            .read(cx)
            .left_source_entry()
            .and_then(|id| Uuid::parse_str(id).ok());
        let is_shown_under_old_id = match (shown, choice, self.vault_state.read(cx).manager()) {
            (Some(shown), Some(choice), Some(vault)) => vault.current_id(shown) == choice,
            _ => false,
        };
        if is_shown_under_old_id {
            return;
        }
        let Some(id) = choice else {
            session_entity.update(cx, |session, cx| session.use_local_left(cx));
            return;
        };
        let entry = self
            .vault_state
            .read(cx)
            .manager()
            .and_then(|vm| vm.get_entry(id).ok());
        let Some(entry) = entry else { return };
        self.connect_with_prompt(
            entry,
            ConnectIntent::AttachLeft(session.clone()),
            window,
            cx,
        );
    }

    fn try_drain_pending_snippet(&mut self, cx: &mut Context<Self>) {
        let Some((server_id, snippet_id)) = self.pending_snippet_for_server.clone() else {
            return;
        };
        // The snippet is read from the vault; `resume_after_unlock` tries again.
        if self.is_vault_locked(cx) {
            return;
        }
        // The host's own tab, not the one in front: the user may have
        // switched tabs while it was connecting.
        let Some(idx) = self.tab_index_for_host(&server_id, cx) else {
            return;
        };
        let session = self.tabs[idx].clone();
        if session.read(cx).shell_handle(cx).is_none() {
            return;
        }
        self.pending_snippet_for_server = None;
        self.run_snippet_on_session(snippet_id, &session, cx);
    }

    /// The connect a snippet was queued for did not start, so the snippet
    /// must not run on some later connect to the same host.
    fn forget_pending_snippet(&mut self, server_id: Uuid) {
        let server_id = server_id.to_string();
        let is_for_host = self
            .pending_snippet_for_server
            .as_ref()
            .is_some_and(|(pending, _)| *pending == server_id);
        if is_for_host {
            self.pending_snippet_for_server = None;
        }
    }

    fn tab_index_for_host(&self, server_id: &str, cx: &App) -> Option<usize> {
        self.tabs
            .iter()
            .position(|session| session.read(cx).server_id == server_id)
    }

    fn active_session_shell_handle(&self, cx: &App) -> Option<crate::shell::ShellHandle> {
        let session = self.tabs.get(self.active_tab)?;
        session.read(cx).shell_handle(cx)
    }

    fn run_snippet_on_active_session(&mut self, snippet_id: Uuid, cx: &mut Context<Self>) {
        let Some(session) = self.tabs.get(self.active_tab).cloned() else {
            tracing::warn!("snippet run requested but no session is in front");
            return;
        };
        self.run_snippet_on_session(snippet_id, &session, cx);
    }

    fn run_snippet_on_session(
        &mut self,
        snippet_id: Uuid,
        session: &Entity<ServerSession>,
        cx: &mut Context<Self>,
    ) {
        let snippet = self
            .vault_state
            .read(cx)
            .manager()
            .and_then(|vm| vm.get_snippet(snippet_id).ok().cloned());
        let Some(snippet) = snippet else {
            tracing::warn!(?snippet_id, "snippet not found for run");
            return;
        };
        let Some(shell) = session.read(cx).shell_handle(cx) else {
            tracing::warn!(
                ?snippet_id,
                "snippet run requested but the session has no shell"
            );
            return;
        };

        if let Ok(server_id) = Uuid::parse_str(&session.read(cx).server_id) {
            self.record_snippet_used(snippet_id, server_id, cx);
        }

        let SnippetBody::Script { content } = snippet.body;
        send_script(shell, content, cx);
    }

    /// Run the host's startup snippet on the session that just connected.
    /// Every reconnect gets a fresh shell, so it runs again.
    fn run_startup_snippet(&mut self, session: &Entity<ServerSession>, cx: &mut Context<Self>) {
        let Ok(server_id) = Uuid::parse_str(&session.read(cx).server_id) else {
            return;
        };
        if self.is_vault_locked(cx) {
            let session = session.downgrade();
            if !self.sessions_owed_startup_snippet.contains(&session) {
                self.sessions_owed_startup_snippet.push(session);
            }
            return;
        }
        let snippet = self.vault_state.read(cx).manager().and_then(|vm| {
            let entry = vm.get_entry(server_id).ok()?;
            vm.get_snippet(entry.startup_snippet?).ok().cloned()
        });
        let Some(snippet) = snippet else { return };
        let Some(shell) = session.read(cx).shell_handle(cx) else {
            return;
        };

        tracing::info!(%server_id, snippet = %snippet.name, "running startup snippet");
        self.record_snippet_used(snippet.id, server_id, cx);
        let SnippetBody::Script { content } = snippet.body;
        send_script(shell, content, cx);
    }

    /// Give the keys to the terminal of the tab in front. `false` when there
    /// is no such terminal.
    fn focus_active_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(session) = self.tabs.get(self.active_tab) else {
            return false;
        };
        let terminal = session
            .read(cx)
            .active_terminal()
            .map(|terminal| terminal.read(cx).focus_handle(cx));
        let Some(terminal) = terminal else {
            return false;
        };
        terminal.focus(window, cx);
        true
    }

    fn set_active_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() {
            return;
        }
        self.change_active_tab(idx, cx);

        if let Some(session) = self.tabs.get(idx) {
            let session = session.clone();
            self.focus_front_tab(window, cx);
            if session.read(cx).pending_conflict.is_some() {
                self.open_conflict_dialog(session.clone(), cx);
            }
            // A question asked while another tab was in front.
            self.open_transfer_conflict_dialog(session.clone(), cx);
        } else {
            self.focus_handle.focus(window, cx);
        }
    }

    /// Give the keys to the tab in front: its terminal, the field it waits
    /// on, or this view while it has neither. They must not stay on the tab
    /// that was in front before: its elements are no longer drawn, and gpui
    /// then hands every key to the window root, past the shortcuts here
    /// (Cmd+1 did nothing while the tab in front was still connecting).
    fn focus_front_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let front = self.tabs.get(self.active_tab).cloned();
        self.focused_tab = front.as_ref().map(|session| session.entity_id());
        match front.and_then(|session| session.read(cx).focus_target(cx)) {
            Some(target) => target.focus(window, cx),
            None => self.focus_handle.focus(window, cx),
        }
    }

    /// `focus_front_tab` for every way a tab can come to the front: a click
    /// in the sidebar, a closed tab, a host opened from the list. A dialog
    /// or an overlay keeps the keys until it closes.
    fn focus_front_tab_if_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let front = self
            .tabs
            .get(self.active_tab)
            .map(|session| session.entity_id());
        if front == self.focused_tab || self.is_overlay_open(window, cx) {
            return;
        }
        self.focus_front_tab(window, cx);
    }

    /// Show the shortcut hints once the modifier has been held a moment, and
    /// hide them as soon as it is let go or joined by another one.
    fn on_modifiers_changed(
        &mut self,
        event: &ModifiersChangedEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Over the palette or the settings the hints would only shine through
        // the dimmed window.
        if !shortcut_hints::is_hint_modifier(&event.modifiers) || self.is_overlay_open(window, cx) {
            self.hint_task = None;
            shortcut_hints::set_shown(false, window, cx);
            return;
        }
        if self.hint_task.is_some() || shortcut_hints::is_shown(cx) {
            return;
        }
        self.hint_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(shortcut_hints::SHOW_DELAY)
                .await;
            this.update_in(cx, |this, window, cx| {
                this.hint_task = None;
                shortcut_hints::set_shown(true, window, cx);
            })
            .ok();
        }));
    }

    fn open_server_by_id(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let entry = self
            .vault_state
            .read(cx)
            .manager()
            .and_then(|vm| vm.get_entry(id).ok());
        let Some(entry) = entry else {
            self.forget_pending_snippet(id);
            return;
        };
        self.open_server(entry, window, cx);
    }

    /// Connect to an address typed into the palette. A host that is already
    /// saved is opened as is; a new one is saved first (agent auth), so host
    /// key trust, recents and reconnect work the same as for any other host.
    fn quick_connect(&mut self, target: &SshTarget, window: &mut Window, cx: &mut Context<Self>) {
        // Every early return below leaves no session to take the focus the
        // palette had, so hand it back first.
        self.focus_handle.focus(window, cx);
        let Some(user) = target.user.clone() else {
            return;
        };
        let port = target.port.unwrap_or(22);
        let entries = self
            .vault_state
            .read(cx)
            .manager()
            .and_then(|vm| vm.list_entries().ok())
            .unwrap_or_default();

        if let Some(id) = find_saved_host(&entries, &user, &target.host, port) {
            self.open_server_by_id(id, window, cx);
            return;
        }

        // A jump host that is not saved has no credentials to connect with.
        // Going direct instead would be a different route than the one asked for.
        let jump_host = match &target.jump {
            None => None,
            Some(jump) => match find_saved_jump_host(&entries, jump) {
                Some(id) => Some(id),
                None => {
                    tracing::warn!(host = %target.host, jump = %jump, "quick connect: jump host is not saved");
                    self.set_status(
                        format!("Jump host \u{201C}{jump}\u{201D} is not saved. Add it first."),
                        cx,
                    );
                    return;
                }
            },
        };

        let entry = quick_connect_entry(user, target.host.clone(), port, jump_host);
        let result = self.vault_state.update(cx, |vs, cx| {
            let r = vs.manager_mut().map(|vm| vm.add_entry(entry.clone()));
            cx.notify();
            r
        });
        match result {
            Some(Ok(())) => {}
            Some(Err(e)) => {
                tracing::error!(?e, host = %entry.host, "quick connect: failed to save host");
                self.set_status(format!("Could not save {}: {e}", entry.host), cx);
                return;
            }
            None => return,
        }

        tracing::info!(host = %entry.host, user = %entry.username, port, "quick connect: host saved");
        let label = entry.label.clone();
        self.open_server(entry, window, cx);
        self.set_status(format!("Saved as {label}"), cx);
    }

    fn show_all_servers(&mut self, cx: &mut Context<Self>) {
        self.change_active_tab(self.tabs.len(), cx);
    }

    fn change_active_tab(&mut self, idx: usize, cx: &mut Context<Self>) {
        self.active_tab = idx;
        let active_id = self.tabs.get(idx).map(|s| s.read(cx).server_id.clone());
        self.sidebar
            .update(cx, |s, cx| s.set_active_server(active_id, cx));
        self.sync_session_states(cx);
        cx.notify();
    }

    /// Push each session's connection state to the sidebar and the all-servers
    /// grid, which show it next to the host.
    fn sync_session_states(&mut self, cx: &mut Context<Self>) {
        // In tab order, so the sidebar's OPEN zone does not reshuffle as
        // sessions come and go.
        let mut sessions: Vec<(String, HostStatus)> = Vec::with_capacity(self.tabs.len());
        let mut open_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        for s in &self.tabs {
            let sess = s.read(cx);
            let id = sess.server_id.clone();
            let status = if sess.browser.is_some() {
                HostStatus::Connected
            } else if sess.disconnected.is_some() || sess.has_connect_failure() {
                HostStatus::Disconnected
            } else {
                HostStatus::Connecting
            };
            sessions.push((id.clone(), status));
            open_ids.insert(id);
        }
        self.sidebar
            .update(cx, |s, cx| s.set_sessions(sessions, cx));
        self.servers_view
            .update(cx, |s, cx| s.set_connected_ids(open_ids, cx));
    }

    /// A host whose team vault was lost goes on under a new id. Move the
    /// open sessions over to it, or the host would show as closed in the
    /// lists and a click on it would connect a second time.
    fn follow_renamed_hosts(&mut self, cx: &mut Context<Self>) {
        let renamed: Vec<(Entity<ServerSession>, String)> = {
            let Some(vault) = self.vault_state.read(cx).manager() else {
                return;
            };
            self.tabs
                .iter()
                .filter_map(|session| {
                    let id = Uuid::parse_str(&session.read(cx).server_id).ok()?;
                    let current = vault.current_id(id);
                    (current != id).then(|| (session.clone(), current.to_string()))
                })
                .collect()
        };
        if renamed.is_empty() {
            return;
        }
        for (session, id) in renamed {
            tracing::info!(host = %id, "open session follows its host to a new id");
            session.update(cx, |session, _| session.server_id = id);
        }
        self.change_active_tab(self.active_tab, cx);
    }

    fn close_server_by_id(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if let Some(idx) = self.tab_index_for_host(&id.to_string(), cx) {
            self.close_tab(idx, cx);
        }
    }

    /// Persist the recency timestamp the sidebar uses to order recent hosts.
    /// Best-effort: a write failure is logged, never surfaced.
    fn record_open(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let result = self.vault_state.update(cx, |vs, cx| {
            let r = vs.manager_mut().map(|vm| vm.touch_entry_opened(id));
            cx.notify();
            r
        });
        if let Some(Err(e)) = result {
            tracing::warn!(?e, "failed to record host open time");
        }
        self.save_vault_soon(cx);
    }

    /// Write the small vault changes a moment after the last one.
    fn save_vault_soon(&mut self, cx: &mut Context<Self>) {
        self.vault_save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(VAULT_SAVE_DELAY).await;
            this.update(cx, |this, cx| this.save_vault_now(cx)).ok();
        }));
    }

    fn save_vault_now(&mut self, cx: &mut Context<Self>) {
        self.vault_save_task = None;
        let result = self
            .vault_state
            .update(cx, |vs, _| vs.manager_mut().map(|vm| vm.save_pending()));
        if let Some(Err(e)) = result {
            tracing::warn!(?e, "failed to save the vault");
        }
    }

    /// Persist the per-server recency the command palette uses to surface
    /// recently-used snippets. Best-effort: a write failure is logged only.
    fn record_snippet_used(&mut self, snippet_id: Uuid, server_id: Uuid, cx: &mut Context<Self>) {
        let result = self.vault_state.update(cx, |vs, _| {
            vs.manager_mut()
                .map(|vm| vm.touch_snippet_used(snippet_id, server_id))
        });
        if let Some(Err(e)) = result {
            tracing::warn!(?e, "failed to record snippet usage");
        }
    }

    fn open_server(&mut self, entry: ServerEntry, window: &mut Window, cx: &mut Context<Self>) {
        let server_id = entry.id.to_string();

        // Bump recency (covers both opening fresh and re-activating an
        // existing tab) so the sidebar's RECENT zone reflects real usage.
        self.record_open(entry.id, cx);

        // Single-open guard: activate existing tab if already open.
        if let Some(idx) = self.tab_index_for_host(&server_id, cx) {
            self.change_active_tab(idx, cx);
            return;
        }

        self.connect_with_prompt(entry, ConnectIntent::OpenTab, window, cx);
    }

    /// Resolve the host's credential, asking for a key passphrase when one
    /// is needed, then connect for `intent`.
    fn connect_with_prompt(
        &mut self,
        entry: ServerEntry,
        intent: ConnectIntent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let credential = match self.vault_state.read(cx).manager() {
            Some(vm) => route::target_credential(vm, &entry),
            None => {
                if matches!(intent, ConnectIntent::OpenTab) {
                    self.forget_pending_snippet(entry.id);
                }
                self.set_status("The vault is locked.".to_string(), cx);
                return;
            }
        };
        let credential = match credential {
            Ok(credential) => credential,
            // The answer goes into the credential, so reconnects do not ask again.
            Err(CredentialIssue::Passphrase {
                key_name,
                pem,
                issue,
            }) => {
                self.show_passphrase_prompt(entry, intent, key_name, pem, issue, window, cx);
                return;
            }
            Err(CredentialIssue::KeyMissing) => {
                if matches!(intent, ConnectIntent::OpenTab) {
                    self.forget_pending_snippet(entry.id);
                }
                self.set_status(
                    "SSH key not found. Reassign it in Settings.".to_string(),
                    cx,
                );
                return;
            }
        };

        self.finish_connect(entry, credential, intent, window, cx);
    }

    /// The credential is in hand: open the tab, or attach the host to the
    /// left pane of the session that asked.
    fn finish_connect(
        &mut self,
        entry: ServerEntry,
        credential: AuthCredential,
        intent: ConnectIntent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session = match intent {
            ConnectIntent::OpenTab => {
                self.launch_session(entry, credential, window, cx);
                return;
            }
            ConnectIntent::AttachLeft(session) => session,
        };
        // The tab may have been closed while the passphrase dialog was up.
        let Some(session) = session.upgrade() else {
            return;
        };
        let route = match self.build_route(&entry, credential, cx) {
            Ok(route) => route,
            Err(message) => {
                tracing::warn!(host = %entry.label, message, "cannot build the left server's route");
                self.set_status(message, cx);
                return;
            }
        };
        // Asked again: a transfer may have started while the dialog was up.
        if let Some(refusal) = session.read(cx).left_switch_refusal(cx) {
            self.set_status(refusal, cx);
            return;
        }
        let info = connection_info(&entry);
        session.update(cx, |session, cx| session.connect_peer(info, route, cx));
    }

    /// Modal asking for an encrypted key's passphrase before connecting.
    /// The answer is checked against the PEM here, so a wrong one keeps the
    /// dialog open instead of failing at the server.
    fn show_passphrase_prompt(
        &mut self,
        entry: ServerEntry,
        intent: ConnectIntent,
        key_name: String,
        pem: Zeroizing<String>,
        issue: crate::ssh::KeyIssue,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = format!("{}@{}", entry.username, entry.host);
        let initial_error = matches!(issue, crate::ssh::KeyIssue::WrongPassphrase)
            .then(|| "The saved passphrase no longer decrypts this key.".to_string());
        let view =
            cx.new(|cx| PassphrasePromptView::new(key_name, target, initial_error, window, cx));

        cx.subscribe_in(
            &view,
            window,
            move |this, view, event, window, cx| match event {
                PassphrasePromptEvent::Submitted(passphrase) => {
                    if crate::ssh::check_key_passphrase(&pem, Some(passphrase.as_str())).is_some() {
                        view.update(cx, |v, cx| {
                            v.set_error("Wrong passphrase.".to_string(), cx);
                        });
                        return;
                    }
                    window.close_dialog(cx);
                    let credential = AuthCredential::PrivateKey {
                        pem: pem.clone(),
                        passphrase: Some(passphrase.clone()),
                    };
                    this.finish_connect(entry.clone(), credential, intent.clone(), window, cx);
                }
                PassphrasePromptEvent::Cancelled => {
                    if matches!(intent, ConnectIntent::OpenTab) {
                        this.forget_pending_snippet(entry.id);
                    }
                    window.close_dialog(cx);
                }
            },
        )
        .detach();

        let view_for_dialog = view.clone();
        window.open_dialog(&mut *cx, move |dialog, _, _| {
            dialog_chrome(dialog)
                .w(DIALOG_NARROW)
                .child(view_for_dialog.clone())
        });
    }

    /// The path this session will connect over, with the credential for `entry` already
    /// resolved (and possibly prompted for) by the caller.
    fn build_route(
        &self,
        entry: &ServerEntry,
        credential: AuthCredential,
        cx: &App,
    ) -> Result<SshRoute, String> {
        // Bail rather than fall back: a route built without the vault would
        // quietly skip both the bastions and the proxy and dial the target
        // straight from this network.
        let vault = self.vault_state.read(cx);
        let Some(vm) = vault.manager() else {
            return Err("The vault is locked.".to_string());
        };
        route::route_for(vm, entry, credential)
    }

    fn launch_session(
        &mut self,
        entry: ServerEntry,
        credential: AuthCredential,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let route = match self.build_route(&entry, credential, cx) {
            Ok(route) => route,
            Err(message) => {
                tracing::warn!(host = %entry.label, message, "cannot build the connection route");
                self.forget_pending_snippet(entry.id);
                self.set_status(message, cx);
                return;
            }
        };

        let conn_info = connection_info(&entry);
        // Seed the browser with the folders last shown for this host (persisted
        // together), so a restart or fresh open lands back where the user was.
        let initial_restore = match (entry.last_local_dir.clone(), entry.last_remote_dir.clone()) {
            (Some(local), Some(remote)) => Some(RestorePaths { local, remote }),
            _ => None,
        };
        // The session keeps the forwards itself, so it can start the
        // auto-start ones again on every reconnect without the vault.
        let forwards = forward_runtimes(&entry);
        drop(entry);

        let file_associations = self.file_associations.clone();
        let session = cx.new(|cx| {
            ServerSession::start_connect(
                conn_info,
                route,
                initial_restore,
                forwards,
                file_associations,
                cx,
            )
        });

        cx.subscribe_in(
            &session,
            window,
            |this, session_entity, event, window, cx| match event {
                ServerSessionEvent::StatusChanged(msg) => {
                    this.set_status(msg.clone(), cx);
                    this.sync_session_states(cx);
                }
                ServerSessionEvent::Connected { elapsed } => {
                    // Without a figure the status is cleared, so a stale message
                    // from before the connect does not stay up.
                    let status = match elapsed {
                        Some(elapsed) => connect_time_message(
                            session_entity.read(cx).conn_info.display_name(),
                            *elapsed,
                        ),
                        None => String::new(),
                    };
                    this.set_status(status, cx);
                    this.sync_session_states(cx);
                    this.try_drain_pending_snippet(cx);
                    this.run_startup_snippet(session_entity, cx);
                }
                ServerSessionEvent::ConflictDetected => {
                    let is_active = this.tabs.get(this.active_tab) == Some(session_entity);
                    // A dialog opened behind the lock screen would take focus from
                    // the password field. The conflict stays pending on the session
                    // and `resume_after_unlock` shows it.
                    if is_active && !this.is_vault_locked(cx) {
                        this.open_conflict_dialog(session_entity.clone(), cx);
                    }
                }
                ServerSessionEvent::TransferConflict => {
                    let is_active = this.tabs.get(this.active_tab) == Some(session_entity);
                    // Same as above: nothing may open behind the lock screen.
                    if is_active && !this.is_vault_locked(cx) {
                        this.open_transfer_conflict_dialog(session_entity.clone(), cx);
                    }
                }
                ServerSessionEvent::PickerRequested { extension, local } => {
                    if this.is_vault_locked(cx) {
                        this.picker_owed_after_unlock = Some((extension.clone(), local.clone()));
                    } else {
                        this.show_picker_for(extension.clone(), local.clone(), cx);
                    }
                }
                ServerSessionEvent::OsDetected(os) => {
                    let server_id = session_entity.read(cx).server_id.clone();
                    if let Ok(id) = Uuid::parse_str(&server_id) {
                        let os = os.clone();
                        let result = this.vault_state.update(cx, |vs, cx| {
                            let r = vs.manager_mut().map(|vm| vm.set_entry_os(id, os));
                            cx.notify();
                            r
                        });
                        if let Some(Err(e)) = result {
                            tracing::warn!(?e, "failed to persist detected OS");
                        }
                        this.save_vault_soon(cx);
                    }
                }
                ServerSessionEvent::HostKeyAccepted {
                    entry_id,
                    fingerprint,
                } => {
                    // The key belongs to the hop that presented it, which on a
                    // jumped session is often a bastion, not the tab's host.
                    if let Ok(id) = Uuid::parse_str(entry_id) {
                        let fingerprint = fingerprint.clone();
                        let result = this.vault_state.update(cx, |vs, cx| {
                            let r = vs
                                .manager_mut()
                                .map(|vm| vm.set_entry_host_key(id, fingerprint));
                            cx.notify();
                            r
                        });
                        if let Some(Err(e)) = result {
                            tracing::warn!(?e, "failed to persist trusted host key");
                        }
                    }
                }
                ServerSessionEvent::LeftSourcePickerRequested => {
                    this.show_left_source_picker(session_entity, window, cx);
                }
                ServerSessionEvent::PathsChanged { local, remote } => {
                    let server_id = session_entity.read(cx).server_id.clone();
                    if let Ok(id) = Uuid::parse_str(&server_id) {
                        let (local, remote) = (local.clone(), remote.clone());
                        let result = this.vault_state.update(cx, |vs, _| {
                            vs.manager_mut()
                                .map(|vm| vm.set_entry_last_dirs(id, local, remote))
                        });
                        if let Some(Err(e)) = result {
                            tracing::warn!(?e, "failed to persist last SFTP folders");
                        }
                        this.save_vault_soon(cx);
                    }
                }
                ServerSessionEvent::TransferFinished {
                    name,
                    kind,
                    route,
                    outcome,
                } => {
                    this.notify_transfer_finished(
                        session_entity,
                        name,
                        *kind,
                        route.as_deref(),
                        outcome,
                        window,
                        cx,
                    );
                }
                ServerSessionEvent::CloseRequested => {
                    if let Some(idx) = this.tabs.iter().position(|s| s == session_entity) {
                        this.close_tab(idx, cx);
                    }
                }
                ServerSessionEvent::ReconnectRequested => {
                    this.reconnect_session(session_entity, window, cx);
                }
                ServerSessionEvent::ConnectionStateChanged => {
                    this.sync_session_states(cx);
                    // A snippet queued for a host that could not be reached must
                    // not fire on some later connect the user no longer links to it.
                    let session = session_entity.read(cx);
                    let is_waiting_on_failed = session.has_connect_failure()
                        && this
                            .pending_snippet_for_server
                            .as_ref()
                            .is_some_and(|(server_id, _)| *server_id == session.server_id);
                    if is_waiting_on_failed {
                        this.pending_snippet_for_server = None;
                    }
                    cx.notify();
                }
                ServerSessionEvent::EditRequested { entry_id } => {
                    if let Ok(id) = Uuid::parse_str(entry_id) {
                        this.show_edit_form(id, window, cx);
                    }
                }
                ServerSessionEvent::PasswordAccepted { entry_id, password } => {
                    if let Ok(id) = Uuid::parse_str(entry_id) {
                        let result = this.vault_state.update(cx, |vs, cx| {
                            let r = vs
                                .manager_mut()
                                .map(|vm| vm.set_entry_password(id, password));
                            cx.notify();
                            r
                        });
                        if let Some(Err(e)) = result {
                            tracing::warn!(?e, "failed to save the new password");
                            this.set_status("Could not save the new password".to_string(), cx);
                        }
                    }
                }
            },
        )
        .detach();

        self.tabs.push(session);
        self.change_active_tab(self.tabs.len() - 1, cx);
    }

    /// Replace a dead session with a fresh connection to the same server.
    fn reconnect_session(
        &mut self,
        session: &Entity<ServerSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A fresh connection needs the entry from the vault. Closing the tab
        // now would lose it for good, so it waits for `resume_after_unlock`.
        if self.is_vault_locked(cx) {
            let session = session.downgrade();
            if !self.sessions_owed_reconnect.contains(&session) {
                self.sessions_owed_reconnect.push(session);
            }
            return;
        }

        // Drop the dead session, then re-open the server fresh. Closing first
        // is required: open_server's single-open guard would otherwise just
        // re-activate this dead tab instead of connecting.
        let server_id = session.read(cx).server_id.clone();
        if let Some(idx) = self.tabs.iter().position(|s| s == session) {
            self.close_tab(idx, cx);
        }
        if let Ok(id) = Uuid::parse_str(&server_id) {
            self.open_server_by_id(id, window, cx);
        }
    }

    /// Report a finished transfer to the user when they are looking at another
    /// app: bounce the dock icon and post a system notification. All of a
    /// session's notifications share one tag, so a burst of finishing transfers
    /// replaces itself instead of stacking up a pile of banners.
    fn notify_transfer_finished(
        &self,
        session: &Entity<ServerSession>,
        name: &str,
        kind: TransferKind,
        route: Option<&str>,
        outcome: &TransferOutcome,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        // Cancelling is something the user just did on purpose, and a visible
        // window already shows the result in the tasks popover.
        if window.is_window_active() || matches!(outcome, TransferOutcome::Canceled) {
            return;
        }

        let session = session.read(cx);
        let is_failed = matches!(outcome, TransferOutcome::Failed(_));
        let title = transfer_notification_title(kind, is_failed);
        let body = match outcome {
            TransferOutcome::Failed(error) => format!("{name}: {error}"),
            _ => format!(
                "{name} · {}",
                route.unwrap_or(session.conn_info.display_name())
            ),
        };
        let tag = format!("flint.transfer.{}", session.server_id);

        window.request_attention();
        cx.show_system_notification(SystemNotification {
            tag: tag.into(),
            title: title.into(),
            body: body.into(),
            actions: Vec::new(),
        });
    }

    /// The user just saved an edit to `id`. Sessions that could not connect
    /// because of that entry try again with the new settings.
    fn reconnect_failed_sessions(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let entry_id = id.to_string();
        let failed: Vec<_> = self
            .tabs
            .iter()
            .filter(|session| session.read(cx).has_connect_failure_for(&entry_id))
            .cloned()
            .collect();
        if failed.is_empty() {
            return;
        }
        // Reopening a session selects its new tab. An edit made from the
        // sidebar must not pull the user away from the tab they are on.
        let active_before = self
            .tabs
            .get(self.active_tab)
            .map(|session| session.read(cx).server_id.clone());
        for session in failed {
            self.reconnect_session(&session, window, cx);
        }
        let Some(active_before) = active_before else {
            self.show_all_servers(cx);
            return;
        };
        if let Some(idx) = self.tab_index_for_host(&active_before, cx) {
            self.change_active_tab(idx, cx);
        }
    }

    /// Let every open server session react to the machine waking up.
    fn retry_sessions_after_wake(&mut self, cx: &mut Context<Self>) {
        SyncService::global(cx).update(cx, |service, _| service.refresh_soon());
        for session in self.tabs.clone() {
            session.update(cx, |session, cx| session.retry_after_wake(cx));
        }
    }

    fn close_tab(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() {
            return;
        }
        let new_active = active_tab_after_close(self.active_tab, idx, self.tabs.len());
        let session = self.tabs.remove(idx);
        session.update(cx, |s, cx| s.cleanup_before_close(cx));
        self.change_active_tab(new_active, cx);
    }

    /// Ask what to do with the transfer the session's browser is holding back
    /// because its target name is taken. Does nothing when none is waiting.
    fn open_transfer_conflict_dialog(
        &mut self,
        session: Entity<ServerSession>,
        cx: &mut Context<Self>,
    ) {
        let Some(browser) = session.read(cx).browser.clone() else {
            return;
        };
        let Some(conflict) = browser.read(cx).pending_transfer_conflict().cloned() else {
            return;
        };
        let weak_browser = browser.downgrade();
        let wh = self.window_handle;
        cx.spawn(async move |_, cx| {
            cx.update_window(wh, |_, window, cx| {
                // Several conflicts announce themselves at once; the first
                // dialog asks about them one by one.
                if window.has_active_dialog(cx) {
                    return;
                }
                window.open_dialog(cx, move |dialog, _, cx| {
                    transfer_conflict_dialog(dialog, &conflict, &weak_browser, cx)
                });
            })
            .ok();
        })
        .detach();
    }

    fn open_conflict_dialog(&mut self, session: Entity<ServerSession>, cx: &mut Context<Self>) {
        let local = match session.read(cx).pending_conflict.clone() {
            Some(p) => p,
            None => return,
        };
        let weak_browser = session.read(cx).browser.as_ref().map(|b| b.downgrade());
        let weak_session = session.downgrade();
        let wh = self.window_handle;
        cx.spawn(async move |_, cx| {
            let Some(wb) = weak_browser else { return };
            cx.update_window(wh, |_, window, cx| {
                let filename = local
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("file")
                    .to_string();
                window.open_dialog(cx, move |dialog, _, _| {
                    let (lc, wb_c, ws_c) = (local.clone(), wb.clone(), weak_session.clone());
                    let (lr, wb_r, ws_r) = (local.clone(), wb.clone(), weak_session.clone());
                    let (lo, wb_o, ws_o) = (local.clone(), wb.clone(), weak_session.clone());
                    dialog_chrome(dialog).w(DIALOG_WIDE).child(
                        v_flex()
                            .p(px(12.))
                            .gap(px(8.))
                            .child(
                                DialogHeader::new().child(
                                    DialogTitle::new()
                                        .child(format!("{filename} changed on the host")),
                                ),
                            )
                            .child(div().text_size(px(12.5)).child(format!(
                                "\"{filename}\" changed on the host while you were editing it here.",
                            )))
                            .child(
                                DialogFooter::new().child(
                                    DialogAction::new().child(
                                        h_flex()
                                            .gap_2()
                                            .child(
                                                Button::new("conflict-cancel")
                                                    .ghost()
                                                    .child("Cancel")
                                                    .on_click(move |_, window, cx| {
                                                        wb_c.update(cx, |b, cx| {
                                                            b.resolve_conflict_cancel(&lc, cx);
                                                        })
                                                        .ok();
                                                        ws_c.update(cx, |s, _| {
                                                            s.pending_conflict = None;
                                                        })
                                                        .ok();
                                                        window.close_dialog(cx);
                                                    }),
                                            )
                                            .child(
                                                Button::new("conflict-reload")
                                                    .child("Reload from host")
                                                    .on_click(move |_, window, cx| {
                                                        wb_r.update(cx, |b, cx| {
                                                            b.resolve_conflict_reload(
                                                                lr.clone(),
                                                                cx,
                                                            );
                                                        })
                                                        .ok();
                                                        ws_r.update(cx, |s, _| {
                                                            s.pending_conflict = None;
                                                        })
                                                        .ok();
                                                        window.close_dialog(cx);
                                                    }),
                                            )
                                            .child(
                                                Button::new("conflict-overwrite")
                                                    .primary()
                                                    .child("Overwrite on host")
                                                    .on_click(move |_, window, cx| {
                                                        wb_o.update(cx, |b, cx| {
                                                            b.resolve_conflict_overwrite(
                                                                lo.clone(),
                                                                cx,
                                                            );
                                                        })
                                                        .ok();
                                                        ws_o.update(cx, |s, _| {
                                                            s.pending_conflict = None;
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
            })
            .ok();
        })
        .detach();
    }

    fn show_picker_for(&mut self, extension: String, local: PathBuf, cx: &mut Context<Self>) {
        let file_associations = self.file_associations.clone();
        let picker = cx.new(|cx| AppPicker::new(extension, local, file_associations, cx));
        cx.subscribe(&picker, |this, _, event: &AppPickerEvent, cx| {
            if let AppPickerEvent::OpenFailed(message) = event {
                this.set_status(message.clone(), cx);
            }
            let wh = this.window_handle;
            cx.spawn(async move |_, cx| {
                cx.update_window(wh, |_, window, cx| {
                    window.close_dialog(cx);
                })
                .ok();
            })
            .detach();
        })
        .detach();
        let wh = self.window_handle;
        cx.spawn(async move |_, cx| {
            cx.update_window(wh, |_, window, cx| {
                window.open_dialog(cx, move |dialog, _, _| {
                    dialog_chrome(dialog).w(DIALOG_WIDE).child(picker.clone())
                });
            })
            .ok();
        })
        .detach();
    }

    /// Return every left file pane that shows this host to this computer.
    fn drop_peer_for(&mut self, entry_id: &str, cx: &mut Context<Self>) {
        for session in &self.tabs {
            session.update(cx, |session, cx| session.drop_peer_for(entry_id, cx));
        }
    }

    fn show_remove_dialog(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let vault = self.vault_state.read(cx);
        let label = vault
            .manager()
            .and_then(|vm| vm.get_entry(id).ok())
            .map(|e| e.label.clone())
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| "this host".to_string());
        // Removing a bastion silently drops the jump-host link of every host
        // behind it, so name them before it happens.
        let behind = vault
            .manager()
            .map(|vm| vm.servers_using_jump_host(id))
            .unwrap_or_default();
        let jump_warning = (!behind.is_empty()).then(|| {
            format!(
                "Jump host for: {}. Those hosts will connect directly instead.",
                behind.join(", ")
            )
        });

        let weak = cx.weak_entity();

        window.open_dialog(&mut *cx, move |dialog, _window, cx| {
            let (weak2, label2) = (weak.clone(), label.clone());
            let jump_warning = jump_warning.clone();
            let warning_color = cx.theme().warning;
            dialog_chrome(dialog).w(DIALOG_NARROW).child(
                v_flex()
                    .p(px(12.))
                    .gap(px(8.))
                    .child(
                        DialogHeader::new()
                            .child(DialogTitle::new().child("Delete host")),
                    )
                    .child(
                        div()
                            .text_size(px(12.5))
                            .child(format!("Delete \"{}\"? This cannot be undone.", label2)),
                    )
                    .when_some(jump_warning, |el, warning| {
                        el.child(div().text_size(px(12.5)).text_color(warning_color).child(warning))
                    })
                    .child(
                        DialogFooter::new().child(
                            DialogAction::new().child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        Button::new("cancel-remove")
                                            .ghost()
                                            .child("Cancel")
                                            .on_click(|_, window, cx| {
                                                window.close_dialog(cx);
                                            }),
                                    )
                                    .child(
                                        Button::new("confirm-remove")
                                            .danger()
                                            .child("Delete")
                                            .on_click(move |_, window, cx| {
                                                weak2
                                                    .update(cx, |this, inner_cx| {
                                                        // Close tab first so cleanup runs before
                                                        // the entry is gone from the vault.
                                                        let server_id_str = id.to_string();
                                                        this.drop_peer_for(&server_id_str, inner_cx);
                                                        if let Some(idx) = this
                                                            .tab_index_for_host(&server_id_str, inner_cx)
                                                        {
                                                            this.close_tab(idx, inner_cx);
                                                        }
                                                        let result = this.vault_state.update(
                                                            inner_cx,
                                                            |vs, cx| {
                                                                let r = vs.manager_mut().map(|vm| {
                                                                    vm.delete_entry(id)
                                                                });
                                                                cx.notify();
                                                                r
                                                            },
                                                        );
                                                        if let Some(Err(e)) = result {
                                                            tracing::error!(
                                                                ?e,
                                                                "failed to delete entry"
                                                            );
                                                            this.set_status(
                                                                format!("Could not delete the host: {e}"),
                                                                inner_cx,
                                                            );
                                                        }
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

    /// Dialog for importing hosts from ~/.ssh/config and PuTTY sessions.
    fn show_import_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.new(|cx| ImportView::new(self.vault_state.clone(), window, cx));

        cx.subscribe_in(
            &view,
            window,
            |this, _view, event, window, cx| match event {
                ImportViewEvent::Done {
                    added,
                    snippets_added,
                    keys_failed,
                    jumps_skipped,
                    save_error,
                } => {
                    window.close_dialog(cx);
                    // A failed save still leaves the hosts in the running vault.
                    // Say so, or the list and the status line disagree.
                    let hosts = plural(*added, "host");
                    let mut msg = match save_error {
                        Some(error) => format!(
                            "Imported {hosts}, but the vault could not be saved ({error}). \
                         They will be gone on restart."
                        ),
                        None => format!("Imported {hosts}."),
                    };
                    if *snippets_added > 0 {
                        msg.push_str(&format!(" {} added.", plural(*snippets_added, "snippet")));
                    }
                    if *keys_failed > 0 {
                        msg.push_str(&format!(
                            " {} could not be read. Set the credentials by hand.",
                            plural(*keys_failed, "key file")
                        ));
                    }
                    if *jumps_skipped > 0 {
                        msg.push_str(&format!(
                            " {} could not be linked. Those hosts connect directly.",
                            plural(*jumps_skipped, "ProxyJump")
                        ));
                    }
                    this.set_status(msg, cx);
                }
                ImportViewEvent::Cancelled => {
                    window.close_dialog(cx);
                }
            },
        )
        .detach();

        let view_for_dialog = view.clone();
        window.open_dialog(&mut *cx, move |dialog, _, _| {
            dialog_chrome(dialog)
                .w(DIALOG_WIDE)
                .child(view_for_dialog.clone())
        });
    }

    /// Clone a server into a new entry (fresh id, "<label> copy"), so the user
    /// can tweak it without retyping. The ServersView observer refreshes the grid.
    fn duplicate_server(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let entry = self
            .vault_state
            .read(cx)
            .manager()
            .and_then(|vm| vm.get_entry(id).ok());
        let Some(entry) = entry else { return };

        let mut copy = entry.clone();
        copy.id = Uuid::new_v4();
        // A host without a name is shown by its address, so its copy is too.
        let name = if entry.label.is_empty() {
            &entry.host
        } else {
            &entry.label
        };
        copy.label = format!("{name} copy");
        // A clone is a distinct host record: it has never connected, so drop
        // session-derived state that belongs to the original.
        copy.last_opened_at = None;
        copy.host_key_fingerprint = None;
        copy.os = None;
        copy.last_local_dir = None;
        copy.last_remote_dir = None;
        // Forwards come along, but a forward id addresses one vault row's rule;
        // two hosts must never carry the same one.
        for forward in copy.forwards.iter_mut() {
            forward.id = Uuid::new_v4();
        }

        let result = self.vault_state.update(cx, |vs, cx| {
            let r = vs.manager_mut().map(|vm| vm.add_entry(copy));
            cx.notify();
            r
        });
        if let Some(Err(e)) = result {
            tracing::error!(?e, "failed to duplicate server");
        }
    }

    /// Hand a host's just-saved forwards to its open session, if it has one.
    fn push_forwards_to_session(
        &mut self,
        id: Uuid,
        forwards: Vec<ForwardRuntime>,
        cx: &mut Context<Self>,
    ) {
        let target = id.to_string();
        let sessions: Vec<Entity<ServerSession>> = self
            .tabs
            .iter()
            .filter(|session| session.read(cx).server_id == target)
            .cloned()
            .collect();
        for session in sessions {
            let forwards = forwards.clone();
            session.update(cx, |session, cx| session.update_forwards(forwards, cx));
        }
    }

    /// A shared host is about to be saved with a key its team does not hold
    /// yet. Saving shares the key with every member, so the first Save only
    /// says so in the form; a second Save with the same key goes through.
    /// Returns whether the save has to wait.
    fn asks_before_sharing_key(
        &mut self,
        entry: &ServerEntry,
        form: &Entity<ServerFormView>,
        cx: &mut Context<Self>,
    ) -> bool {
        let shared = self
            .vault_state
            .read(cx)
            .manager()
            .and_then(|vault| vault.key_shared_by_saving(entry));
        let Some((key_name, team)) = shared else {
            return false;
        };
        let crate::vault::AuthMethod::PrivateKey { key_id } = entry.auth else {
            return false;
        };
        if self.key_share_asked == Some((entry.id, key_id)) {
            return false;
        }
        self.key_share_asked = Some((entry.id, key_id));
        let message = format!(
            "This host is shared with {team}. Saving also shares the key \"{key_name}\" there: \
             every member who sees the host will be able to use it. Press Save again to confirm."
        );
        form.update(cx, |form, cx| form.set_warning(message, cx));
        true
    }

    /// The dialog with the newer build's notes and its download.
    fn show_update_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.new(UpdateView::new);
        cx.subscribe_in(
            &view,
            window,
            |this, _, event: &UpdateViewEvent, window, cx| match event {
                UpdateViewEvent::Closed => {
                    window.close_dialog(cx);
                    this.focus_handle.focus(window, cx);
                }
            },
        )
        .detach();
        window.open_dialog(&mut *cx, move |dialog, _, _| {
            dialog_chrome(dialog).w(DIALOG_WIDE).child(view.clone())
        });
    }

    /// The title bar's mark for a newer build: a soft accent chip with the
    /// version, or the percent while it downloads. `None` when there is
    /// nothing newer.
    fn render_update_mark(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let service = UpdateService::global(cx);
        let service = service.read(cx);
        let version = &service.release.as_ref()?.version;
        let text = match service.download {
            Download::Running { percent } => format!("Downloading {percent}%"),
            _ => format!("Update {version}"),
        };
        let theme = cx.theme();
        let (bg, fg, hover_bg, active_bg) = (
            crate::theme::accent_soft(),
            theme.primary,
            crate::theme::accent_soft_press(),
            crate::theme::accent_soft_press(),
        );

        Some(
            h_flex()
                .id("titlebar-update")
                .flex_none()
                .h(px(22.))
                .px(px(8.))
                .gap(px(4.))
                .items_center()
                .rounded(px(4.))
                .bg(bg)
                .text_size(px(11.))
                .text_color(fg)
                .cursor_pointer()
                .hover(move |s| s.bg(hover_bg))
                .active(move |s| s.bg(active_bg))
                .child(
                    Icon::empty()
                        .path("icons/download.svg")
                        .size(px(11.))
                        .text_color(fg),
                )
                .child(text)
                .on_click(cx.listener(|this, _, window, cx| this.show_update_dialog(window, cx))),
        )
    }

    /// The dialog for writing to the developer.
    fn show_feedback_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.new(|cx| FeedbackView::new(self.vault_state.clone(), window, cx));
        cx.subscribe_in(
            &view,
            window,
            |this, _, event: &FeedbackEvent, window, cx| match event {
                FeedbackEvent::Closed => {
                    window.close_dialog(cx);
                    this.focus_handle.focus(window, cx);
                }
            },
        )
        .detach();
        window.open_dialog(&mut *cx, move |dialog, _, _| {
            dialog_chrome(dialog).w(DIALOG_WIDE).child(view.clone())
        });
    }

    fn show_edit_form(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let entry = self
            .vault_state
            .read(cx)
            .manager()
            .and_then(|vm| vm.get_entry(id).ok());
        let Some(entry) = entry else { return };
        let mode = FormMode::Edit {
            id: entry.id,
            created_at: entry.created_at,
        };
        self.show_server_form_with_entry(mode, Some(entry), window, cx);
    }

    fn show_server_form(&mut self, mode: FormMode, window: &mut Window, cx: &mut Context<Self>) {
        self.show_server_form_with_entry(mode, None, window, cx);
    }

    fn show_server_form_with_entry(
        &mut self,
        mode: FormMode,
        prefill: Option<ServerEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let is_add = matches!(mode, FormMode::Add);
        let form =
            cx.new(|cx| ServerFormView::new(mode, prefill, self.vault_state.clone(), window, cx));

        cx.subscribe_in(&form, window, move |this, form, event, window, cx| {
            match event {
                FormEvent::Saved(entry) => {
                    let id = entry.id;
                    if !is_add && this.asks_before_sharing_key(entry, form, cx) {
                        return;
                    }
                    let result = this.vault_state.update(cx, |vs, cx| {
                        cx.notify();
                        vs.manager_mut().map(|vm| {
                            if is_add {
                                vm.add_entry(entry.clone())
                            } else {
                                // The form does not know the detected OS, notes,
                                // recency or trusted host key; they come from
                                // the stored entry. The trusted key is kept only
                                // for the same host:port: a different address is
                                // a different server and must be confirmed again.
                                let mut entry = entry.clone();
                                if let Ok(stored) = vm.get_entry(id) {
                                    entry.os = stored.os;
                                    // The form has no notes field, so it
                                    // always sends `None`.
                                    entry.notes = stored.notes;
                                    entry.last_opened_at = stored.last_opened_at;
                                    // The last shown SFTP folders come from the
                                    // session, not from the form.
                                    entry.last_local_dir = stored.last_local_dir;
                                    entry.last_remote_dir = stored.last_remote_dir;
                                    // Proxy routing is set in Settings, not in
                                    // the server form.
                                    entry.proxy = stored.proxy;
                                    if stored.host == entry.host && stored.port == entry.port {
                                        entry.host_key_fingerprint = stored.host_key_fingerprint;
                                    }
                                }
                                vm.update_entry(entry)
                            }
                        })
                    });
                    if let Some(Err(e)) = result {
                        // A rejected jump-host link lands here; the dialog stays
                        // open, so the reason has to be shown in it.
                        tracing::error!(?e, "failed to save vault entry");
                        let message = e.to_string();
                        form.update(cx, |form, cx| form.set_error(message, cx));
                        return;
                    }
                    // The form owns forwards, so a live session must not keep
                    // running rules the user just changed or deleted.
                    if !is_add {
                        let forwards = forward_runtimes(entry);
                        this.push_forwards_to_session(id, forwards, cx);
                    }
                    this.key_share_asked = None;
                    window.close_dialog(cx);
                    if is_add {
                        this.open_server_by_id(id, window, cx);
                    } else {
                        this.reconnect_failed_sessions(id, window, cx);
                    }
                }
                FormEvent::Cancelled => {
                    this.key_share_asked = None;
                    window.close_dialog(cx);
                }
            }
        })
        .detach();

        let form_for_dialog = form.clone();
        window.open_dialog(&mut *cx, move |dialog, _, _| {
            dialog_chrome(dialog)
                .w(DIALOG_FORM)
                .child(form_for_dialog.clone())
        });
        // A new host starts from its address; pasting `user@host:port` there
        // fills the user and the port too.
        if is_add {
            form.update(cx, |form, cx| form.focus_host(window, cx));
        }
    }

    /// Settings, the palette or a dialog sits on top of the main view.
    fn is_overlay_open(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.settings_view.is_some()
            || self.command_palette.is_some()
            || window.has_active_dialog(cx)
    }

    fn shortcut_close_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_overlay_open(window, cx) {
            return;
        }
        let idx = self.active_tab;
        self.close_tab(idx, cx);
    }

    fn shortcut_toggle_files_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_overlay_open(window, cx) {
            return;
        }
        let Some(session) = self.tabs.get(self.active_tab) else {
            return;
        };
        session.update(cx, |session, cx| session.toggle_files_focus(window, cx));
    }

    fn shortcut_new_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_overlay_open(window, cx) {
            return;
        }
        // Off macOS the chord is Ctrl+Shift+N, which is New Folder in a file
        // pane. Passing it on lets the pane's own key handler have it.
        if !cfg!(target_os = "macos") && self.is_file_pane_focused(window, cx) {
            cx.propagate();
            return;
        }
        self.open_extra_terminal(cx);
    }

    fn is_file_pane_focused(&self, window: &Window, cx: &App) -> bool {
        let Some(session) = self.tabs.get(self.active_tab) else {
            return false;
        };
        session
            .read(cx)
            .browser
            .as_ref()
            .is_some_and(|browser| browser.read(cx).has_focus(window, cx))
    }

    fn shortcut_toggle_sftp_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_overlay_open(window, cx) {
            return;
        }
        self.toggle_sftp_pane(cx);
    }

    /// One more shell in the active session, over the connection it has.
    fn open_extra_terminal(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.tabs.get(self.active_tab) else {
            return;
        };
        session.update(cx, |session, cx| session.open_extra_terminal(cx));
    }

    fn toggle_sftp_pane(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.tabs.get(self.active_tab) else {
            return;
        };
        session.update(cx, |session, cx| session.toggle_sftp_pane(cx));
    }

    fn shortcut_next_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_overlay_open(window, cx) {
            return;
        }
        let Some(next) = next_tab_index(self.active_tab, self.tabs.len()) else {
            return;
        };
        self.set_active_tab(next, window, cx);
    }

    fn shortcut_prev_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_overlay_open(window, cx) {
            return;
        }
        let Some(prev) = prev_tab_index(self.active_tab, self.tabs.len()) else {
            return;
        };
        self.set_active_tab(prev, window, cx);
    }

    fn shortcut_activate_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_overlay_open(window, cx) {
            return;
        }
        if idx < self.tabs.len() {
            self.set_active_tab(idx, window, cx);
        }
    }

    fn render_title_bar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(target_os = "macos")]
        let left_pad = if window.is_fullscreen() {
            px(8.)
        } else {
            px(80.)
        };
        #[cfg(not(target_os = "macos"))]
        let left_pad = px(12.);

        let theme = cx.theme();
        let bg_elevated = theme.title_bar;
        let text_muted = theme.muted_foreground;
        let text_placeholder = crate::theme::text_placeholder();
        let field = crate::theme::field();

        // Placed at the centre of the bar, whatever sits to its left and right.
        let palette_trigger = h_flex()
            .id("titlebar-palette-trigger")
            .absolute()
            .left(gpui::relative(0.5))
            .top(gpui::relative(0.5))
            .h(px(24.))
            .w(px(360.))
            .px(px(10.))
            .gap(px(8.))
            .items_center()
            .bg(field)
            .rounded(px(7.))
            .text_size(px(12.5))
            .text_color(text_placeholder)
            .cursor_pointer()
            .hover(|s| s.bg(crate::theme::element()).text_color(text_muted))
            .active(|s| s.bg(crate::theme::element_active()).text_color(text_muted))
            .child(
                Icon::new(IconName::Search)
                    .size(px(13.))
                    .text_color(text_placeholder),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child("Go to host, snippet or command"),
            )
            // Lit up like the other shortcut hints while the modifier is held.
            .child(if shortcut_hints::is_shown(cx) {
                shortcut_hints::badge(PALETTE_SHORTCUT).into_any_element()
            } else {
                div()
                    .flex()
                    .items_center()
                    .px(px(5.))
                    .py(px(1.))
                    .rounded(px(4.))
                    .bg(field)
                    .text_size(px(10.5))
                    .text_color(text_muted)
                    .child(PALETTE_SHORTCUT)
                    .into_any_element()
            })
            .on_click(cx.listener(|this, _, window, cx| this.toggle_command_palette(window, cx)));

        // Pull palette trigger up & left by half its size via negative margins.
        // `.occlude()` makes its hitbox opaque so clicks land on it instead of
        // falling through to the draggable title-bar region beneath it.
        let palette_trigger = palette_trigger.ml(px(-180.)).mt(px(-12.)).occlude();

        let update_mark = self.render_update_mark(cx);

        // Fills the space between "flint" and the right edge. The palette
        // floats over its centre but `.occlude()`s itself, so the gaps on
        // either side of the palette remain grab handles.
        let drag_region = window_chrome::drag_region("titlebar-drag");

        #[cfg(not(target_os = "macos"))]
        let window_controls = window_chrome::window_controls(window);

        // The title bar and the sidebar are one surface, so no line under it.
        // macOS shows the traffic lights there; elsewhere the name sits there.
        let bar = h_flex()
            .relative()
            .w_full()
            .h(window_chrome::TITLE_BAR_HEIGHT)
            .pl(left_pad)
            .pr(px(6.))
            .gap(px(8.))
            .bg(bg_elevated)
            .items_center()
            .when(cfg!(not(target_os = "macos")), |bar| {
                bar.child(
                    div()
                        .text_size(px(12.))
                        .text_color(text_muted)
                        .child("Flint"),
                )
            })
            .child(drag_region)
            // Right of the drag region, which shrinks to make room: the
            // search field is placed on its own and does not move.
            .children(update_mark);

        // Window controls sit at the far right (Windows/Linux).
        #[cfg(not(target_os = "macos"))]
        let bar = bar.child(window_controls);

        // Palette is added last so it paints on top of the drag region.
        bar.child(palette_trigger)
    }

    fn render_active_tab(&self) -> AnyElement {
        match self.tabs.get(self.active_tab) {
            Some(session) => session.clone().into_any_element(),
            None => self
                .servers_view
                .clone()
                .cached(StyleRefinement::default().size_full())
                .into_any_element(),
        }
    }
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.focus_front_tab_if_changed(window, cx);
        let chrome = crate::theme::chrome();
        let panel = cx.theme().background;
        let border = crate::theme::border();
        let muted_fg = cx.theme().muted_foreground;

        let title_bar = self.render_title_bar(window, cx);
        let active_body = self.render_active_tab();

        // A transient message wins; otherwise the front session's state. With
        // neither, the strip is not drawn at all.
        let status_left: Option<AnyElement> = if !self.status.is_empty() {
            Some(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.))
                    .text_color(muted_fg)
                    .child(self.status.clone())
                    .into_any_element(),
            )
        } else {
            self.tabs.get(self.active_tab).map(|session| {
                let s = session.read(cx);
                if s.browser.is_some() {
                    let host_text = format!(
                        "{}@{}:{}",
                        s.conn_info.username, s.conn_info.host, s.conn_info.port
                    );
                    h_flex()
                        .gap(px(12.))
                        .items_center()
                        .child(
                            h_flex()
                                .gap(px(6.))
                                .items_center()
                                .text_size(px(12.))
                                .text_color(cx.theme().success)
                                .child(
                                    div()
                                        .size(px(6.))
                                        .rounded_full()
                                        .flex_none()
                                        .bg(cx.theme().success),
                                )
                                .child("Connected"),
                        )
                        .child(
                            div()
                                .font_family(crate::theme::MONO_FONT)
                                .text_size(px(11.5))
                                .text_color(muted_fg)
                                .child(host_text),
                        )
                        .into_any_element()
                } else if s.disconnected.is_some() || s.has_connect_failure() {
                    let label = if s.has_connect_failure() {
                        "connection failed"
                    } else {
                        "disconnected"
                    };
                    h_flex()
                        .gap(px(6.))
                        .items_center()
                        .child(
                            div()
                                .size(px(6.))
                                .rounded_full()
                                .flex_none()
                                .bg(cx.theme().danger),
                        )
                        .child(div().text_size(px(12.)).text_color(muted_fg).child(label))
                        .into_any_element()
                } else {
                    h_flex()
                        .gap(px(6.))
                        .items_center()
                        .child(
                            div()
                                .size(px(6.))
                                .rounded_full()
                                .flex_none()
                                .bg(cx.theme().warning),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(muted_fg)
                                .child("Connecting…"),
                        )
                        .into_any_element()
                }
            })
        };

        // The working area is a rounded card on the window surface. The
        // status strip belongs to it and only shows when it has something.
        let status_strip = status_left.map(|left| {
            h_flex()
                .flex_none()
                .h(px(28.))
                .px(px(14.))
                .border_t_1()
                .border_color(border)
                .items_center()
                .child(div().flex_1().min_w_0().overflow_hidden().child(left))
        });

        v_flex()
            .size_full()
            .bg(chrome)
            .track_focus(&self.focus_handle)
            .on_modifiers_changed(cx.listener(Self::on_modifiers_changed))
            .on_action(cx.listener(|this, _: &ToggleSettings, window, cx| {
                this.toggle_settings(window, cx);
            }))
            .on_action(cx.listener(|this, _: &NewConnection, window, cx| {
                this.show_server_form(FormMode::Add, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleCommandPalette, window, cx| {
                this.toggle_command_palette(window, cx);
            }))
            .on_action(cx.listener(|this, _: &NewTerminal, window, cx| {
                this.shortcut_new_terminal(window, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleSftpPane, window, cx| {
                this.shortcut_toggle_sftp_pane(window, cx)
            }))
            .on_action(cx.listener(|this, _: &LockVault, window, cx| {
                this.lock_vault(window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &CloseTab, window, cx| this.shortcut_close_tab(window, cx)),
            )
            .on_action(cx.listener(|this, _: &ToggleFilesFocus, window, cx| {
                this.shortcut_toggle_files_focus(window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &NextTab, window, cx| this.shortcut_next_tab(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &PrevTab, window, cx| this.shortcut_prev_tab(window, cx)),
            )
            .on_action(cx.listener(|this, _: &ActivateTab1, window, cx| {
                this.shortcut_activate_tab(0, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ActivateTab2, window, cx| {
                this.shortcut_activate_tab(1, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ActivateTab3, window, cx| {
                this.shortcut_activate_tab(2, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ActivateTab4, window, cx| {
                this.shortcut_activate_tab(3, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ActivateTab5, window, cx| {
                this.shortcut_activate_tab(4, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ActivateTab6, window, cx| {
                this.shortcut_activate_tab(5, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ActivateTab7, window, cx| {
                this.shortcut_activate_tab(6, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ActivateTab8, window, cx| {
                this.shortcut_activate_tab(7, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ActivateTab9, window, cx| {
                this.shortcut_activate_tab(8, window, cx)
            }))
            .child(title_bar)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .overflow_hidden()
                    // Cached like the hosts view: a terminal frame or a
                    // transfer tick redraws this view, and the sidebar has
                    // not changed. The style repeats the sidebar's own size.
                    .child(
                        self.sidebar.clone().cached(
                            StyleRefinement::default()
                                .w(px(SIDEBAR_WIDTH))
                                .flex_none()
                                .h_full(),
                        ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .mr(px(8.))
                            .mb(px(8.))
                            .rounded(px(crate::ui::PANEL_RADIUS))
                            .border_1()
                            .border_color(border)
                            .bg(panel)
                            .overflow_hidden()
                            .child(
                                div()
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_hidden()
                                    .child(active_body),
                            )
                            .children(status_strip),
                    ),
            )
            .when_some(self.settings_view.as_ref(), |el, sv| el.child(sv.clone()))
            .when_some(self.command_palette.as_ref(), |el, sl| el.child(sl.clone()))
    }
}

/// Type a script into a shell and press enter for it.
fn send_script(shell: crate::shell::ShellHandle, content: String, cx: &mut Context<AppView>) {
    let mut bytes = content.into_bytes();
    bytes.push(b'\r');
    cx.spawn(async move |_, _| {
        shell.send_input(bytes).await;
    })
    .detach();
}

/// A new vault entry for a quick-connected host: named after the host, signed
/// in through the SSH agent, everything else left at its default.
fn quick_connect_entry(
    username: String,
    host: String,
    port: u16,
    jump_host: Option<Uuid>,
) -> ServerEntry {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0);
    ServerEntry {
        id: Uuid::new_v4(),
        label: host.clone(),
        host,
        port,
        username,
        auth: AuthMethod::Agent,
        notes: None,
        os: None,
        last_opened_at: None,
        host_key_fingerprint: None,
        last_remote_dir: None,
        last_local_dir: None,
        group: None,
        tags: Vec::new(),
        proxy: crate::vault::ProxyChoice::Default,
        jump_host,
        forwards: Vec::new(),
        startup_snippet: None,
        created_at: now,
        updated_at: now,
    }
}

/// What a session shows and logs about the host it is connected to.
fn connection_info(entry: &ServerEntry) -> Connection {
    Connection {
        id: entry.id.to_string(),
        name: entry.label.clone(),
        host: entry.host.clone(),
        port: entry.port,
        username: entry.username.clone(),
    }
}

/// A host's saved forwards as the runtime list a session tracks. Each starts
/// out stopped: nothing is bound until the connection is up.
fn forward_runtimes(entry: &ServerEntry) -> Vec<ForwardRuntime> {
    entry
        .forwards
        .iter()
        .map(|forward| ForwardRuntime {
            spec: forward.spec(),
            name: forward.name.clone(),
            auto_start: forward.auto_start,
            state: ForwardState::Stopped { reason: None },
        })
        .collect()
}

fn transfer_notification_title(kind: TransferKind, is_failed: bool) -> &'static str {
    match (kind, is_failed) {
        (TransferKind::Upload, true) => "Upload failed",
        (TransferKind::Download, true) => "Download failed",
        (TransferKind::Copy, true) => "Copy failed",
        (TransferKind::Upload, false) => "Upload finished",
        (TransferKind::Download, false) => "Download finished",
        (TransferKind::Copy, false) => "Copy finished",
    }
}

/// The active index after the tab at `closed` is removed from `tab_count`
/// tabs. The home view sits past the last tab and stays in front when it was.
fn active_tab_after_close(active: usize, closed: usize, tab_count: usize) -> usize {
    let remaining = tab_count - 1;
    if active >= tab_count {
        return remaining;
    }
    if active > closed {
        return active - 1;
    }
    // The last tab was in front and was closed: the one before it takes over.
    active.min(remaining.saturating_sub(1))
}

/// Tab to show after "next tab". The home view sits past the last tab
/// (`active >= tab_count`); from there the cycle enters at the first tab.
fn next_tab_index(active: usize, tab_count: usize) -> Option<usize> {
    if tab_count == 0 {
        return None;
    }
    if active >= tab_count {
        return Some(0);
    }
    Some((active + 1) % tab_count)
}

/// Tab to show after "previous tab". From the home view the cycle enters at
/// the last tab.
fn prev_tab_index(active: usize, tab_count: usize) -> Option<usize> {
    if tab_count == 0 {
        return None;
    }
    if active == 0 || active >= tab_count {
        return Some(tab_count - 1);
    }
    Some(active - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_notification_title_names_the_kind_and_the_end() {
        assert_eq!(
            transfer_notification_title(TransferKind::Copy, false),
            "Copy finished"
        );
        assert_eq!(
            transfer_notification_title(TransferKind::Copy, true),
            "Copy failed"
        );
        assert_eq!(
            transfer_notification_title(TransferKind::Upload, true),
            "Upload failed"
        );
        assert_eq!(
            transfer_notification_title(TransferKind::Download, false),
            "Download finished"
        );
    }

    #[test]
    fn closing_a_tab_from_the_home_view_stays_on_the_home_view() {
        assert_eq!(active_tab_after_close(3, 0, 3), 2);
        assert_eq!(active_tab_after_close(1, 0, 1), 0);
    }

    #[test]
    fn closing_a_tab_keeps_the_same_session_in_front() {
        assert_eq!(active_tab_after_close(2, 0, 3), 1);
        assert_eq!(active_tab_after_close(0, 2, 3), 0);
    }

    #[test]
    fn closing_the_tab_in_front_shows_its_neighbour() {
        assert_eq!(active_tab_after_close(1, 1, 3), 1);
        assert_eq!(active_tab_after_close(2, 2, 3), 1);
        assert_eq!(active_tab_after_close(0, 0, 1), 0);
    }

    #[test]
    fn tab_cycling_does_nothing_without_tabs() {
        assert_eq!(next_tab_index(0, 0), None);
        assert_eq!(prev_tab_index(0, 0), None);
    }

    #[test]
    fn tab_cycling_wraps_around() {
        assert_eq!(next_tab_index(0, 3), Some(1));
        assert_eq!(next_tab_index(2, 3), Some(0));
        assert_eq!(prev_tab_index(2, 3), Some(1));
        assert_eq!(prev_tab_index(0, 3), Some(2));
        assert_eq!(next_tab_index(0, 1), Some(0));
        assert_eq!(prev_tab_index(0, 1), Some(0));
    }

    #[test]
    fn tab_cycling_from_home_enters_at_the_ends() {
        assert_eq!(next_tab_index(3, 3), Some(0));
        assert_eq!(prev_tab_index(3, 3), Some(2));
    }
}
