use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::connection::{Connection, ForwardSpec};
use crate::file_associations::SharedFileAssociations;
use crate::preferences::{Preferences, TerminalPosition};
use crate::shell::ShellHandle;
use crate::ssh::{
    AuthCredential, AuthError, AuthPromptField, CancelFlag, ForwardState, SshConnection,
    SshConnectionEvent, SshRoute, TransferOutcome,
};
use crate::theme as flint_theme;
use crate::ui::peer_link::{Link, PeerLink};
use crate::ui::sftp_browser::{
    BrowserEvent, PreservedFiles, RestorePaths, SftpBrowser, TransferKind,
};
use crate::ui::terminal_view::TerminalView;
use crate::ui::transfer_rate::TransferRate;
use gpui::prelude::*;
use gpui::{
    div, px, AnyElement, App, AsyncApp, ClipboardItem, Context, Entity, EventEmitter, FocusHandle,
    Focusable as _, MouseButton, Pixels, StyleRefinement, Task, WeakEntity, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputEvent, InputState},
    resizable::{h_resizable, resizable_panel, v_resizable, ResizableState},
    spinner::Spinner,
    v_flex, ActiveTheme as _, Sizable as _, StyledExt as _, WindowExt as _,
};
use zeroize::Zeroizing;

pub struct DisconnectedState {
    pub host: String,
    pub preserved: PreservedFiles,
    /// Why the last attempt failed, shown under the title.
    pub reason: String,
}

/// One tracked ad-hoc SFTP transfer (drag/drop or context-menu), listed in the
/// tasks popover.
pub struct TransferTask {
    pub id: u64,
    pub name: String,
    pub kind: TransferKind,
    /// The servers involved when the session's own does not say it all: both
    /// ends of a copy, or the left server of an upload or a download.
    pub route: Option<String>,
    /// False for a transfer that only uses the left server. It goes on when
    /// the session's own connection drops.
    uses_session: bool,
    pub done: u64,
    /// 0 until the worker stats the file (renders as indeterminate).
    pub total: u64,
    /// Waits for a free transfer slot; no bytes move yet.
    pub is_queued: bool,
    pub started: Instant,
    /// Recent progress ticks, for the live speed and the time left.
    pub rate: TransferRate,
    /// `Some` once finished; the task then lives in the history list.
    pub outcome: Option<TransferOutcome>,
    pub finished_at: Option<Instant>,
    /// Row expanded in the tasks popover to show transfer detail.
    pub expanded: bool,
    cancel: CancelFlag,
}

impl TransferTask {
    pub fn pct(&self) -> u32 {
        if self.total > 0 {
            ((self.done as f64 / self.total as f64) * 100.0).min(100.0) as u32
        } else {
            0
        }
    }

    pub fn request_cancel(&self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// One of the host's saved forwards plus how it is doing right now. The
/// session owns these, not the SSH thread: forwards die with the connection and
/// the auto-start ones are re-armed on every reconnect.
#[derive(Clone)]
pub struct ForwardRuntime {
    pub spec: ForwardSpec,
    pub name: String,
    pub auto_start: bool,
    pub state: ForwardState,
}

impl ForwardRuntime {
    pub fn is_bound(&self) -> bool {
        matches!(self.state, ForwardState::Bound { .. })
    }

    /// The reason it stopped, when it stopped by itself rather than on request.
    pub fn failure(&self) -> Option<&str> {
        match &self.state {
            ForwardState::Stopped { reason } => reason.as_deref(),
            ForwardState::Bound { .. } => None,
        }
    }
}

pub enum ServerSessionEvent {
    StatusChanged(String),
    /// The shell is ready, on a first connect or a reconnect. `elapsed` is the
    /// time from the start of the connect, set only when it is worth showing
    /// (see [`shown_connect_time`]).
    Connected {
        elapsed: Option<Duration>,
    },
    ConflictDetected,
    /// The browser holds a transfer whose target name is taken.
    TransferConflict,
    PickerRequested {
        extension: String,
        local: PathBuf,
    },
    CloseRequested,
    /// User asked to re-establish a permanently-dropped connection.
    ReconnectRequested,
    /// The session gave up connecting, or started trying again. AppView
    /// refreshes the host indicators from the session's state.
    ConnectionStateChanged,
    /// User asked to edit a host from the "could not connect" card. On a
    /// jumped session that can be the bastion that failed, not the tab's host.
    EditRequested {
        entry_id: String,
    },
    /// A password typed on the "could not connect" card got the session in and
    /// the user asked to keep it. Store it on `entry_id`'s entry.
    PasswordAccepted {
        entry_id: String,
        password: Zeroizing<String>,
    },
    /// Remote OS auto-detected after connect (e.g. "Ubuntu 22.04").
    OsDetected(String),
    /// The user (or `~/.ssh/known_hosts`) trusted this host key fingerprint.
    /// Persist it on `entry_id`'s entry so future connects verify silently.
    /// On a jumped host that is often a bastion, never the target.
    HostKeyAccepted {
        entry_id: String,
        fingerprint: String,
    },
    /// The user wants to choose what the left file pane shows: this computer
    /// or another saved server.
    LeftSourcePickerRequested,
    /// The SFTP browser navigated to new folders. Persist them on the entry so
    /// a restart or reconnect reopens them.
    PathsChanged {
        local: String,
        remote: String,
    },
    /// A transfer ended. AppView turns this into a system notification when the
    /// user is looking at another app.
    TransferFinished {
        name: String,
        kind: TransferKind,
        route: Option<String>,
        outcome: TransferOutcome,
    },
}

/// The machine an event came from, resolved against the live route. Prompts
/// and failures must name it rather than the tab's host: on a jumped session
/// the machine that spoke is often a bastion.
pub(crate) struct HopIdentity {
    /// Display label, falling back to the address when the label is empty.
    pub(crate) name: String,
    pub(crate) host: String,
    pub(crate) username: String,
    pub(crate) is_jump: bool,
}

/// A host-key decision waiting on the user. The SSH handshake is parked until
/// `reply` is answered (see [`crate::ssh::HostKeyPrompt`]).
pub struct HostKeyPromptState {
    /// Vault entry of the hop presenting the key, not always the target.
    pub entry_id: String,
    pub key_type: String,
    pub fingerprint: String,
    /// `Some` = the key CHANGED relative to what was trusted before (possible
    /// man-in-the-middle); `None` = first contact with this host.
    pub stored_fingerprint: Option<String>,
    reply: tokio::sync::oneshot::Sender<bool>,
}

impl HostKeyPromptState {
    pub(crate) fn new(prompt: crate::ssh::HostKeyPrompt) -> Self {
        Self {
            entry_id: prompt.entry_id,
            key_type: prompt.key_type,
            fingerprint: prompt.fingerprint,
            stored_fingerprint: prompt.stored_fingerprint,
            reply: prompt.reply,
        }
    }
}

/// A keyboard-interactive challenge (2FA code, PAM prompt) waiting on the
/// user. Authentication is parked until `reply` is answered (see
/// [`crate::ssh::AuthPrompt`]).
pub struct AuthPromptState {
    /// Vault entry of the hop asking, not always the target.
    pub entry_id: String,
    pub name: String,
    pub instructions: String,
    pub fields: Vec<AuthPromptField>,
    /// One input per field. Created lazily on the first render that shows the
    /// prompt: the connection event pump has no `Window` to create them with.
    inputs: Vec<Entity<InputState>>,
    reply: tokio::sync::oneshot::Sender<Option<Vec<Zeroizing<String>>>>,
}

impl AuthPromptState {
    pub(crate) fn new(prompt: crate::ssh::AuthPrompt) -> Self {
        Self {
            entry_id: prompt.entry_id,
            name: prompt.name,
            instructions: prompt.instructions,
            fields: prompt.fields,
            inputs: Vec::new(),
            reply: prompt.reply,
        }
    }
}

/// Best-effort remote OS probe: parse `/etc/os-release`, fall back to `uname`.
/// Returns a short label like "Ubuntu 22.04" or `None` on failure/empty.
async fn detect_os(conn: &SshConnection) -> Option<String> {
    let cmd = ". /etc/os-release 2>/dev/null && \
        printf '%s %s' \"$NAME\" \"$VERSION_ID\" || uname -sr"
        .to_string();
    let handle = conn.open_exec(cmd).await.ok()?;
    let rx = handle.subscribe();
    let mut out: Vec<u8> = Vec::new();
    // Drain until the exec channel closes, which happens when the command exits.
    while let Ok(chunk) = rx.recv().await {
        out.extend_from_slice(&chunk);
    }
    let label = String::from_utf8_lossy(&out).trim().to_string();
    if label.is_empty() {
        None
    } else {
        Some(label)
    }
}

/// Fetch the server's shell history (bash/zsh/fish) over an exec channel to back
/// the terminal's command-suggestion popup. Best-effort: returns empty on any
/// failure. See [`crate::ui::terminal_suggestions::parse_history`].
async fn fetch_shell_history(conn: &SshConnection) -> Vec<String> {
    // Read only the active shell's history. Every shell's file together would
    // bring in stale commands (an old ~/.bash_history under a zsh user) and mix
    // up recency, since each file is newest-last on its own.
    // Only the end of the file is sent back: a history can be many megabytes, and
    // a fish entry takes several lines, so the cut is well above the 5000
    // commands that are kept.
    let cmd = "case \"$SHELL\" in \
        *zsh) cat \"$HOME/.zsh_history\";; \
        *bash) cat \"$HOME/.bash_history\";; \
        *fish) cat \"$HOME/.local/share/fish/fish_history\";; \
        *) cat \"$HOME/.bash_history\" \"$HOME/.zsh_history\" \
            \"$HOME/.local/share/fish/fish_history\";; \
        esac 2>/dev/null | tail -n 20000"
        .to_string();
    let Ok(handle) = conn.open_exec(cmd).await else {
        return Vec::new();
    };
    let rx = handle.subscribe();
    let mut out: Vec<u8> = Vec::new();
    while let Ok(chunk) = rx.recv().await {
        out.extend_from_slice(&chunk);
    }
    crate::ui::terminal_suggestions::parse_history(&String::from_utf8_lossy(&out))
}

/// The result of `work`, or `None` when `limit` ends first.
async fn finish_within<T>(
    work: impl std::future::Future<Output = T>,
    limit: impl std::future::Future,
) -> Option<T> {
    match futures::future::select(Box::pin(work), Box::pin(limit)).await {
        futures::future::Either::Left((value, _)) => Some(value),
        futures::future::Either::Right(_) => None,
    }
}

/// Open the shells for the extra terminal tabs after a reconnect. A server that
/// refuses one (its `MaxSessions` limit) leaves the session with fewer tabs.
async fn open_extra_shells(conn: &SshConnection, count: usize) -> Vec<ShellHandle> {
    let mut shells = Vec::with_capacity(count);
    for _ in 0..count {
        match conn.open_shell(24, 80).await {
            Ok(shell) => shells.push(shell),
            Err(error) => {
                tracing::warn!(
                    target: "flint::ssh",
                    %error,
                    reopened = shells.len(),
                    wanted = count,
                    "could not reopen every extra terminal",
                );
                break;
            }
        }
    }
    shells
}

/// Why a connection ended, kept so the tab can say so.
pub(crate) struct ConnectFailure {
    /// Vault entry of the hop that failed, not always the target.
    pub(crate) entry_id: String,
    pub(crate) message: String,
    /// The hop turned down its stored password, so the card asks for a new one.
    pub(crate) is_password_rejected: bool,
    /// Trying the same route again can work (network blip, server still
    /// booting). A rejected password or key is not transient.
    pub(crate) is_transient: bool,
}

/// How the active connection ended, telling the reconnect loop what to do next.
enum SessionOutcome {
    /// The connection is over. `None` is a plain drop with nothing to report.
    Ended(Option<ConnectFailure>),
    /// The session entity is gone. Stop.
    Gone,
}

pub(crate) const CONNECTION_CLOSED: &str = "The host closed the connection.";

fn capitalize_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The text for a failed connect. It names the machine that refused: on a
/// jumped host the failure is often the bastion's, not the target's.
pub(crate) fn failure_message(error: &AuthError, hop: &HopIdentity) -> String {
    let reason = match error {
        // Shown under a title that already says the connection failed.
        AuthError::ConnectionFailed(detail) => capitalize_first(detail),
        AuthError::WrongPassword => {
            format!("Password rejected for {}@{}", hop.username, hop.host)
        }
        other => capitalize_first(&other.to_string()),
    };
    if hop.is_jump {
        format!("Jump host {}: {reason}", hop.name)
    } else {
        reason
    }
}

/// The connect time to show, or `None` when the figure would mislead: a
/// reconnect is not what the user asked for, and a host-key or 2FA prompt
/// adds the time the user took to answer it.
fn shown_connect_time(
    elapsed: Duration,
    is_reconnect: bool,
    has_waited_on_user: bool,
) -> Option<Duration> {
    (!is_reconnect && !has_waited_on_user).then_some(elapsed)
}

/// Status line text for a finished connect, e.g. "Connected to prod-db in 380 ms".
pub(crate) fn connect_time_message(host: &str, elapsed: Duration) -> String {
    if elapsed < Duration::from_secs(10) {
        format!("Connected to {host} in {} ms", elapsed.as_millis())
    } else {
        format!("Connected to {host} in {:.1} s", elapsed.as_secs_f64())
    }
}

/// The line on the "connection lost" card about edited files that were not
/// uploaded.
fn preserved_files_note(count: usize, cache_path: &str) -> String {
    let files = if count == 1 {
        "1 file".to_string()
    } else {
        format!("{count} files")
    };
    format!("{files} with unsaved changes kept in {cache_path}")
}

/// Drive one SSH connection to completion: wait for it to authenticate, build the
/// SFTP browser + terminal, then keep listening until it disconnects. Returns how
/// it ended so the caller can decide whether to reconnect.
async fn drive_connection(
    conn: SshConnection,
    connect_started: Instant,
    this: &WeakEntity<ServerSession>,
    cx: &mut AsyncApp,
) -> SessionOutcome {
    let event_rx = conn.subscribe_events();
    let mut has_waited_on_user = false;
    while let Ok(event) = event_rx.recv().await {
        match event {
            SshConnectionEvent::Connected => {
                let sftp_result = conn.open_sftp().await;
                let shell_result = match sftp_result {
                    Ok((sftp_primary, home_dir)) => {
                        let sftp_prefetch = match conn.open_sftp().await {
                            Ok((h, _)) => Some(h),
                            Err(e) => {
                                tracing::warn!(
                                    target: "flint::sftp",
                                    "prefetch channel open failed, prefetch disabled: {}",
                                    e
                                );
                                None
                            }
                        };
                        match conn.open_shell(24, 80).await {
                            Ok(shell) => {
                                // A reconnect brings back as many terminal
                                // tabs as the session had, each a fresh shell.
                                let extra_count = this
                                    .update(cx, |session, _| {
                                        session.terminals.len().saturating_sub(1)
                                    })
                                    .unwrap_or(0);
                                let mut shells = vec![shell];
                                shells.extend(open_extra_shells(&conn, extra_count).await);
                                Ok((sftp_primary, sftp_prefetch, home_dir, shells))
                            }
                            Err(e) => Err(format!("Could not open a shell: {e}")),
                        }
                    }
                    Err(e) => Err(format!("Could not open SFTP: {e}")),
                };

                match shell_result {
                    Ok((sftp_primary, sftp_prefetch, home_dir, shells)) => {
                        let elapsed = connect_started.elapsed();
                        let built = this.update(cx, |session, cx| {
                            let is_reconnect = session.browser.is_some();
                            tracing::info!(
                                target: "flint::ssh",
                                host = %session.conn_info.display_name(),
                                elapsed_ms = elapsed.as_millis() as u64,
                                is_reconnect,
                                has_waited_on_user,
                                "session ready"
                            );
                            let conn_info = session.conn_info.clone();
                            let file_associations = session.file_associations.clone();
                            // On a reconnect, carry over the folders the old
                            // browser was showing. On the first connect, use the
                            // folders saved for this host, so a restart opens
                            // where the user left off.
                            let restore: Option<RestorePaths> = session
                                .browser
                                .as_ref()
                                .map(|b| b.read(cx).current_paths(cx))
                                .or_else(|| session.initial_restore.clone());
                            // The left server has its own connection, which
                            // outlived this one. Its pane moves to the new
                            // browser with the transfers that only use it.
                            let left_server = session
                                .browser
                                .as_ref()
                                .and_then(|old| old.update(cx, |old, _| old.take_left_server()));
                            let browser = cx.new(|cx| {
                                SftpBrowser::new(
                                    conn_info,
                                    sftp_primary,
                                    sftp_prefetch,
                                    home_dir,
                                    file_associations,
                                    restore,
                                    cx,
                                )
                            });
                            cx.subscribe(&browser, |this, _browser, event, cx| match event {
                                BrowserEvent::StatusChanged(msg) => {
                                    cx.emit(ServerSessionEvent::StatusChanged(msg.clone()));
                                }
                                BrowserEvent::Error(msg) => {
                                    cx.emit(ServerSessionEvent::StatusChanged(format!(
                                        "Error: {msg}"
                                    )));
                                }
                                BrowserEvent::ConflictDetected { local, .. } => {
                                    this.pending_conflict = Some(local.clone());
                                    cx.emit(ServerSessionEvent::ConflictDetected);
                                }
                                BrowserEvent::TransferConflict => {
                                    cx.emit(ServerSessionEvent::TransferConflict);
                                }
                                BrowserEvent::CdTerminalHere(path) => {
                                    this.cd_terminal_to(path, cx);
                                }
                                BrowserEvent::EditInTerminal(path) => {
                                    this.edit_in_terminal(path.clone(), cx);
                                }
                                BrowserEvent::RequestPickerForFile { extension, local } => {
                                    cx.emit(ServerSessionEvent::PickerRequested {
                                        extension: extension.clone(),
                                        local: local.clone(),
                                    });
                                }
                                BrowserEvent::PathsChanged { local, remote } => {
                                    cx.emit(ServerSessionEvent::PathsChanged {
                                        local: local.clone(),
                                        remote: remote.clone(),
                                    });
                                }
                                BrowserEvent::TransferStarted {
                                    id,
                                    name,
                                    kind,
                                    route,
                                    uses_session,
                                    cancel,
                                } => {
                                    this.transfers.push(TransferTask {
                                        id: *id,
                                        name: name.clone(),
                                        kind: *kind,
                                        route: route.clone(),
                                        uses_session: *uses_session,
                                        done: 0,
                                        total: 0,
                                        is_queued: false,
                                        started: Instant::now(),
                                        rate: TransferRate::default(),
                                        outcome: None,
                                        finished_at: None,
                                        expanded: false,
                                        cancel: cancel.clone(),
                                    });
                                    this.ensure_tick(cx);
                                    cx.notify();
                                }
                                BrowserEvent::TransferProgress { id, done, total } => {
                                    if let Some(t) = this.transfers.iter_mut().find(|t| t.id == *id)
                                    {
                                        t.done = *done;
                                        t.total = *total;
                                        t.rate.record(Instant::now(), *done);
                                        cx.notify();
                                    }
                                }
                                BrowserEvent::TransferWaiting { id, is_waiting } => {
                                    if let Some(t) = this.transfers.iter_mut().find(|t| t.id == *id)
                                    {
                                        t.is_queued = *is_waiting;
                                        // Time and speed count from the moment
                                        // the bytes start to move.
                                        if !is_waiting {
                                            t.started = Instant::now();
                                            t.rate.reset();
                                        }
                                        cx.notify();
                                    }
                                }
                                BrowserEvent::TransferFinished { id, outcome } => {
                                    this.finish_transfer(*id, outcome.clone(), cx);
                                }
                                BrowserEvent::LeftSourcePickerRequested => {
                                    cx.emit(ServerSessionEvent::LeftSourcePickerRequested);
                                }
                                BrowserEvent::PeerRetryRequested { password } => {
                                    this.retry_peer(password.clone(), cx);
                                }
                                BrowserEvent::UseLocalRequested => {
                                    this.use_local_left(cx);
                                }
                            })
                            .detach();
                            session.browser = Some(browser);
                            session.restore_peer(left_server, cx);
                            session.terminals.clear();
                            for shell in shells {
                                session.add_terminal(shell, cx);
                            }
                            // A shell the server refused to reopen may have
                            // been the active one.
                            session.active_terminal = session
                                .active_terminal
                                .min(session.terminals.len().saturating_sub(1));
                            session.is_terminal_focus_pending = true;
                            session.reconnecting = None;
                            session.disconnected = None;
                            session.connect_failure = None;
                            for (entry_id, password) in
                                std::mem::take(&mut session.passwords_to_save)
                            {
                                cx.emit(ServerSessionEvent::PasswordAccepted {
                                    entry_id,
                                    password,
                                });
                            }
                            cx.emit(ServerSessionEvent::Connected {
                                elapsed: shown_connect_time(
                                    elapsed,
                                    is_reconnect,
                                    has_waited_on_user,
                                ),
                            });
                            cx.notify();
                        });
                        if built.is_err() {
                            return SessionOutcome::Gone;
                        }

                        // Forwards die with the connection, so every connect
                        // re-arms the ones marked auto-start. A forward the
                        // user started by hand is deliberately not persisted.
                        let auto_start = this
                            .update(cx, |session, _| {
                                session
                                    .forwards
                                    .iter()
                                    .filter(|f| f.auto_start)
                                    .map(|f| f.spec.clone())
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        for spec in auto_start {
                            conn.start_forward(spec).await;
                        }

                        let probing = this.update(cx, |session, cx| {
                            session.probe_host(conn.clone(), cx);
                        });
                        if probing.is_err() {
                            return SessionOutcome::Gone;
                        }
                        // Keep listening for forward changes and the disconnect.
                    }
                    Err(message) => {
                        // Authenticated, but channel setup failed. That is likely
                        // transient, so let the loop reconnect.
                        tracing::warn!(target: "flint::ssh", error = message, "post-connect setup failed");
                        let Ok(entry_id) = this.update(cx, |session, _| session.server_id.clone())
                        else {
                            return SessionOutcome::Gone;
                        };
                        return SessionOutcome::Ended(Some(ConnectFailure {
                            entry_id,
                            message,
                            is_password_rejected: false,
                            is_transient: true,
                        }));
                    }
                }
            }
            SshConnectionEvent::AuthFailed { entry_id, error } => {
                tracing::warn!(target: "flint::ssh", %error, "connect failed");
                let failure = this.update(cx, |session, _| {
                    let hop = session.hop_identity(Link::Session, &entry_id);
                    ConnectFailure {
                        message: failure_message(&error, &hop),
                        is_password_rejected: matches!(error, AuthError::WrongPassword),
                        // A failed TCP connect is transient (server still
                        // booting, flaky network). A rejected credential is not.
                        is_transient: matches!(error, AuthError::ConnectionFailed(_)),
                        entry_id,
                    }
                });
                return match failure {
                    Ok(failure) => SessionOutcome::Ended(Some(failure)),
                    Err(_) => SessionOutcome::Gone,
                };
            }
            SshConnectionEvent::HostKeyPrompt(prompt) => {
                // Park the handshake on the user: show the prompt and keep
                // listening. The reply sent from respond_host_key resumes the
                // connection or ends it.
                has_waited_on_user = true;
                let shown = this.update(cx, |session, cx| {
                    session.host_key_prompt = Some(HostKeyPromptState::new(prompt));
                    cx.notify();
                });
                if shown.is_err() {
                    return SessionOutcome::Gone;
                }
            }
            SshConnectionEvent::HostKeyTrusted {
                entry_id,
                fingerprint,
            } => {
                // Verified silently against ~/.ssh/known_hosts. Record it for
                // this session's reconnect loop and persist it on the hop's own
                // entry so future connects don't depend on the known_hosts file.
                let stored = this.update(cx, |session, cx| {
                    session.trust_host_key(Link::Session, &entry_id, &fingerprint);
                    cx.emit(ServerSessionEvent::HostKeyAccepted {
                        entry_id,
                        fingerprint,
                    });
                });
                if stored.is_err() {
                    return SessionOutcome::Gone;
                }
            }
            SshConnectionEvent::AuthPrompt(prompt) => {
                // Parked like the host-key prompt: the reply sent from
                // respond_auth_prompt resumes auth or ends it.
                has_waited_on_user = true;
                let shown = this.update(cx, |session, cx| {
                    session.auth_prompt = Some(AuthPromptState::new(prompt));
                    cx.notify();
                });
                if shown.is_err() {
                    return SessionOutcome::Gone;
                }
            }
            SshConnectionEvent::ForwardChanged { id, state } => {
                let applied = this.update(cx, |session, cx| {
                    session.apply_forward_state(&id, state, cx);
                });
                if applied.is_err() {
                    return SessionOutcome::Gone;
                }
            }
            SshConnectionEvent::Disconnected => {
                return SessionOutcome::Ended(None);
            }
        }
    }
    // The event channel closed without a Disconnected event. Treat it as a loss.
    SessionOutcome::Ended(None)
}

pub struct ServerSession {
    pub server_id: String,
    pub conn_info: Connection,
    pub connection: Option<SshConnection>,
    pub browser: Option<Entity<SftpBrowser>>,
    /// One per shell, all on the same connection. Empty until connected.
    /// See [`crate::ui::terminal_tabs`].
    pub(crate) terminals: Vec<Entity<TerminalView>>,
    /// Index into `terminals` of the tab on screen.
    pub(crate) active_terminal: usize,
    /// The active terminal should take the keyboard on the next render.
    pub(crate) is_terminal_focus_pending: bool,
    /// The file panes are hidden and the terminal fills the tab.
    pub(crate) is_sftp_hidden: bool,
    /// The server's shell history, kept so a terminal tab opened later gets
    /// the same suggestions as the first.
    pub(crate) shell_history: Vec<String>,
    /// Folders persisted for this host, used to seed the first browser build so
    /// a restart reopens them. Later reconnects use the live browser's paths.
    initial_restore: Option<RestorePaths>,
    pub disconnected: Option<DisconnectedState>,
    /// Why a session that never connected stopped trying. Drives the "could
    /// not connect" card; the reconnect loop waits there for a retry.
    connect_failure: Option<ConnectFailure>,
    /// The password field on that card. Created lazily on the first render
    /// that shows it: the reconnect loop has no `Window` to create it with.
    password_input: Option<Entity<InputState>>,
    is_saving_password: bool,
    /// Passwords typed on the card, one per hop, held until the whole route
    /// connects. A jumped session can ask for the bastion's and then the
    /// target's, so an earlier one must outlive a later failure.
    passwords_to_save: Vec<(String, Zeroizing<String>)>,
    /// `Some(attempt)` while an automatic reconnect is in flight (drives the
    /// "Reconnecting" overlay). `None` once connected or permanently given up.
    pub reconnecting: Option<u32>,
    /// Cuts a reconnect backoff short. The loop waits on this alongside its
    /// timer, so a system wake retries at once instead of sitting out a delay
    /// that was measured against a network the machine took down with it.
    /// It also restarts the loop while it waits on the "could not connect" card.
    retry_now: async_channel::Sender<()>,
    /// A host-key decision blocking the in-flight handshake, rendered as a
    /// modal overlay. `None` when no prompt is pending.
    pub host_key_prompt: Option<HostKeyPromptState>,
    /// A keyboard-interactive challenge blocking the in-flight auth, rendered
    /// as a modal overlay. `None` when no prompt is pending.
    pub auth_prompt: Option<AuthPromptState>,
    /// The connection to another saved server shown in the left file pane.
    /// `None` while that pane shows this computer.
    pub(crate) peer: Option<PeerLink>,
    /// The path this session connects over: the bastions in front of the host,
    /// the host itself, and the fingerprints to trust silently. Shared with the
    /// reconnect loop, which respawns from it: accepting a key mid-session
    /// updates the matching hop so later reconnects don't re-prompt.
    route: Arc<Mutex<SshRoute>>,
    pub pending_conflict: Option<PathBuf>,
    /// Active ad-hoc SFTP transfers (newest last).
    pub transfers: Vec<TransferTask>,
    /// Finished transfers, newest first, capped at [`HISTORY_CAP`].
    pub transfer_history: Vec<TransferTask>,
    /// Whether the tasks popover is open.
    pub tasks_open: bool,
    /// The host's saved port forwards and their live state.
    pub forwards: Vec<ForwardRuntime>,
    /// Whether the forwards popover is open.
    pub forwards_open: bool,
    /// A 1 s repaint loop is live (drives elapsed-time labels while transfers run).
    ticking: bool,
    pub(crate) connection_generation: u64,
    file_associations: SharedFileAssociations,
    /// Drag state for the resizable SFTP / terminal split.
    split_state: Entity<ResizableState>,
    /// The divider when the terminal sits next to the files. Kept apart so
    /// each layout remembers its own sizes.
    split_state_beside: Entity<ResizableState>,
    _connect_task: Task<()>,
    /// Holds a clone of the connection, which stays open as long as any
    /// clone lives. Kept here so that it ends with the session.
    probe_task: Option<Task<()>>,
}

impl EventEmitter<ServerSessionEvent> for ServerSession {}

impl ServerSession {
    pub fn start_connect(
        conn_info: Connection,
        route: SshRoute,
        initial_restore: Option<RestorePaths>,
        forwards: Vec<ForwardRuntime>,
        file_associations: SharedFileAssociations,
        cx: &mut Context<Self>,
    ) -> Self {
        let server_id = conn_info.id.clone();
        let stored_conn = conn_info.clone();

        let route = Arc::new(Mutex::new(route));

        // Capacity 1: repeated nudges collapse into one pending retry.
        let (retry_tx, retry_rx) = async_channel::bounded::<()>(1);

        // First connection. The reconnect loop spawns later ones from a fresh
        // clone of the route, so the credentials it carries stay available.
        let mut connect_started = Instant::now();
        let ssh_conn = SshConnection::spawn(route.lock().expect("route mutex").clone());

        let task = {
            let first_conn = ssh_conn.clone();
            let route = route.clone();
            let retry_rx = retry_rx.clone();
            let target_id = server_id.clone();
            cx.spawn(async move |this, cx| {
                let mut conn = first_conn;
                let mut attempt: u32 = 0;
                loop {
                    // Publish the active connection (and any reconnect status) so the
                    // UI and snippet runs see the live handle.
                    if this
                        .update(cx, |session, cx| {
                            session.connection = Some(conn.clone());
                            session.connection_generation =
                                session.connection_generation.wrapping_add(1);
                            session.reconnecting = (attempt > 0).then_some(attempt);
                            // Prompts from a dead connection cannot be
                            // answered. Drop them so a stale modal does not
                            // stay over the reconnect or lost screen.
                            session.auth_prompt = None;
                            session.host_key_prompt = None;
                            // Forwards never survive a connection, so a row
                            // still showing "bound" would be lying.
                            for forward in session.forwards.iter_mut() {
                                forward.state = ForwardState::Stopped { reason: None };
                            }
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }

                    let failure =
                        match drive_connection(conn.clone(), connect_started, &this, cx).await {
                            SessionOutcome::Gone => break,
                            SessionOutcome::Ended(failure) => {
                                failure.unwrap_or_else(|| ConnectFailure {
                                    entry_id: target_id.clone(),
                                    message: CONNECTION_CLOSED.to_string(),
                                    is_password_rejected: false,
                                    is_transient: true,
                                })
                            }
                        };
                    // The workers of this connection went down without a final
                    // report. The left server stays connected, and so does a
                    // transfer between it and this computer.
                    let Ok((has_connected, has_recovered)) = this.update(cx, |session, cx| {
                        session.fail_open_transfers(&failure.message, cx);
                        // Only a connect that got through clears `reconnecting`.
                        (session.browser.is_some(), session.reconnecting.is_none())
                    }) else {
                        break;
                    };
                    // The retries are counted per drop. Without this a tab
                    // that recovered five times would give up on the sixth
                    // drop without a single try.
                    if has_recovered {
                        attempt = 0;
                    }

                    // The retry ladder is for a link that dropped. A host that
                    // never connected is more likely mistyped than flaky, so
                    // the user gets the reason at once instead of a long wait.
                    let is_retrying =
                        has_connected && failure.is_transient && attempt < MAX_RECONNECT_ATTEMPTS;
                    if !is_retrying {
                        if this
                            .update(cx, |session, cx| session.give_up(failure, cx))
                            .is_err()
                            || has_connected
                        {
                            break;
                        }
                        // A connect that got as far as logging in is still
                        // open. Dropping the last handle closes it, so it does
                        // not sit idle behind the error card.
                        drop(conn);
                        // The error card is up. Wait there until the user (or a
                        // system wake) asks for another try.
                        while retry_rx.try_recv().is_ok() {}
                        if retry_rx.recv().await.is_err() {
                            break;
                        }
                    } else {
                        attempt += 1;
                        // Back off 1s, 2s, 4s, then 8s between tries.
                        if this
                            .update(cx, |session, cx| {
                                session.reconnecting = Some(attempt);
                                cx.notify();
                            })
                            .is_err()
                        {
                            break;
                        }
                        // A nudge that arrived while the link was healthy says
                        // nothing about this wait. Drop it before listening.
                        while retry_rx.try_recv().is_ok() {}
                        let backoff = Duration::from_secs(1u64 << (attempt - 1).min(3));
                        let timer = cx.background_executor().timer(backoff);
                        futures::future::select(Box::pin(timer), Box::pin(retry_rx.recv())).await;
                    }
                    connect_started = Instant::now();
                    // Clone out of the mutex in one statement: the guard must
                    // not be alive across the next await.
                    conn = SshConnection::spawn(route.lock().expect("route mutex").clone());
                }
            })
        };

        Self {
            server_id,
            conn_info: stored_conn,
            connection: Some(ssh_conn),
            browser: None,
            terminals: Vec::new(),
            active_terminal: 0,
            is_terminal_focus_pending: false,
            is_sftp_hidden: false,
            shell_history: Vec::new(),
            initial_restore,
            disconnected: None,
            connect_failure: None,
            password_input: None,
            is_saving_password: true,
            passwords_to_save: Vec::new(),
            reconnecting: None,
            host_key_prompt: None,
            auth_prompt: None,
            peer: None,
            retry_now: retry_tx,
            route,
            pending_conflict: None,
            transfers: Vec::new(),
            transfer_history: Vec::new(),
            tasks_open: false,
            forwards,
            forwards_open: false,
            ticking: false,
            connection_generation: 0,
            file_associations,
            split_state: cx.new(|_| ResizableState::default()),
            split_state_beside: cx.new(|_| ResizableState::default()),
            _connect_task: task,
            probe_task: None,
        }
    }

    /// The machine just woke from sleep. A reconnect backoff that is still
    /// counting down was measured against a network that went away with the
    /// machine, so retry at once. A session that already gave up is restarted
    /// from scratch, unless its "connection lost" modal is also telling the
    /// user where their unsaved files went, which reconnecting would take away.
    pub fn retry_after_wake(&mut self, cx: &mut Context<Self>) {
        if self.reconnecting.is_some() {
            let _ = self.retry_now.try_send(());
            return;
        }
        // A first connect that failed for a network reason may have been
        // tried against a network that was not up yet.
        if self
            .connect_failure
            .as_ref()
            .is_some_and(|failure| failure.is_transient)
        {
            tracing::info!(
                target: "flint::ssh",
                host = %self.conn_info.display_name(),
                "system wake: retrying a failed connect",
            );
            self.retry_connect(cx);
            return;
        }
        if self
            .disconnected
            .as_ref()
            .is_some_and(|state| state.preserved.count == 0)
        {
            tracing::info!(
                target: "flint::ssh",
                host = %self.conn_info.display_name(),
                "system wake: reconnecting a dropped session",
            );
            cx.emit(ServerSessionEvent::ReconnectRequested);
        }
    }

    /// Type `cd <path>` into this session's shell and run it.
    fn cd_terminal_to(&mut self, path: &str, cx: &mut Context<Self>) {
        let Some(terminal) = self.active_terminal().map(|terminal| terminal.read(cx)) else {
            cx.emit(ServerSessionEvent::StatusChanged(
                "No terminal in this session".to_string(),
            ));
            return;
        };
        // On the alternate screen the keys would go to vim or htop, not to a shell.
        if terminal
            .last_snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.alt_screen)
        {
            cx.emit(ServerSessionEvent::StatusChanged(
                "Terminal is busy with a full-screen program".to_string(),
            ));
            return;
        }
        let Some(input) = crate::shell::cd_input(path) else {
            cx.emit(ServerSessionEvent::StatusChanged(
                "Can't cd: the folder name has a control character".to_string(),
            ));
            return;
        };
        let shell = terminal.shell.clone();
        tracing::debug!(path, "cd terminal to the file pane folder");
        cx.spawn(async move |_, _| {
            shell.send_input(input).await;
        })
        .detach();
    }

    /// The session never connected and is showing the "could not connect" card.
    pub fn has_connect_failure(&self) -> bool {
        self.connect_failure.is_some()
    }

    /// Whether that card is about this vault entry: the tab's own host, or the
    /// jump host that failed on the way to it.
    pub fn has_connect_failure_for(&self, entry_id: &str) -> bool {
        self.connect_failure
            .as_ref()
            .is_some_and(|failure| self.server_id == entry_id || failure.entry_id == entry_id)
    }

    /// Move a transfer from the running list to the history. Does nothing for
    /// an id that is not running, so a second report is harmless.
    fn finish_transfer(&mut self, id: u64, outcome: TransferOutcome, cx: &mut Context<Self>) {
        let Some(pos) = self.transfers.iter().position(|t| t.id == id) else {
            return;
        };
        let mut task = self.transfers.remove(pos);
        // A dropped final progress tick (throttled, or try_send onto a full
        // channel) can leave a completed transfer stuck below 100%. Snap it
        // so the history row shows the real end state.
        if matches!(outcome, TransferOutcome::Done { .. }) && task.total > 0 {
            task.done = task.total;
        }
        // Canceled while it waited: the history row must not say it still waits.
        task.is_queued = false;
        task.outcome = Some(outcome.clone());
        task.finished_at = Some(Instant::now());
        cx.emit(ServerSessionEvent::TransferFinished {
            name: task.name.clone(),
            kind: task.kind,
            route: task.route.clone(),
            outcome,
        });
        self.transfer_history.insert(0, task);
        self.transfer_history.truncate(HISTORY_CAP);
        cx.notify();
    }

    /// The session's own connection is gone: end every transfer that ran
    /// over it as failed, since no worker is left to report them. The browser
    /// is told as well, so nothing new starts on that connection.
    fn fail_open_transfers(&mut self, reason: &str, cx: &mut Context<Self>) {
        if let Some(browser) = &self.browser {
            browser.update(cx, |browser, _| browser.session_ended());
        }
        let ids: Vec<u64> = self
            .transfers
            .iter()
            .filter(|t| t.uses_session)
            .map(|t| t.id)
            .collect();
        if ids.is_empty() {
            return;
        }
        tracing::warn!(
            target: "flint::sftp",
            host = %self.conn_info.display_name(),
            count = ids.len(),
            reason,
            "connection ended with transfers still running",
        );
        for task in self.transfers.iter().filter(|t| t.uses_session) {
            task.request_cancel();
        }
        for id in ids {
            self.finish_transfer(id, TransferOutcome::Failed(reason.to_string()), cx);
        }
    }

    /// Stop trying. A session that was connected keeps its panes under the
    /// "connection lost" modal; one that never connected shows the error card.
    fn give_up(&mut self, failure: ConnectFailure, cx: &mut Context<Self>) {
        tracing::warn!(
            target: "flint::ssh",
            host = %self.conn_info.display_name(),
            reason = %failure.message,
            "giving up on the connection",
        );
        self.connection = None;
        self.probe_task = None;
        self.reconnecting = None;
        // Nothing is left to answer these prompts.
        self.auth_prompt = None;
        self.host_key_prompt = None;
        // The left server is of no use behind the "connection lost" modal.
        self.drop_peer(cx);
        // The last connection took every forward down with it.
        for forward in self.forwards.iter_mut() {
            forward.state = ForwardState::Stopped { reason: None };
        }
        match &self.browser {
            Some(browser) => {
                let preserved = browser.update(cx, |b, _| b.cleanup_on_disconnect());
                self.disconnected = Some(DisconnectedState {
                    host: self.conn_info.display_name().to_string(),
                    preserved,
                    reason: failure.message,
                });
            }
            None => self.connect_failure = Some(failure),
        }
        cx.emit(ServerSessionEvent::ConnectionStateChanged);
        cx.notify();
    }

    /// Leave the error card and let the parked reconnect loop try again with
    /// whatever the route holds now.
    fn retry_connect(&mut self, cx: &mut Context<Self>) {
        self.connect_failure = None;
        self.password_input = None;
        let _ = self.retry_now.try_send(());
        cx.emit(ServerSessionEvent::ConnectionStateChanged);
        cx.notify();
    }

    /// Retry with the password typed on the error card. It goes into the route
    /// only; the vault gets it once it has worked (see `PasswordAccepted`).
    fn submit_password(&mut self, cx: &mut Context<Self>) {
        let Some(failure) = &self.connect_failure else {
            return;
        };
        if !failure.is_password_rejected {
            return;
        }
        let entry_id = failure.entry_id.clone();
        let Some(input) = &self.password_input else {
            return;
        };
        let password = Zeroizing::new(input.read(cx).value().to_string());
        if password.is_empty() {
            return;
        }
        if !set_hop_password(
            &mut self.route.lock().expect("route mutex"),
            &entry_id,
            &password,
        ) {
            tracing::warn!(
                entry_id,
                "password typed for a hop that is not on the route"
            );
            return;
        }
        // The route now holds this password, so an older one typed for the
        // same hop is no longer what a successful connect would prove.
        self.passwords_to_save.retain(|(id, _)| *id != entry_id);
        if self.is_saving_password {
            self.passwords_to_save.push((entry_id, password));
        }
        self.retry_connect(cx);
    }

    /// The password field is created here, on the first render that shows the
    /// card, for the same reason as the auth prompt inputs below.
    fn ensure_password_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let is_wanted = self
            .connect_failure
            .as_ref()
            .is_some_and(|failure| failure.is_password_rejected);
        if !is_wanted || self.password_input.is_some() {
            return;
        }
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder("New password")
        });
        cx.subscribe(&input, |this, _input, event: &InputEvent, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.submit_password(cx);
            }
        })
        .detach();
        // A dialog on top (the edit form, a confirm) keeps the keyboard.
        if !window.has_active_dialog(cx) {
            input.update(cx, |state, cx| state.focus(window, cx));
        }
        self.password_input = Some(input);
    }

    /// Detect the remote OS and fetch the shell history in a task of their
    /// own. Awaited in the event loop, a host whose exec never closes (a
    /// forced command, a menu shell on network gear) would hold back every
    /// forward and disconnect event.
    fn probe_host(&mut self, conn: SshConnection, cx: &mut Context<Self>) {
        // With suggestions off, no exec channel is opened for the history.
        let is_suggestions_enabled = Preferences::is_suggestions_enabled(cx);
        self.probe_task = Some(cx.spawn(async move |this, cx| {
            let limit = cx.background_executor().timer(PROBE_TIMEOUT);
            if let Some(os) = finish_within(detect_os(&conn), limit).await.flatten() {
                this.update(cx, |_session, cx| {
                    cx.emit(ServerSessionEvent::OsDetected(os))
                })
                .ok();
            }
            if !is_suggestions_enabled {
                return;
            }
            let limit = cx.background_executor().timer(PROBE_TIMEOUT);
            let history = finish_within(fetch_shell_history(&conn), limit)
                .await
                .unwrap_or_default();
            if history.is_empty() {
                return;
            }
            this.update(cx, |session, cx| {
                // Kept for the terminal tabs opened later.
                session.shell_history = history.clone();
                for terminal in session.terminals.clone() {
                    terminal.update(cx, |terminal, cx| {
                        terminal.set_history(history.clone(), cx);
                    });
                }
            })
            .ok();
        }));
    }

    pub fn shell_handle(&self, cx: &App) -> Option<ShellHandle> {
        self.active_terminal()
            .map(|terminal| terminal.read(cx).shell.clone())
    }

    /// Record what the SSH thread reports about one forward. The state is
    /// always complete, so a missed event heals on the next one.
    fn apply_forward_state(&mut self, id: &str, state: ForwardState, cx: &mut Context<Self>) {
        let Some(forward) = self.forwards.iter_mut().find(|f| f.spec.id == id) else {
            return;
        };
        forward.state = state;
        // An auto-start that failed has no other way to reach the user: the
        // popover may well be closed.
        if let Some(reason) = forward.failure() {
            let message = format!("Port forward '{}' stopped: {reason}", forward.name);
            cx.emit(ServerSessionEvent::StatusChanged(message));
        }
        cx.notify();
    }

    /// Adopt the host's forwards after the server form saved them. A row the
    /// user deleted or re-pointed is stopped on the live connection first, so a
    /// session never keeps running a spec that no longer exists; it stays
    /// stopped until the user starts it again or the session reconnects.
    pub fn update_forwards(&mut self, saved: Vec<ForwardRuntime>, cx: &mut Context<Self>) {
        // Stopped whether or not the row looks bound here: a forward started a
        // moment ago is still stopped in this list until its event lands, and
        // skipping it would leave it listening with no row to stop it from.
        let stale: Vec<String> = self
            .forwards
            .iter()
            .filter(|old| !saved.iter().any(|new| is_same_route(&new.spec, &old.spec)))
            .map(|old| old.spec.id.clone())
            .collect();

        let mut next = saved;
        for forward in next.iter_mut() {
            let previous = self
                .forwards
                .iter()
                .find(|old| is_same_route(&forward.spec, &old.spec));
            if let Some(previous) = previous {
                forward.state = previous.state.clone();
            }
        }
        self.forwards = next;
        cx.notify();

        if stale.is_empty() {
            return;
        }
        let Some(conn) = self.connection.clone() else {
            return;
        };
        cx.spawn(async move |_, _| {
            for id in stale {
                conn.stop_forward(id).await;
            }
        })
        .detach();
    }

    /// The route a link connects over and the host it is for. A left server
    /// that is gone falls back to the session's own.
    fn link_target(&self, link: Link) -> (&Arc<Mutex<SshRoute>>, &Connection) {
        match (link, &self.peer) {
            (Link::Peer, Some(peer)) => (&peer.route, &peer.info),
            _ => (&self.route, &self.conn_info),
        }
    }

    /// Look up a hop on a link's route by its vault entry id. An id that is
    /// not on the route falls back to the target, the only hop a direct
    /// connection has.
    pub(crate) fn hop_identity(&self, link: Link, entry_id: &str) -> HopIdentity {
        let (route, target) = self.link_target(link);
        let route = route.lock().expect("route mutex");
        let Some(hop) = route.hops().find(|hop| hop.entry_id == entry_id) else {
            tracing::warn!(entry_id, "event from a hop that is not on the route");
            return HopIdentity {
                name: target.display_name().to_string(),
                host: target.host.clone(),
                username: target.username.clone(),
                is_jump: false,
            };
        };
        HopIdentity {
            name: hop.display_name().to_string(),
            host: hop.host.clone(),
            username: hop.username.clone(),
            is_jump: entry_id != target.id,
        }
    }

    /// Trust a fingerprint for one hop so a later connect over the same
    /// route verifies it silently instead of prompting again.
    pub(crate) fn trust_host_key(&mut self, link: Link, entry_id: &str, fingerprint: &str) {
        let (route, _) = self.link_target(link);
        let mut route = route.lock().expect("route mutex");
        let Some(hop) = route.hops_mut().find(|hop| hop.entry_id == entry_id) else {
            tracing::warn!(
                entry_id,
                "trusted a host key for a hop that is not on the route"
            );
            return;
        };
        hop.expected_host_key = Some(fingerprint.to_string());
    }

    fn host_key_prompt(&self, link: Link) -> Option<&HostKeyPromptState> {
        match link {
            Link::Session => self.host_key_prompt.as_ref(),
            Link::Peer => self.peer.as_ref()?.host_key_prompt.as_ref(),
        }
    }

    fn take_host_key_prompt(&mut self, link: Link) -> Option<HostKeyPromptState> {
        match link {
            Link::Session => self.host_key_prompt.take(),
            Link::Peer => self.peer.as_mut()?.host_key_prompt.take(),
        }
    }

    fn auth_prompt(&self, link: Link) -> Option<&AuthPromptState> {
        match link {
            Link::Session => self.auth_prompt.as_ref(),
            Link::Peer => self.peer.as_ref()?.auth_prompt.as_ref(),
        }
    }

    fn auth_prompt_mut(&mut self, link: Link) -> Option<&mut AuthPromptState> {
        match link {
            Link::Session => self.auth_prompt.as_mut(),
            Link::Peer => self.peer.as_mut()?.auth_prompt.as_mut(),
        }
    }

    fn take_auth_prompt(&mut self, link: Link) -> Option<AuthPromptState> {
        match link {
            Link::Session => self.auth_prompt.take(),
            Link::Peer => self.peer.as_mut()?.auth_prompt.take(),
        }
    }

    /// Resolve a link's pending host-key prompt. Accepting resumes the parked
    /// handshake, records the fingerprint on the route, and emits an event so
    /// the vault entry is updated. Rejecting aborts the connection: the
    /// session's own closes the tab, the left server's goes back to LOCAL.
    fn respond_host_key(&mut self, link: Link, accept: bool, cx: &mut Context<Self>) {
        let Some(prompt) = self.take_host_key_prompt(link) else {
            return;
        };
        let _ = prompt.reply.send(accept);
        if accept {
            self.trust_host_key(link, &prompt.entry_id, &prompt.fingerprint);
            cx.emit(ServerSessionEvent::HostKeyAccepted {
                entry_id: prompt.entry_id,
                fingerprint: prompt.fingerprint,
            });
        } else {
            self.abandon_link(link, cx);
        }
        cx.notify();
    }

    /// Resolve a link's pending keyboard-interactive prompt. Submitting sends
    /// one answer per field (in order) and resumes the parked auth; canceling
    /// aborts the connection, as rejecting a host key does.
    fn respond_auth_prompt(&mut self, link: Link, submit: bool, cx: &mut Context<Self>) {
        let Some(prompt) = self.take_auth_prompt(link) else {
            return;
        };
        if submit {
            let answers: Vec<Zeroizing<String>> = prompt
                .inputs
                .iter()
                .map(|input| Zeroizing::new(input.read(cx).value().to_string()))
                .collect();
            let _ = prompt.reply.send(Some(answers));
        } else {
            let _ = prompt.reply.send(None);
            self.abandon_link(link, cx);
        }
        cx.notify();
    }

    /// The user turned a prompt down, so the connection behind it is over.
    fn abandon_link(&mut self, link: Link, cx: &mut Context<Self>) {
        match link {
            Link::Session => cx.emit(ServerSessionEvent::CloseRequested),
            // A link that still asks shows no files, so nothing guards it.
            Link::Peer => self.drop_peer(cx),
        }
    }

    /// An auth prompt arrives on a connection event pump, which has no
    /// `Window`, so its input entities are created here, on the first render
    /// that shows the prompt. Focus lands on the first field.
    fn ensure_auth_prompt_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for link in [Link::Session, Link::Peer] {
            self.ensure_auth_prompt_inputs_for(link, window, cx);
        }
    }

    fn ensure_auth_prompt_inputs_for(
        &mut self,
        link: Link,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let field_echos: Vec<bool> = match self.auth_prompt(link) {
            Some(p) if p.inputs.is_empty() && !p.fields.is_empty() => {
                p.fields.iter().map(|f| f.echo).collect()
            }
            _ => return,
        };
        let inputs: Vec<Entity<InputState>> = field_echos
            .iter()
            .map(|&echo| cx.new(|cx| InputState::new(window, cx).masked(!echo)))
            .collect();
        for input in &inputs {
            cx.subscribe(input, move |this, _input, event: &InputEvent, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.respond_auth_prompt(link, true, cx);
                }
            })
            .detach();
        }
        if let Some(first) = inputs.first() {
            first.update(cx, |state, cx| state.focus(window, cx));
        }
        if let Some(prompt) = self.auth_prompt_mut(link) {
            prompt.inputs = inputs;
        }
    }

    pub fn cleanup_before_close(&mut self, cx: &mut Context<Self>) {
        if let Some(browser) = &self.browser {
            let preserved = browser.update(cx, |b, _| b.cleanup_on_disconnect());
            if preserved.count > 0 {
                tracing::info!(
                    count = preserved.count,
                    path = %preserved.cache_root.display(),
                    "tab closed with unsaved files preserved"
                );
            }
        }
    }

    /// Spawn a 1 s repaint loop so elapsed-time labels advance while transfers
    /// are active. Self-terminates once nothing remains.
    fn ensure_tick(&mut self, cx: &mut Context<Self>) {
        if self.ticking || self.transfers.is_empty() {
            return;
        }
        self.ticking = true;
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(1000))
                .await;
            let keep_going = this
                .update(cx, |s, cx| {
                    if s.transfers.is_empty() {
                        s.ticking = false;
                        false
                    } else {
                        cx.notify();
                        true
                    }
                })
                .unwrap_or(false);
            if !keep_going {
                break;
            }
        })
        .detach();
    }
}

impl ServerSession {
    /// Move the keys from the terminal to the file panes, or back. This is the
    /// only way into the file panes without the mouse: the terminal takes Tab.
    pub fn toggle_files_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(browser), Some(terminal_view)) = (&self.browser, self.active_terminal()) else {
            return;
        };
        // Both are covered by the "connection lost" overlay.
        if self.disconnected.is_some() {
            return;
        }
        if browser.read(cx).has_focus(window, cx) {
            terminal_view.focus_handle(cx).focus(window, cx);
        } else {
            browser.update(cx, |browser, cx| browser.focus_files(window, cx));
            self.show_file_panes(cx);
        }
    }

    /// Bring the keys to the left file pane, whatever it shows. Used when
    /// its source was just chosen, so the keys end up in the new file list.
    pub fn focus_left_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(browser) = &self.browser else { return };
        // Covered by the "connection lost" overlay.
        if self.disconnected.is_some() {
            return;
        }
        browser.update(cx, |browser, cx| browser.focus_left(window, cx));
        // A source was just chosen for the second pane, so it must be seen.
        if !Preferences::is_second_pane_shown(cx) {
            Preferences::update(cx, |prefs| prefs.is_second_pane_shown = true);
            self.repaint_files(cx);
        }
        self.show_file_panes(cx);
    }

    /// What should hold the keys when this tab comes to the front: its
    /// terminal, or while there is none yet, a field that waits for an
    /// answer. `None` while the session is only connecting.
    pub fn focus_target(&self, cx: &App) -> Option<FocusHandle> {
        if let Some(terminal) = self.active_terminal() {
            return Some(terminal.read(cx).focus_handle(cx));
        }
        [Link::Session, Link::Peer]
            .into_iter()
            .filter_map(|link| self.auth_prompt(link))
            .find_map(|prompt| prompt.inputs.first())
            .or(self.password_input.as_ref())
            .map(|input| input.read(cx).focus_handle(cx))
    }

    /// Hidden panes cannot take the keys.
    fn show_file_panes(&mut self, cx: &mut Context<Self>) {
        if self.is_sftp_hidden {
            self.is_sftp_hidden = false;
            cx.notify();
        }
    }
}

impl ServerSession {
    /// The whole tab for a session that never connected: what went wrong and
    /// what to do about it. A rejected password can be retyped in place.
    fn render_connect_failure(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let failure = self.connect_failure.as_ref()?;
        let theme = cx.theme();
        let hop = self.hop_identity(Link::Session, &failure.entry_id);
        let edit_label = if hop.is_jump {
            "Edit jump host"
        } else {
            "Edit host"
        };
        let entry_id = failure.entry_id.clone();
        let message = failure.message.clone();
        let password_input = self
            .password_input
            .as_ref()
            .filter(|_| failure.is_password_rejected);

        // Fixed width, not max_w: same wrapped-text problem as the host-key
        // card below.
        let mut card = v_flex()
            .px(px(12.))
            .py(px(10.))
            .w(px(380.))
            .bg(flint_theme::pop_surface())
            .rounded(px(6.))
            .border_1()
            .border_color(flint_theme::border_focused())
            .shadow_lg()
            .gap(px(8.))
            .child(
                div()
                    .text_size(px(13.))
                    .font_semibold()
                    .text_color(theme.danger)
                    .child(format!(
                        "Could not connect to {}",
                        self.conn_info.display_name()
                    )),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .child(message.clone()),
            );

        if let Some(input) = password_input {
            card = card.child(Input::new(input).small()).child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Checkbox::new("connect-failure-save-password")
                            .checked(self.is_saving_password)
                            .label("Save this password")
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.is_saving_password = *checked;
                                cx.notify();
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("connect-failure-password")
                            .small()
                            .primary()
                            .child("Connect")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.submit_password(cx);
                            })),
                    ),
            );
        }

        let mut actions = h_flex()
            .gap_2()
            .justify_end()
            .child(
                Button::new("connect-failure-close")
                    .small()
                    .ghost()
                    .child("Close")
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.emit(ServerSessionEvent::CloseRequested);
                    })),
            )
            .child(
                Button::new("connect-failure-copy")
                    .small()
                    .ghost()
                    .child("Copy error")
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(message.clone()));
                    })),
            )
            .child(
                Button::new("connect-failure-edit")
                    .small()
                    .child(edit_label)
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(ServerSessionEvent::EditRequested {
                            entry_id: entry_id.clone(),
                        });
                    })),
            );
        // With the password field up, "Connect" is the retry. A plain retry
        // would only send the rejected password again.
        if password_input.is_none() {
            actions = actions.child(
                Button::new("connect-failure-retry")
                    .small()
                    .primary()
                    .child("Retry")
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.emit(ServerSessionEvent::ReconnectRequested);
                    })),
            );
        }

        Some(
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(card.child(actions))
                .into_any_element(),
        )
    }

    /// Modal overlay for a pending host-key decision. First contact gets a
    /// neutral "verify this fingerprint" prompt; a changed key gets a loud
    /// warning since it can mean a man-in-the-middle.
    fn render_host_key_prompt(&self, link: Link, cx: &mut Context<Self>) -> Option<AnyElement> {
        let prompt = self.host_key_prompt(link)?;
        let theme = cx.theme();
        // Name the machine that presented the key. On a jumped session that is
        // often a bastion, and blaming the target would send the user looking
        // at the wrong server.
        let hop = self.hop_identity(link, &prompt.entry_id);
        let host = if hop.is_jump {
            format!("jump host {} ({})", hop.name, hop.host)
        } else {
            hop.host.clone()
        };
        let mismatch = prompt.stored_fingerprint.is_some();

        let fingerprint_row = |label: &str, value: String, color: gpui::Hsla| {
            v_flex()
                .gap_1()
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.muted_foreground)
                        .child(label.to_string()),
                )
                .child(
                    div()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(theme.muted)
                        .font_family("Cascadia Code")
                        .text_size(px(11.5))
                        .text_color(color)
                        .child(value),
                )
        };

        // Fixed width, not max_w: with max_w the wrapped description text
        // under-reports the card's height and the footer buttons render
        // outside the painted box (same convention as the app's dialogs).
        let mut card = v_flex()
            .px(px(12.))
            .py(px(10.))
            .w(px(480.))
            .bg(flint_theme::pop_surface())
            .rounded(px(6.))
            .border_1()
            .border_color(if mismatch {
                theme.danger
            } else {
                flint_theme::border_focused()
            })
            .shadow_lg()
            .gap(px(8.));

        if mismatch {
            card = card
                .child(
                    div()
                        .text_color(theme.danger)
                        .text_size(px(13.))
                        .font_semibold()
                        .child(format!("Host key for {host} changed")),
                )
                .child(div().text_size(px(12.)).text_color(theme.foreground).child(
                    "The host presented a different key than the one you trusted \
                             before. It may have been reinstalled, or someone may be \
                             intercepting the connection. Do not trust the new key unless \
                             you know why it changed.",
                ))
                .child(fingerprint_row(
                    "Previously trusted",
                    prompt.stored_fingerprint.clone().unwrap_or_default(),
                    theme.muted_foreground,
                ))
                .child(fingerprint_row(
                    &format!("Presented now ({})", prompt.key_type),
                    prompt.fingerprint.clone(),
                    theme.danger,
                ))
                .child(
                    h_flex()
                        .gap_2()
                        .justify_end()
                        .child(
                            Button::new("hostkey-reject")
                                .small()
                                .primary()
                                .child("Cancel connection")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.respond_host_key(link, false, cx);
                                })),
                        )
                        .child(
                            Button::new("hostkey-accept")
                                .small()
                                .danger()
                                .child("Trust new key")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.respond_host_key(link, true, cx);
                                })),
                        ),
                );
        } else {
            card = card
                .child(
                    div()
                        .text_size(px(13.))
                        .font_semibold()
                        .text_color(theme.foreground)
                        .child(format!("Verify host key for {host}")),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme.muted_foreground)
                        .child(
                            "First connection to this host. Compare this fingerprint \
                             with the one the host reports before you trust it.",
                        ),
                )
                .child(fingerprint_row(
                    &format!("Fingerprint ({})", prompt.key_type),
                    prompt.fingerprint.clone(),
                    theme.foreground,
                ))
                .child(
                    h_flex()
                        .gap_2()
                        .justify_end()
                        .child(
                            Button::new("hostkey-reject")
                                .small()
                                .ghost()
                                .child("Cancel")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.respond_host_key(link, false, cx);
                                })),
                        )
                        .child(
                            Button::new("hostkey-accept")
                                .small()
                                .primary()
                                .child("Trust and connect")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.respond_host_key(link, true, cx);
                                })),
                        ),
                );
        }

        Some(
            div()
                .absolute()
                .inset_0()
                // The working area is a rounded card; gpui does not clip to its
                // corners, so the overlay rounds its own top ones.
                .rounded_t(px(crate::ui::PANEL_RADIUS - 1.))
                .bg(theme.overlay)
                .flex()
                .items_center()
                .justify_center()
                .on_mouse_down(MouseButton::Left, |_, _, _| {})
                .on_mouse_down(MouseButton::Right, |_, _, _| {})
                .child(card)
                .into_any_element(),
        )
    }

    /// Modal overlay for a keyboard-interactive challenge (2FA codes, PAM
    /// prompts). Field labels and instructions come from the server.
    fn render_auth_prompt(&self, link: Link, cx: &mut Context<Self>) -> Option<AnyElement> {
        let prompt = self.auth_prompt(link)?;
        let theme = cx.theme();
        // Same reason as the host-key card: the challenge may come from a
        // bastion, so the title has to name the hop that asked.
        let hop = self.hop_identity(link, &prompt.entry_id);
        let (_, target) = self.link_target(link);
        let host = hop.host.clone();
        let username = hop.username.clone();

        let title = if prompt.name.trim().is_empty() {
            format!("Authentication for {host}")
        } else {
            prompt.name.trim().to_string()
        };
        let subtitle = if prompt.instructions.trim().is_empty() {
            format!("{username}@{host} asks for another sign-in step.")
        } else {
            prompt.instructions.trim().to_string()
        };

        // Fixed width, not max_w: same wrapped-text problem as the host-key
        // card above.
        let mut card = v_flex()
            .px(px(12.))
            .py(px(10.))
            .w(px(380.))
            .bg(flint_theme::pop_surface())
            .rounded(px(6.))
            .border_1()
            .border_color(flint_theme::border_focused())
            .shadow_lg()
            .gap(px(8.))
            .child(
                div()
                    .text_size(px(13.))
                    .font_semibold()
                    .text_color(theme.foreground)
                    .child(title),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .child(subtitle),
            );

        // Title and instructions come from the server, and a bastion's
        // challenge reads exactly like the target's. Say which machine is
        // asking before the user types a one-time code into it.
        if hop.is_jump {
            card = card.child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.warning)
                    .child(format!(
                        "Asked by jump host {} ({}@{}) on the way to {}.",
                        hop.name,
                        username,
                        host,
                        target.display_name(),
                    )),
            );
        }

        for (ix, field) in prompt.fields.iter().enumerate() {
            let label = field.text.trim().trim_end_matches(':').to_string();
            let mut row = v_flex().gap_1();
            if !label.is_empty() {
                row = row.child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.muted_foreground)
                        .child(label),
                );
            }
            if let Some(input) = prompt.inputs.get(ix) {
                row = row.child(Input::new(input).small());
            }
            card = card.child(row);
        }

        card = card.child(
            h_flex()
                .gap_2()
                .justify_end()
                .child(
                    Button::new("auth-prompt-cancel")
                        .small()
                        .ghost()
                        .child("Cancel")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.respond_auth_prompt(link, false, cx);
                        })),
                )
                .child(
                    Button::new("auth-prompt-submit")
                        .small()
                        .primary()
                        .child("Continue")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.respond_auth_prompt(link, true, cx);
                        })),
                ),
        );

        Some(
            div()
                .absolute()
                .inset_0()
                // The working area is a rounded card; gpui does not clip to its
                // corners, so the overlay rounds its own top ones.
                .rounded_t(px(crate::ui::PANEL_RADIUS - 1.))
                .bg(theme.overlay)
                .flex()
                .items_center()
                .justify_center()
                .on_mouse_down(MouseButton::Left, |_, _, _| {})
                .on_mouse_down(MouseButton::Right, |_, _, _| {})
                .child(card)
                .into_any_element(),
        )
    }
}

/// Put a new password on one hop of the route. Returns false when the hop is
/// not on it.
pub(crate) fn set_hop_password(
    route: &mut SshRoute,
    entry_id: &str,
    password: &Zeroizing<String>,
) -> bool {
    let Some(hop) = route.hops_mut().find(|hop| hop.entry_id == entry_id) else {
        return false;
    };
    hop.credential = AuthCredential::Password(password.clone());
    true
}

/// Two specs describe the same tunnel: only the display name or the
/// auto-start flag may differ.
fn is_same_route(a: &ForwardSpec, b: &ForwardSpec) -> bool {
    a.id == b.id
        && a.kind == b.kind
        && a.bind_address == b.bind_address
        && a.bind_port == b.bind_port
        && a.target_host == b.target_host
        && a.target_port == b.target_port
}

const HISTORY_CAP: usize = 8;

/// How long the OS probe and the history fetch may each take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// How many times to silently retry a dropped connection before giving up and
/// showing the permanent "connection lost" modal.
const MAX_RECONNECT_ATTEMPTS: u32 = 5;

impl Render for ServerSession {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_auth_prompt_inputs(window, cx);
        self.ensure_password_input(window, cx);
        self.focus_terminal_if_pending(window, cx);
        let failure_card = self.render_connect_failure(cx);
        let theme = cx.theme();

        let content: AnyElement = if let (Some(browser), Some(terminal_view)) =
            (&self.browser, self.active_terminal())
        {
            let term_header = h_flex()
                .h(px(38.))
                .flex_none()
                .min_w_0()
                .px(px(8.))
                .items_center()
                .child(self.render_terminal_tabs(cx));

            let terminal_block = v_flex()
                .size_full()
                .overflow_hidden()
                .child(term_header)
                .child(
                    div()
                        .flex_1()
                        .overflow_hidden()
                        .child(terminal_view.clone()),
                );
            // Cached: terminal output redraws the session many times a second,
            // and the file panes have not changed.
            let files_block = div().size_full().overflow_hidden().child(
                browser
                    .clone()
                    .cached(StyleRefinement::default().size_full()),
            );

            // The initial sizes only set the ratio: the panels are scaled to
            // fill whatever room there is. With the file panes hidden the
            // terminal is the whole tab.
            let panes = if self.is_sftp_hidden {
                terminal_block.into_any_element()
            } else {
                match Preferences::terminal_position(cx) {
                    TerminalPosition::Below => v_resizable("sftp-terminal-split")
                        .with_state(&self.split_state)
                        .child(
                            resizable_panel()
                                .size(px(400.))
                                .size_range(px(120.)..Pixels::MAX)
                                .child(files_block),
                        )
                        .child(
                            resizable_panel()
                                .size(px(400.))
                                .size_range(px(120.)..Pixels::MAX)
                                .child(terminal_block),
                        )
                        .into_any_element(),
                    // The terminal gets the larger part: it wants long lines.
                    TerminalPosition::Beside => h_resizable("terminal-sftp-split")
                        .with_state(&self.split_state_beside)
                        .child(
                            resizable_panel()
                                .size(px(600.))
                                .size_range(px(240.)..Pixels::MAX)
                                .child(terminal_block),
                        )
                        .child(
                            resizable_panel()
                                .size(px(400.))
                                .size_range(px(240.)..Pixels::MAX)
                                .child(files_block),
                        )
                        .into_any_element(),
                }
            };
            let split = v_flex()
                .size_full()
                .child(self.render_session_header(cx))
                .child(div().flex_1().min_h_0().overflow_hidden().child(panes));

            if let Some(ref info) = self.disconnected {
                let host = info.host.clone();
                let reason = info.reason.clone();
                let preserved_count = info.preserved.count;
                let cache_path = info.preserved.cache_root.to_string_lossy().into_owned();
                div()
                    .relative()
                    .size_full()
                    .child(split)
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            // The working area is a rounded card; gpui does not clip to its
                            // corners, so the overlay rounds its own top ones.
                            .rounded_t(px(crate::ui::PANEL_RADIUS - 1.))
                            .bg(theme.overlay)
                            .flex()
                            .items_center()
                            .justify_center()
                            .on_mouse_down(MouseButton::Left, |_, _, _| {})
                            .on_mouse_down(MouseButton::Right, |_, _, _| {})
                            .child(
                                v_flex()
                                    .px(px(12.))
                                    .py(px(10.))
                                    .w(px(380.))
                                    .bg(flint_theme::pop_surface())
                                    .rounded(px(6.))
                                    .border_1()
                                    .border_color(flint_theme::border_focused())
                                    .shadow_lg()
                                    .gap(px(8.))
                                    .child(
                                        div()
                                            .text_color(theme.danger)
                                            .text_size(px(13.))
                                            .font_semibold()
                                            .child(format!("Connection to {} lost", host)),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(12.))
                                            .text_color(theme.muted_foreground)
                                            .child(reason),
                                    )
                                    .when(preserved_count > 0, |el| {
                                        el.child(
                                            div()
                                                .text_size(px(12.))
                                                .text_color(theme.warning)
                                                .child(preserved_files_note(
                                                    preserved_count,
                                                    &cache_path,
                                                )),
                                        )
                                    })
                                    .child(
                                        h_flex()
                                            .justify_end()
                                            .gap_2()
                                            .child(
                                                Button::new("close-disconnect")
                                                    .small()
                                                    .ghost()
                                                    .child("Close")
                                                    .on_click(cx.listener(|_, _, _, cx| {
                                                        cx.emit(ServerSessionEvent::CloseRequested);
                                                    })),
                                            )
                                            .child(
                                                Button::new("reconnect-disconnect")
                                                    .small()
                                                    .primary()
                                                    .child("Reconnect")
                                                    .on_click(cx.listener(|_, _, _, cx| {
                                                        cx.emit(
                                                            ServerSessionEvent::ReconnectRequested,
                                                        );
                                                    })),
                                            ),
                                    ),
                            ),
                    )
                    .into_any_element()
            } else if let Some(attempt) = self.reconnecting {
                // Transient drop: dim the (frozen) content and show a spinner while
                // the reconnect loop retries in the background.
                let host = self.conn_info.display_name().to_string();
                div()
                    .relative()
                    .size_full()
                    .child(split)
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            // The working area is a rounded card; gpui does not clip to its
                            // corners, so the overlay rounds its own top ones.
                            .rounded_t(px(crate::ui::PANEL_RADIUS - 1.))
                            .bg(theme.overlay)
                            .flex()
                            .items_center()
                            .justify_center()
                            .on_mouse_down(MouseButton::Left, |_, _, _| {})
                            .on_mouse_down(MouseButton::Right, |_, _, _| {})
                            .child(
                                v_flex()
                                    .px(px(12.))
                                    .py(px(10.))
                                    .w(px(380.))
                                    .bg(flint_theme::pop_surface())
                                    .rounded(px(6.))
                                    .border_1()
                                    .border_color(flint_theme::border_focused())
                                    .shadow_lg()
                                    .gap(px(8.))
                                    .child(
                                        h_flex()
                                            .gap(px(8.))
                                            .items_center()
                                            .child(Spinner::new())
                                            .child(
                                                div()
                                                    .text_color(theme.foreground)
                                                    .text_size(px(13.))
                                                    .font_semibold()
                                                    .child(format!("Reconnecting to {host}…")),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(12.))
                                            .text_color(theme.muted_foreground)
                                            .child(format!(
                                                "Attempt {attempt} of {MAX_RECONNECT_ATTEMPTS}"
                                            )),
                                    ),
                            ),
                    )
                    .into_any_element()
            } else {
                split.into_any_element()
            }
        } else if let Some(card) = failure_card {
            card
        } else {
            v_flex()
                .size_full()
                .gap(px(8.))
                .items_center()
                .justify_center()
                .child(Spinner::new().large())
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(theme.muted_foreground)
                        .child(match self.reconnecting {
                            Some(n) => {
                                format!("Reconnecting… (attempt {n} of {MAX_RECONNECT_ATTEMPTS})")
                            }
                            None => "Connecting…".to_string(),
                        }),
                )
                .child(
                    Button::new("cancel-connect")
                        .small()
                        .ghost()
                        .child("Cancel")
                        .on_click(cx.listener(|_, _, _, cx| {
                            cx.emit(ServerSessionEvent::CloseRequested);
                        })),
                )
                .into_any_element()
        };

        div()
            .relative()
            .size_full()
            .child(content)
            .when_some(
                self.render_host_key_prompt(Link::Session, cx),
                |el, prompt| el.child(prompt),
            )
            .when_some(self.render_auth_prompt(Link::Session, cx), |el, prompt| {
                el.child(prompt)
            })
            .when_some(self.render_host_key_prompt(Link::Peer, cx), |el, prompt| {
                el.child(prompt)
            })
            .when_some(self.render_auth_prompt(Link::Peer, cx), |el, prompt| {
                el.child(prompt)
            })
            .when_some(self.render_tasks_popover(cx), |el, pop| el.child(pop))
            .when_some(self.render_forwards_popover(cx), |el, pop| el.child(pop))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hop(entry_id: &str) -> crate::ssh::SshHop {
        crate::ssh::SshHop {
            entry_id: entry_id.to_string(),
            label: entry_id.to_string(),
            host: format!("{entry_id}.internal"),
            port: 22,
            username: "deploy".to_string(),
            credential: AuthCredential::Agent,
            expected_host_key: None,
        }
    }

    fn identity(is_jump: bool) -> HopIdentity {
        HopIdentity {
            name: "bastion".to_string(),
            host: "box".to_string(),
            username: "deploy".to_string(),
            is_jump,
        }
    }

    #[test]
    fn failure_message_drops_the_prefix_the_title_already_shows() {
        let error = AuthError::ConnectionFailed("connection refused".to_string());
        assert_eq!(
            failure_message(&error, &identity(false)),
            "Connection refused"
        );
    }

    #[test]
    fn failure_message_says_whose_password_was_rejected() {
        assert_eq!(
            failure_message(&AuthError::WrongPassword, &identity(false)),
            "Password rejected for deploy@box"
        );
    }

    #[test]
    fn failure_message_names_the_jump_host() {
        assert_eq!(
            failure_message(&AuthError::AgentNoKeys, &identity(true)),
            "Jump host bastion: The SSH agent holds no keys"
        );
    }

    #[test]
    fn connect_time_is_shown_only_for_an_unattended_first_connect() {
        let elapsed = Duration::from_millis(380);
        assert_eq!(shown_connect_time(elapsed, false, false), Some(elapsed));
        assert_eq!(shown_connect_time(elapsed, true, false), None);
        assert_eq!(shown_connect_time(elapsed, false, true), None);
    }

    #[test]
    fn connect_time_message_uses_milliseconds_then_seconds() {
        assert_eq!(
            connect_time_message("prod-db", Duration::from_millis(380)),
            "Connected to prod-db in 380 ms"
        );
        assert_eq!(
            connect_time_message("prod-db", Duration::from_millis(9_999)),
            "Connected to prod-db in 9999 ms"
        );
        assert_eq!(
            connect_time_message("prod-db", Duration::from_millis(12_340)),
            "Connected to prod-db in 12.3 s"
        );
    }

    #[test]
    fn preserved_files_note_counts_one_file_and_many() {
        assert_eq!(
            preserved_files_note(1, "/cache/prod"),
            "1 file with unsaved changes kept in /cache/prod"
        );
        assert_eq!(
            preserved_files_note(3, "/cache/prod"),
            "3 files with unsaved changes kept in /cache/prod"
        );
    }

    #[test]
    fn finish_within_gives_the_result_or_none_when_the_limit_ends_first() {
        use std::future::{pending, ready};

        let finished = futures::executor::block_on(finish_within(ready(7), pending::<()>()));
        assert_eq!(finished, Some(7));

        let timed_out = futures::executor::block_on(finish_within(pending::<u8>(), ready(())));
        assert_eq!(timed_out, None);
    }

    #[test]
    fn capitalize_first_handles_empty_and_non_ascii() {
        assert_eq!(capitalize_first(""), "");
        assert_eq!(capitalize_first("échec"), "Échec");
    }

    #[test]
    fn set_hop_password_changes_only_the_named_hop() {
        let mut route = SshRoute {
            jumps: vec![hop("bastion")],
            target: hop("db"),
            proxy: None,
        };
        let password = Zeroizing::new("s3cret".to_string());

        assert!(set_hop_password(&mut route, "bastion", &password));
        assert!(!set_hop_password(&mut route, "elsewhere", &password));

        assert!(matches!(
            &route.jumps[0].credential,
            AuthCredential::Password(p) if p.as_str() == "s3cret"
        ));
        assert!(matches!(route.target.credential, AuthCredential::Agent));
    }
}
