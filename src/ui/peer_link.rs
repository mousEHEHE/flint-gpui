//! The left file pane's own server: a second, SFTP-only connection a session
//! holds so its browser can show another saved host instead of this computer.
//!
//! These are `impl ServerSession` helpers split out of `server_session.rs`,
//! like `tasks_popover.rs`. The connection is never shared with that host's
//! own tab: a tab replaces its connection on every reconnect, and two readers
//! of one event channel would steal each other's events.

use std::sync::{Arc, Mutex};

use gpui::{AsyncApp, Context, Task, WeakEntity};
use zeroize::Zeroizing;

use crate::connection::Connection;
use crate::ssh::{AuthError, SftpHandle, SshConnection, SshConnectionEvent, SshRoute};
use crate::ui::server_session::{
    failure_message, set_hop_password, AuthPromptState, ConnectFailure, HostKeyPromptState,
    ServerSession, ServerSessionEvent, CONNECTION_CLOSED,
};
use crate::ui::sftp_browser::LeftServerCarry;

/// Which of a session's two connections a prompt or a hop belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Link {
    /// The tab's own server.
    Session,
    /// The server shown in the left file pane.
    Peer,
}

pub(crate) struct PeerLink {
    pub(crate) info: Connection,
    /// Kept so Retry and Reconnect work without the vault, and so a host key
    /// accepted on the first try is not asked about again.
    pub(crate) route: Arc<Mutex<SshRoute>>,
    /// Why the link is down. `None` while it connects or is up.
    failure: Option<ConnectFailure>,
    was_connected: bool,
    // Apart from the session's own prompts: its reconnect loop clears those.
    pub(crate) host_key_prompt: Option<HostKeyPromptState>,
    pub(crate) auth_prompt: Option<AuthPromptState>,
    /// Follows the connection. Dropping the link ends the task and, with it,
    /// the last handle to the connection.
    _task: Task<()>,
}

impl PeerLink {
    /// The server is connected and its files are in the left pane.
    fn is_up(&self) -> bool {
        self.was_connected && self.failure.is_none()
    }
}

/// Open the two SFTP channels of a left server. Listing works without the
/// second one, only slower.
async fn open_peer_sftp(
    conn: &SshConnection,
) -> Result<(SftpHandle, Option<SftpHandle>, String), String> {
    let (primary, home_dir) = conn
        .open_sftp()
        .await
        .map_err(|e| format!("Could not open SFTP: {e}"))?;
    let prefetch = match conn.open_sftp().await {
        Ok((handle, _)) => Some(handle),
        Err(e) => {
            tracing::warn!(
                target: "flint::sftp",
                "left server: prefetch channel open failed, prefetch disabled: {e}"
            );
            None
        }
    };
    Ok((primary, prefetch, home_dir))
}

/// Follow the left server's connection until it ends. No shell, forwards, OS
/// detection or history: the pane only needs SFTP.
async fn drive_peer(conn: SshConnection, this: &WeakEntity<ServerSession>, cx: &mut AsyncApp) {
    let events = conn.subscribe_events();
    while let Ok(event) = events.recv().await {
        let handled = match event {
            SshConnectionEvent::Connected => {
                let opened = open_peer_sftp(&conn).await;
                let attached = this.update(cx, |session, cx| session.peer_connected(opened, cx));
                // Without SFTP the connection is of no use; returning closes it.
                if !matches!(attached, Ok(true)) {
                    return;
                }
                Ok(())
            }
            SshConnectionEvent::AuthFailed { entry_id, error } => {
                tracing::warn!(target: "flint::ssh", %error, "left server: connect failed");
                this.update(cx, |session, cx| {
                    let hop = session.hop_identity(Link::Peer, &entry_id);
                    let failure = ConnectFailure {
                        message: failure_message(&error, &hop),
                        is_password_rejected: matches!(error, AuthError::WrongPassword),
                        is_transient: matches!(error, AuthError::ConnectionFailed(_)),
                        entry_id,
                    };
                    session.peer_ended(Some(failure), cx);
                })
                .ok();
                return;
            }
            SshConnectionEvent::HostKeyPrompt(prompt) => this.update(cx, |session, cx| {
                if let Some(peer) = &mut session.peer {
                    peer.host_key_prompt = Some(HostKeyPromptState::new(prompt));
                }
                cx.notify();
            }),
            SshConnectionEvent::HostKeyTrusted {
                entry_id,
                fingerprint,
            } => this.update(cx, |session, cx| {
                session.trust_host_key(Link::Peer, &entry_id, &fingerprint);
                cx.emit(ServerSessionEvent::HostKeyAccepted {
                    entry_id,
                    fingerprint,
                });
            }),
            SshConnectionEvent::AuthPrompt(prompt) => this.update(cx, |session, cx| {
                if let Some(peer) = &mut session.peer {
                    peer.auth_prompt = Some(AuthPromptState::new(prompt));
                }
                cx.notify();
            }),
            // Nothing starts a forward on this connection.
            SshConnectionEvent::ForwardChanged { .. } => Ok(()),
            SshConnectionEvent::Disconnected => break,
        };
        if handled.is_err() {
            return;
        }
    }
    this.update(cx, |session, cx| session.peer_ended(None, cx))
        .ok();
}

impl ServerSession {
    /// Show `info`'s files in the left pane, over a connection of its own.
    pub fn connect_peer(&mut self, info: Connection, route: SshRoute, cx: &mut Context<Self>) {
        self.spawn_peer(info, Arc::new(Mutex::new(route)), cx);
    }

    fn spawn_peer(
        &mut self,
        info: Connection,
        route: Arc<Mutex<SshRoute>>,
        cx: &mut Context<Self>,
    ) {
        tracing::info!(
            target: "flint::ssh",
            host = %info.display_name(),
            session = %self.conn_info.display_name(),
            "connecting the left pane to another server",
        );
        let conn = SshConnection::spawn(route.lock().expect("route mutex").clone());
        let task = cx.spawn(async move |this, cx| drive_peer(conn, &this, cx).await);
        // Replacing the link drops the old task, which closes its connection.
        self.peer = Some(PeerLink {
            info,
            route,
            failure: None,
            was_connected: false,
            host_key_prompt: None,
            auth_prompt: None,
            _task: task,
        });
        self.show_peer_in_browser(cx);
        cx.notify();
    }

    /// The left server logged in. `opened` is the outcome of opening SFTP on
    /// it; a failure ends the link. Returns whether its files are now in the
    /// left pane, at its home folder.
    fn peer_connected(
        &mut self,
        opened: Result<(SftpHandle, Option<SftpHandle>, String), String>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(peer) = &mut self.peer else {
            return false;
        };
        let (primary, prefetch, home_dir) = match opened {
            Ok(opened) => opened,
            Err(message) => {
                let failure = ConnectFailure {
                    entry_id: peer.info.id.clone(),
                    message,
                    is_password_rejected: false,
                    is_transient: true,
                };
                self.peer_ended(Some(failure), cx);
                return false;
            }
        };
        let Some(browser) = &self.browser else {
            return false;
        };
        peer.was_connected = true;
        let info = peer.info.clone();
        tracing::info!(target: "flint::ssh", host = %info.display_name(), "left server ready");
        browser.update(cx, |browser, cx| {
            browser.attach_peer(info, primary, prefetch, home_dir, cx);
        });
        true
    }

    /// The session reconnected and built a new browser. Make its left pane
    /// match the link again. `carried` is the pane of a server that is up,
    /// taken from the old browser; it goes on in the new one as it was.
    pub(crate) fn restore_peer(
        &mut self,
        carried: Option<LeftServerCarry>,
        cx: &mut Context<Self>,
    ) {
        let (Some(carried), Some(browser)) = (carried, &self.browser) else {
            // This computer, or a server that is connecting or has failed.
            self.show_peer_in_browser(cx);
            return;
        };
        tracing::info!(
            target: "flint::ssh",
            host = %self.conn_info.display_name(),
            "left server kept across the reconnect",
        );
        browser.update(cx, |browser, cx| browser.adopt_left_server(carried, cx));
    }

    /// The left server's connection is over. `None` is a plain drop.
    fn peer_ended(&mut self, failure: Option<ConnectFailure>, cx: &mut Context<Self>) {
        let Some(peer) = &mut self.peer else { return };
        let failure = failure.unwrap_or_else(|| ConnectFailure {
            entry_id: peer.info.id.clone(),
            message: CONNECTION_CLOSED.to_string(),
            is_password_rejected: false,
            is_transient: true,
        });
        tracing::warn!(
            target: "flint::ssh",
            host = %peer.info.display_name(),
            reason = %failure.message,
            "left server connection ended",
        );
        // Nothing is left to answer these prompts.
        peer.host_key_prompt = None;
        peer.auth_prompt = None;
        peer.failure = Some(failure);
        self.show_peer_in_browser(cx);
        cx.notify();
    }

    /// Connect to the left server again over the route it was opened with.
    /// A password typed for the hop that rejected one is used for this link
    /// only and is never saved.
    pub(crate) fn retry_peer(
        &mut self,
        password: Option<Zeroizing<String>>,
        cx: &mut Context<Self>,
    ) {
        let Some(peer) = self.peer.take() else { return };
        if let (Some(password), Some(failure)) = (&password, &peer.failure) {
            let mut route = peer.route.lock().expect("route mutex");
            if !set_hop_password(&mut route, &failure.entry_id, password) {
                tracing::warn!(
                    entry_id = failure.entry_id,
                    "password typed for a hop that is not on the left server's route"
                );
            }
        }
        self.spawn_peer(peer.info, peer.route, cx);
    }

    /// The user asked for this computer in the left pane.
    pub fn use_local_left(&mut self, cx: &mut Context<Self>) {
        // Only a server that shows its files has something to lose. One that
        // is connecting or has failed can go at any time, also while the
        // session itself is reconnecting.
        let is_showing_files = self.peer.as_ref().is_some_and(PeerLink::is_up);
        if let Some(refusal) = self.left_switch_refusal(cx).filter(|_| is_showing_files) {
            cx.emit(ServerSessionEvent::StatusChanged(refusal));
            return;
        }
        self.drop_peer(cx);
    }

    /// The host was removed from the vault: stop showing it in the left pane.
    pub fn drop_peer_for(&mut self, entry_id: &str, cx: &mut Context<Self>) {
        if self
            .peer
            .as_ref()
            .is_some_and(|peer| peer.info.id == entry_id)
        {
            self.drop_peer(cx);
        }
    }

    /// Close the left server's connection and show this computer again.
    pub(crate) fn drop_peer(&mut self, cx: &mut Context<Self>) {
        if self.peer.take().is_none() {
            return;
        }
        self.show_peer_in_browser(cx);
        cx.notify();
    }

    /// Make the browser's left pane match the link: this computer, a server
    /// that is connecting, or one that failed. A server that is up shows as
    /// connecting until `peer_connected` attaches it with its SFTP channels.
    fn show_peer_in_browser(&self, cx: &mut Context<Self>) {
        let Some(browser) = &self.browser else { return };
        browser.update(cx, |browser, cx| {
            let Some(peer) = &self.peer else {
                browser.show_local(cx);
                return;
            };
            let info = peer.info.clone();
            match &peer.failure {
                Some(failure) => browser.show_peer_failed(
                    info,
                    peer.was_connected,
                    failure.message.clone(),
                    failure.is_password_rejected,
                    cx,
                ),
                None => browser.show_peer_connecting(info, cx),
            }
        });
    }

    /// The vault id of the server the left pane shows, when it shows one
    /// and the link to it is not broken.
    pub fn left_source_entry(&self) -> Option<&str> {
        let peer = self.peer.as_ref().filter(|peer| peer.failure.is_none())?;
        Some(peer.info.id.as_str())
    }

    /// Whether the left pane already shows this source and it is not broken:
    /// `None` is this computer, `Some` a saved server's vault id.
    pub fn is_left_source(&self, entry_id: Option<&str>) -> bool {
        match (&self.peer, entry_id) {
            (None, None) => true,
            (Some(peer), Some(entry_id)) => peer.info.id == entry_id && peer.failure.is_none(),
            _ => false,
        }
    }

    /// Why the left pane cannot change what it shows right now, if it cannot.
    pub fn left_switch_refusal(&self, cx: &gpui::App) -> Option<String> {
        let browser = self
            .browser
            .as_ref()
            .filter(|_| self.disconnected.is_none() && self.reconnecting.is_none());
        let Some(browser) = browser else {
            return Some("This session is not connected.".to_string());
        };
        browser.read(cx).left_busy_reason()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh::{AuthCredential, SshHop};

    fn hop(entry_id: &str) -> SshHop {
        SshHop {
            entry_id: entry_id.to_string(),
            label: entry_id.to_string(),
            host: format!("{entry_id}.internal"),
            port: 22,
            username: "deploy".to_string(),
            credential: AuthCredential::Password(Zeroizing::new("old".to_string())),
            expected_host_key: None,
        }
    }

    fn password_of(hop: &SshHop) -> &str {
        match &hop.credential {
            AuthCredential::Password(password) => password.as_str(),
            _ => panic!("the hop has no password"),
        }
    }

    fn link(was_connected: bool, failure: Option<&str>) -> PeerLink {
        PeerLink {
            info: Connection {
                id: "web-02".to_string(),
                name: "web-02".to_string(),
                host: "web-02.internal".to_string(),
                port: 22,
                username: "deploy".to_string(),
            },
            route: Arc::new(Mutex::new(SshRoute {
                jumps: Vec::new(),
                target: hop("web-02"),
                proxy: None,
            })),
            failure: failure.map(|message| ConnectFailure {
                entry_id: "web-02".to_string(),
                message: message.to_string(),
                is_password_rejected: false,
                is_transient: true,
            }),
            was_connected,
            host_key_prompt: None,
            auth_prompt: None,
            _task: Task::ready(()),
        }
    }

    #[test]
    fn a_link_is_up_only_between_login_and_the_end_of_its_connection() {
        assert!(!link(false, None).is_up());
        assert!(link(true, None).is_up());
        assert!(!link(true, Some(CONNECTION_CLOSED)).is_up());
        assert!(!link(false, Some("Connection refused")).is_up());
    }

    #[test]
    fn retry_password_goes_to_the_rejecting_hop_of_the_peer_route() {
        let route = Arc::new(Mutex::new(SshRoute {
            jumps: vec![hop("bastion")],
            target: hop("web-02"),
            proxy: None,
        }));
        let typed = Zeroizing::new("typed".to_string());

        // The link keeps the route behind a mutex and retries from it.
        assert!(set_hop_password(
            &mut route.lock().unwrap(),
            "web-02",
            &typed
        ));
        assert!(!set_hop_password(
            &mut route.lock().unwrap(),
            "web-01",
            &typed
        ));

        let retried = route.lock().unwrap().clone();
        assert_eq!(password_of(&retried.target), "typed");
        assert_eq!(password_of(&retried.jumps[0]), "old");
    }
}
