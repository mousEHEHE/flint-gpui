//! The sync server's HTTP API. Blocking: call it from a worker thread.

use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::vault::format::{AeadBlob, KdfParams};
use crate::vault::sync::{OutgoingChange, PulledItem};

const DEFAULT_URL: &str = "https://flint.mouse.pw:8443/api/v1";
/// Points the app at another server, for development.
const URL_VARIABLE: &str = "FLINT_API_URL";

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("could not reach the sync server: {0}")]
    Network(String),
    #[error("{message}")]
    Refused {
        status: u16,
        code: String,
        message: String,
    },
    #[error("the sync server sent an answer flint does not understand")]
    BadAnswer,
}

impl ApiError {
    pub fn code(&self) -> Option<&str> {
        match self {
            ApiError::Refused { code, .. } => Some(code),
            _ => None,
        }
    }

    /// The session is gone: signed out from another device, or the password
    /// was changed there.
    pub fn is_session_ended(&self) -> bool {
        self.code() == Some("unauthorized")
    }
}

pub struct Api {
    agent: ureq::Agent,
    base_url: String,
}

#[derive(Deserialize)]
pub struct ServerConfig {
    pub github_client_id: String,
}

/// The answer to a GitHub sign-in: who the user is, and for an existing
/// account how its password turns into a key. No data yet.
pub struct GitHubStep {
    pub ticket: String,
    pub is_new: bool,
    pub github_login: String,
    pub kdf: Option<([u8; 32], KdfParams)>,
}

pub struct Session {
    pub session_token: String,
    pub user_id: Uuid,
    /// The account's own vault.
    pub vault_id: Uuid,
}

pub struct LoginAnswer {
    pub session: Session,
    pub enc_dek_by_password: AeadBlob,
    pub enc_dek_by_recovery: AeadBlob,
}

pub struct NewAccount<'a> {
    pub ticket: &'a str,
    pub kdf_salt: &'a [u8; 32],
    pub kdf_params: &'a KdfParams,
    pub auth_key: &'a [u8; 32],
    pub recovery_auth_key: &'a [u8; 32],
    pub enc_dek_by_password: &'a AeadBlob,
    pub enc_dek_by_recovery: &'a AeadBlob,
}

pub struct NewPassword<'a> {
    pub kdf_salt: &'a [u8; 32],
    pub kdf_params: &'a KdfParams,
    pub auth_key: &'a [u8; 32],
    pub enc_dek_by_password: &'a AeadBlob,
}

impl NewPassword<'_> {
    fn to_json(&self) -> Value {
        json!({
            "kdf_salt": BASE64.encode(self.kdf_salt),
            "kdf_params": kdf_json(self.kdf_params),
            "auth_key": BASE64.encode(self.auth_key),
            "enc_dek_by_password": BASE64.encode(self.enc_dek_by_password.to_bytes()),
        })
    }
}

pub struct PulledPage {
    pub items: Vec<PulledItem>,
    pub has_more: bool,
}

pub struct PushAnswer {
    /// Item id and the revision the server gave it.
    pub applied: Vec<(Uuid, u64)>,
    /// The server's version of each item it refused a change to.
    pub conflicts: Vec<PulledItem>,
}

/// Where the server's API lives, without a slash at the end.
pub fn base_url() -> String {
    std::env::var(URL_VARIABLE).unwrap_or_else(|_| DEFAULT_URL.to_string())
}

impl Api {
    pub fn new() -> Self {
        let base_url = base_url();
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .user_agent(&format!("Flint/{}", crate::version::CURRENT))
            .build();
        Self { agent, base_url }
    }

    pub fn config(&self) -> Result<ServerConfig, ApiError> {
        self.get("/config", None)
    }

    pub fn sign_in_with_github(
        &self,
        code: &str,
        code_verifier: &str,
        redirect_uri: &str,
    ) -> Result<GitHubStep, ApiError> {
        #[derive(Deserialize)]
        struct Answer {
            ticket: String,
            is_new: bool,
            github_login: String,
            kdf_salt: Option<String>,
            kdf_params: Option<KdfParams>,
        }
        let answer: Answer = self.post(
            "/auth/github",
            None,
            json!({ "code": code, "code_verifier": code_verifier, "redirect_uri": redirect_uri }),
        )?;
        let kdf = match (answer.kdf_salt, answer.kdf_params) {
            (Some(salt), Some(params)) => Some((key_from_base64(&salt)?, params)),
            _ => None,
        };
        // An existing account without its salt cannot be signed in to.
        if !answer.is_new && kdf.is_none() {
            return Err(ApiError::BadAnswer);
        }
        Ok(GitHubStep {
            ticket: answer.ticket,
            is_new: answer.is_new,
            github_login: answer.github_login,
            kdf,
        })
    }

    pub fn register(&self, account: &NewAccount) -> Result<Session, ApiError> {
        let (device_name, platform) = device();
        let answer: SessionAnswer = self.post(
            "/auth/register",
            None,
            json!({
                "ticket": account.ticket,
                "kdf_salt": BASE64.encode(account.kdf_salt),
                "kdf_params": kdf_json(account.kdf_params),
                "auth_key": BASE64.encode(account.auth_key),
                "recovery_auth_key": BASE64.encode(account.recovery_auth_key),
                "enc_dek_by_password": BASE64.encode(account.enc_dek_by_password.to_bytes()),
                "enc_dek_by_recovery": BASE64.encode(account.enc_dek_by_recovery.to_bytes()),
                "device_name": device_name,
                "platform": platform,
            }),
        )?;
        Ok(answer.into())
    }

    pub fn login(&self, ticket: &str, auth_key: &[u8; 32]) -> Result<LoginAnswer, ApiError> {
        #[derive(Deserialize)]
        struct Answer {
            session_token: String,
            user_id: Uuid,
            vault_id: Uuid,
            enc_dek_by_password: String,
            enc_dek_by_recovery: String,
        }
        let (device_name, platform) = device();
        let answer: Answer = self.post(
            "/auth/login",
            None,
            json!({
                "ticket": ticket,
                "auth_key": BASE64.encode(auth_key),
                "device_name": device_name,
                "platform": platform,
            }),
        )?;
        Ok(LoginAnswer {
            session: Session {
                session_token: answer.session_token,
                user_id: answer.user_id,
                vault_id: answer.vault_id,
            },
            enc_dek_by_password: blob_from_base64(&answer.enc_dek_by_password)?,
            enc_dek_by_recovery: blob_from_base64(&answer.enc_dek_by_recovery)?,
        })
    }

    pub fn logout(&self, token: &str) -> Result<(), ApiError> {
        self.send("POST", "/auth/logout", Some(token), None)
            .map(drop)
    }

    pub fn change_password(
        &self,
        token: &str,
        current_auth_key: &[u8; 32],
        new: &NewPassword,
    ) -> Result<(), ApiError> {
        let body = json!({
            "current_auth_key": BASE64.encode(current_auth_key),
            "new": new.to_json(),
        });
        self.send("POST", "/auth/password", Some(token), Some(&body))
            .map(drop)
    }

    /// Step one of telling the account about a recovery: prove the old phrase.
    /// Returns the ticket for [`Self::finish_recovery`].
    pub fn start_recovery(&self, recovery_auth_key: &[u8; 32]) -> Result<String, ApiError> {
        #[derive(Deserialize)]
        struct Answer {
            recovery_ticket: String,
        }
        let answer: Answer = self.post(
            "/auth/recover",
            None,
            json!({ "recovery_auth_key": BASE64.encode(recovery_auth_key) }),
        )?;
        Ok(answer.recovery_ticket)
    }

    pub fn finish_recovery(
        &self,
        recovery_ticket: &str,
        new: &NewPassword,
        recovery_auth_key: &[u8; 32],
        enc_dek_by_recovery: &AeadBlob,
    ) -> Result<Session, ApiError> {
        let (device_name, platform) = device();
        let answer: SessionAnswer = self.post(
            "/auth/recover/finish",
            None,
            json!({
                "recovery_ticket": recovery_ticket,
                "new": new.to_json(),
                "recovery_auth_key": BASE64.encode(recovery_auth_key),
                "enc_dek_by_recovery": BASE64.encode(enc_dek_by_recovery.to_bytes()),
                "device_name": device_name,
                "platform": platform,
            }),
        )?;
        Ok(answer.into())
    }

    pub fn pull(&self, token: &str, vault_id: Uuid, since: u64) -> Result<PulledPage, ApiError> {
        #[derive(Deserialize)]
        struct Answer {
            items: Vec<Item>,
            has_more: bool,
        }
        let answer: Answer = self.get(
            &format!("/vaults/{vault_id}/changes?since={since}"),
            Some(token),
        )?;
        Ok(PulledPage {
            items: pulled_items(answer.items)?,
            has_more: answer.has_more,
        })
    }

    pub fn push(
        &self,
        token: &str,
        vault_id: Uuid,
        key_version: u64,
        changes: &[OutgoingChange],
    ) -> Result<PushAnswer, ApiError> {
        #[derive(Deserialize)]
        struct Answer {
            applied: Vec<Applied>,
            conflicts: Vec<Item>,
        }
        #[derive(Deserialize)]
        struct Applied {
            id: Uuid,
            rev: u64,
        }
        let changes: Vec<Value> = changes
            .iter()
            .map(|change| {
                json!({
                    "id": change.id,
                    "base_rev": change.base_rev,
                    "key_version": key_version,
                    "is_deleted": change.is_deleted,
                    "blob": BASE64.encode(&change.blob),
                })
            })
            .collect();
        let answer: Answer = self.post(
            &format!("/vaults/{vault_id}/changes"),
            Some(token),
            json!({ "changes": changes }),
        )?;
        Ok(PushAnswer {
            applied: answer.applied.into_iter().map(|a| (a.id, a.rev)).collect(),
            conflicts: pulled_items(answer.conflicts)?,
        })
    }

    /// Send a message to the developer under the signed-in account.
    pub fn send_feedback(
        &self,
        token: &str,
        kind: FeedbackKind,
        message: &str,
    ) -> Result<(), ApiError> {
        let body = json!({
            "kind": kind.as_str(),
            "message": message,
            "app_version": crate::version::CURRENT,
            "platform": std::env::consts::OS,
        });
        self.send("POST", "/feedback", Some(token), Some(&body))
            .map(drop)
    }

    /// Every device signed in to the account.
    pub fn sessions(&self, token: &str) -> Result<Vec<DeviceSession>, ApiError> {
        self.get("/sessions", Some(token))
    }

    pub fn revoke_session(&self, token: &str, session_id: Uuid) -> Result<(), ApiError> {
        self.send(
            "DELETE",
            &format!("/sessions/{session_id}"),
            Some(token),
            None,
        )
        .map(drop)
    }

    /// Delete the account and everything the server holds for it.
    pub fn delete_account(&self, token: &str, auth_key: &[u8; 32]) -> Result<(), ApiError> {
        let body = json!({ "auth_key": BASE64.encode(auth_key) });
        self.send("DELETE", "/me", Some(token), Some(&body))
            .map(drop)
    }

    pub fn replace_recovery_phrase(
        &self,
        token: &str,
        auth_key: &[u8; 32],
        recovery_auth_key: &[u8; 32],
        enc_dek_by_recovery: &AeadBlob,
    ) -> Result<(), ApiError> {
        let body = json!({
            "auth_key": BASE64.encode(auth_key),
            "recovery_auth_key": BASE64.encode(recovery_auth_key),
            "enc_dek_by_recovery": BASE64.encode(enc_dek_by_recovery.to_bytes()),
        });
        self.send("POST", "/auth/recovery-phrase", Some(token), Some(&body))
            .map(drop)
    }

    /// The team key seed as the server keeps it, wrapped; `None` when the
    /// account has made no team keys yet.
    pub fn key_seed(&self, token: &str) -> Result<Option<Vec<u8>>, ApiError> {
        #[derive(Deserialize)]
        struct Me {
            enc_key_seed: Option<String>,
        }
        let me: Me = self.get("/me", Some(token))?;
        me.enc_key_seed
            .map(|seed| bytes_from_base64(&seed))
            .transpose()
    }

    pub fn set_keys(
        &self,
        token: &str,
        auth_key: &[u8; 32],
        sign_key: &[u8; 32],
        box_key: &[u8; 32],
        enc_key_seed: &[u8],
    ) -> Result<(), ApiError> {
        let body = json!({
            "auth_key": BASE64.encode(auth_key),
            "public_sign_key": BASE64.encode(sign_key),
            "public_box_key": BASE64.encode(box_key),
            "enc_key_seed": BASE64.encode(enc_key_seed),
        });
        self.send("PUT", "/me/keys", Some(token), Some(&body))
            .map(drop)
    }

    pub fn teams(&self, token: &str) -> Result<Vec<TeamSummary>, ApiError> {
        self.get("/teams", Some(token))
    }

    pub fn team(&self, token: &str, team_id: Uuid) -> Result<TeamDetails, ApiError> {
        self.get(&format!("/teams/{team_id}"), Some(token))
    }

    /// The whole signed chain of a team, oldest first.
    pub fn rosters(&self, token: &str, team_id: Uuid) -> Result<Vec<SignedRoster>, ApiError> {
        self.get(&format!("/teams/{team_id}/rosters"), Some(token))
    }

    pub fn create_team(
        &self,
        token: &str,
        name: &str,
        roster: &RosterSubmission,
    ) -> Result<(), ApiError> {
        let mut body = roster.to_json();
        body["name"] = json!(name);
        self.send("POST", "/teams", Some(token), Some(&body))
            .map(drop)
    }

    pub fn submit_roster(
        &self,
        token: &str,
        team_id: Uuid,
        roster: &RosterSubmission,
    ) -> Result<(), ApiError> {
        let path = format!("/teams/{team_id}/rosters");
        self.send("POST", &path, Some(token), Some(&roster.to_json()))
            .map(drop)
    }

    /// Register an invite by the hash of its token. Returns the invite's id.
    pub fn create_invite(
        &self,
        token: &str,
        team_id: Uuid,
        token_hash: &[u8; 32],
    ) -> Result<Uuid, ApiError> {
        #[derive(Deserialize)]
        struct Answer {
            id: Uuid,
        }
        let answer: Answer = self.post(
            &format!("/teams/{team_id}/invites"),
            Some(token),
            // Reusable: one link for everyone the owner sends it to.
            json!({ "token_hash": BASE64.encode(token_hash), "is_reusable": true }),
        )?;
        Ok(answer.id)
    }

    pub fn revoke_invite(
        &self,
        token: &str,
        team_id: Uuid,
        invite_id: Uuid,
    ) -> Result<(), ApiError> {
        let path = format!("/teams/{team_id}/invites/{invite_id}");
        self.send("DELETE", &path, Some(token), None).map(drop)
    }

    pub fn invites(&self, token: &str, team_id: Uuid) -> Result<Vec<Invite>, ApiError> {
        self.get(&format!("/teams/{team_id}/invites"), Some(token))
    }

    pub fn preview_invite(
        &self,
        token: &str,
        invite_token: &str,
    ) -> Result<InvitePreview, ApiError> {
        self.get(&format!("/invites/{invite_token}"), Some(token))
    }

    pub fn ask_to_join(
        &self,
        token: &str,
        invite_token: &str,
        mac: &[u8; 32],
    ) -> Result<(), ApiError> {
        let body = json!({ "mac": BASE64.encode(mac) });
        self.send(
            "POST",
            &format!("/invites/{invite_token}/join"),
            Some(token),
            Some(&body),
        )
        .map(drop)
    }

    pub fn leave_team(
        &self,
        token: &str,
        team_id: Uuid,
        auth_key: &[u8; 32],
    ) -> Result<(), ApiError> {
        let body = json!({ "auth_key": BASE64.encode(auth_key) });
        self.send(
            "DELETE",
            &format!("/teams/{team_id}/members/me"),
            Some(token),
            Some(&body),
        )
        .map(drop)
    }

    pub fn delete_team(
        &self,
        token: &str,
        team_id: Uuid,
        auth_key: &[u8; 32],
    ) -> Result<(), ApiError> {
        let body = json!({ "auth_key": BASE64.encode(auth_key) });
        self.send(
            "DELETE",
            &format!("/teams/{team_id}"),
            Some(token),
            Some(&body),
        )
        .map(drop)
    }

    fn get<T: DeserializeOwned>(&self, path: &str, token: Option<&str>) -> Result<T, ApiError> {
        self.send("GET", path, token, None)?
            .into_json()
            .map_err(|_| ApiError::BadAnswer)
    }

    fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        token: Option<&str>,
        body: Value,
    ) -> Result<T, ApiError> {
        self.send("POST", path, token, Some(&body))?
            .into_json()
            .map_err(|_| ApiError::BadAnswer)
    }

    fn send(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&Value>,
    ) -> Result<ureq::Response, ApiError> {
        let mut request = self
            .agent
            .request(method, &format!("{}{path}", self.base_url));
        if let Some(token) = token {
            request = request.set("Authorization", &format!("Bearer {token}"));
        }
        let answer = match body {
            Some(body) => request.send_json(body),
            None => request.call(),
        };
        match answer {
            Ok(response) => Ok(response),
            Err(ureq::Error::Status(status, response)) => Err(refusal(status, response)),
            Err(ureq::Error::Transport(transport)) => Err(ApiError::Network(transport.to_string())),
        }
    }
}

/// A vault item as the server sends it.
#[derive(Deserialize)]
struct Item {
    id: Uuid,
    rev: u64,
    key_version: u64,
    is_deleted: bool,
    blob: String,
}

fn pulled_items(items: Vec<Item>) -> Result<Vec<PulledItem>, ApiError> {
    items
        .into_iter()
        .map(|item| {
            Ok(PulledItem {
                id: item.id,
                rev: item.rev,
                key_version: item.key_version,
                is_deleted: item.is_deleted,
                blob: BASE64.decode(&item.blob).map_err(|_| ApiError::BadAnswer)?,
            })
        })
        .collect()
}

#[derive(Deserialize)]
struct SessionAnswer {
    session_token: String,
    user_id: Uuid,
    vault_id: Uuid,
}

impl From<SessionAnswer> for Session {
    fn from(answer: SessionAnswer) -> Self {
        Session {
            session_token: answer.session_token,
            user_id: answer.user_id,
            vault_id: answer.vault_id,
        }
    }
}

fn refusal(status: u16, response: ureq::Response) -> ApiError {
    #[derive(Deserialize)]
    struct Body {
        error: Detail,
    }
    #[derive(Deserialize)]
    struct Detail {
        code: String,
        message: String,
    }
    match response.into_json::<Body>() {
        Ok(body) => ApiError::Refused {
            status,
            code: body.error.code,
            message: body.error.message,
        },
        // Not from the API itself: the proxy in front of it, or a captive portal.
        Err(_) => ApiError::Refused {
            status,
            code: String::new(),
            message: format!("the sync server answered with status {status}"),
        },
    }
}

fn kdf_json(params: &KdfParams) -> Value {
    json!({ "m_cost": params.m_cost, "t_cost": params.t_cost, "p_cost": params.p_cost })
}

fn key_from_base64(text: &str) -> Result<[u8; 32], ApiError> {
    BASE64
        .decode(text)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(ApiError::BadAnswer)
}

fn blob_from_base64(text: &str) -> Result<AeadBlob, ApiError> {
    let bytes = BASE64.decode(text).map_err(|_| ApiError::BadAnswer)?;
    AeadBlob::from_bytes(&bytes).ok_or(ApiError::BadAnswer)
}

/// The name and platform shown in the account's list of signed-in devices.
fn device() -> (String, &'static str) {
    let name = host_name()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Flint".to_string());
    (name.chars().take(100).collect(), std::env::consts::OS)
}

#[cfg(unix)]
fn host_name() -> Option<String> {
    let mut buffer = [0u8; 256];
    let status = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if status != 0 {
        return None;
    }
    let length = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    String::from_utf8(buffer[..length].to_vec()).ok()
}

#[cfg(not(unix))]
fn host_name() -> Option<String> {
    std::env::var("COMPUTERNAME").ok()
}

fn bytes_from_base64(text: &str) -> Result<Vec<u8>, ApiError> {
    BASE64.decode(text).map_err(|_| ApiError::BadAnswer)
}

/// For fields the server sends as base64 text.
mod base64_bytes {
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        super::BASE64.decode(text).map_err(serde::de::Error::custom)
    }

    pub mod option {
        use super::*;

        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<Vec<u8>>, D::Error> {
            let text = Option::<String>::deserialize(deserializer)?;
            text.map(|text| {
                super::super::BASE64
                    .decode(text)
                    .map_err(serde::de::Error::custom)
            })
            .transpose()
        }
    }
}

/// What a message to the developer is about.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FeedbackKind {
    Bug,
    Idea,
    Other,
}

impl FeedbackKind {
    fn as_str(self) -> &'static str {
        match self {
            FeedbackKind::Bug => "bug",
            FeedbackKind::Idea => "idea",
            FeedbackKind::Other => "other",
        }
    }
}

#[derive(Clone, Deserialize)]
pub struct DeviceSession {
    pub id: Uuid,
    pub device_name: String,
    pub platform: String,
    /// An ISO 8601 timestamp, as the server sent it.
    pub last_seen_at: String,
    pub is_current: bool,
}

#[derive(Deserialize)]
pub struct TeamSummary {
    pub id: Uuid,
    pub name: String,
}

#[derive(Deserialize)]
pub struct TeamDetails {
    pub members: Vec<TeamMember>,
    pub my_vault_keys: Vec<WrappedVaultKey>,
    /// The inviter's proof of the roster that added this account.
    #[serde(deserialize_with = "base64_bytes::option::deserialize")]
    pub join_confirmation: Option<Vec<u8>>,
}

#[derive(Deserialize)]
pub struct TeamMember {
    pub user_id: Uuid,
    pub github_login: String,
}

#[derive(Deserialize)]
pub struct WrappedVaultKey {
    pub vault_id: Uuid,
    pub key_version: u64,
    #[serde(deserialize_with = "base64_bytes::deserialize")]
    pub wrapped_key: Vec<u8>,
}

#[derive(Deserialize)]
pub struct SignedRoster {
    #[serde(deserialize_with = "base64_bytes::deserialize")]
    pub body: Vec<u8>,
    #[serde(deserialize_with = "base64_bytes::deserialize")]
    pub signature: Vec<u8>,
    pub signer_user_id: Uuid,
}

#[derive(Deserialize)]
pub struct Invite {
    pub id: Uuid,
    /// On a request made through a link: the invite the link belongs to.
    pub parent_id: Option<Uuid>,
    pub join_request: Option<JoinRequest>,
}

#[derive(Clone, Deserialize)]
pub struct JoinRequest {
    pub user_id: Uuid,
    pub github_login: String,
    #[serde(deserialize_with = "base64_bytes::deserialize")]
    pub public_sign_key: Vec<u8>,
    #[serde(deserialize_with = "base64_bytes::deserialize")]
    pub public_box_key: Vec<u8>,
    #[serde(deserialize_with = "base64_bytes::deserialize")]
    pub mac: Vec<u8>,
}

#[derive(Deserialize)]
pub struct InvitePreview {
    pub team_id: Uuid,
    pub team_name: String,
}

/// A signed roster with everything the server applies together with it.
#[derive(Default)]
pub struct RosterSubmission {
    pub body: Vec<u8>,
    pub signature: Vec<u8>,
    /// Vault, member, and the vault key wrapped for that member.
    pub wraps: Vec<(Uuid, Uuid, Vec<u8>)>,
    pub rotations: Vec<Rotation>,
    /// New member and the proof they check the roster with.
    pub confirmations: Vec<(Uuid, [u8; 32])>,
}

/// Every item of a vault, encrypted again under its next key.
pub struct Rotation {
    pub vault_id: Uuid,
    pub base_rev: u64,
    pub items: Vec<(Uuid, Vec<u8>)>,
}

impl RosterSubmission {
    fn to_json(&self) -> Value {
        let wraps: Vec<Value> = self
            .wraps
            .iter()
            .map(|(vault_id, user_id, key)| {
                json!({ "vault_id": vault_id, "user_id": user_id, "wrapped_key": BASE64.encode(key) })
            })
            .collect();
        let rotations: Vec<Value> = self
            .rotations
            .iter()
            .map(|rotation| {
                let items: Vec<Value> = rotation
                    .items
                    .iter()
                    .map(|(id, blob)| json!({ "id": id, "blob": BASE64.encode(blob) }))
                    .collect();
                json!({ "vault_id": rotation.vault_id, "base_rev": rotation.base_rev, "items": items })
            })
            .collect();
        let confirmations: Vec<Value> = self
            .confirmations
            .iter()
            .map(|(user_id, proof)| json!({ "user_id": user_id, "proof": BASE64.encode(proof) }))
            .collect();
        json!({
            "body": BASE64.encode(&self.body),
            "signature": BASE64.encode(&self.signature),
            "wraps": wraps,
            "rotations": rotations,
            "confirmations": confirmations,
        })
    }
}
