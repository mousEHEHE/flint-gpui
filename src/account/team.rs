//! Teams: sharing hosts with other accounts without trusting the server.
//!
//! Who is in a team is written in its roster, a JSON document signed by the
//! owner or an admin. Each roster names the hash of the one before it, so a
//! device that trusts one roster can check every later one itself. A team
//! vault has its own key, and the roster says who may hold it; the key
//! reaches a member wrapped to their public key.

use base64::engine::general_purpose::{STANDARD as BASE64, URL_SAFE_NO_PAD as BASE64_URL};
use base64::Engine as _;
use ed25519_dalek::{Signer as _, SigningKey, VerifyingKey};
use rand::{rngs::OsRng, RngCore as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;
use uuid::Uuid;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

use super::api::{Api, ApiError, JoinRequest, RosterSubmission, Rotation, SignedRoster};
use crate::vault::crypto;
use crate::vault::format::AeadBlob;
use crate::vault::sync::{
    self, InviteSecret, PendingJoin, TeamContext, TeamPin, TeamState, TeamUpdate, TeamsUpdate,
    VaultKey,
};

const INVITE_PREFIX: &str = "flint://invite/";
/// The server's limits on a team's vaults.
const MAX_VAULTS: usize = 10;
const MAX_VAULT_NAME_CHARS: usize = 60;

#[derive(Debug, thiserror::Error)]
pub enum TeamError {
    #[error(transparent)]
    Api(#[from] ApiError),
    #[error("enter your master password to set up team keys")]
    PasswordNeeded,
    #[error("this is not a flint invite link")]
    BadLink,
    #[error("the team could not be verified: {0}")]
    Untrusted(&'static str),
    #[error("{0}")]
    Refused(&'static str),
}

/// The account's signing and encryption keys, both made from one seed.
pub struct TeamKeys {
    seed: Zeroizing<[u8; 32]>,
}

impl TeamKeys {
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self {
            seed: Zeroizing::new(seed),
        }
    }

    pub fn seed(&self) -> [u8; 32] {
        *self.seed
    }

    fn signing(&self) -> SigningKey {
        SigningKey::from_bytes(&crypto::derive_subkey(&self.seed, "flint team sign v1"))
    }

    fn unwrapping(&self) -> StaticSecret {
        StaticSecret::from(*crypto::derive_subkey(&self.seed, "flint team box v1"))
    }

    pub fn sign_key(&self) -> [u8; 32] {
        self.signing().verifying_key().to_bytes()
    }

    pub fn box_key(&self) -> [u8; 32] {
        PublicKey::from(&self.unwrapping()).to_bytes()
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Roster {
    pub team_id: Uuid,
    pub seq: u64,
    /// Hex SHA-256 of the roster before this one; `None` in the first.
    pub prev_hash: Option<String>,
    pub vaults: Vec<RosterVault>,
    pub members: Vec<RosterMember>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RosterVault {
    pub id: Uuid,
    pub key_version: u64,
    /// Lets a member check the key they unwrapped is the one the admin meant.
    pub key_commit: String,
    /// Shown to members. Left out when not set: the server takes a name of
    /// 1 to 60 characters, or no name at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RosterMember {
    pub user_id: Uuid,
    pub sign_key: String,
    pub box_key: String,
    pub role: String,
    pub vault_ids: Vec<Uuid>,
}

impl Roster {
    pub fn parse(body: &[u8]) -> Option<Self> {
        serde_json::from_slice(body).ok()
    }

    pub fn member(&self, user_id: Uuid) -> Option<&RosterMember> {
        self.members.iter().find(|member| member.user_id == user_id)
    }

    pub fn is_manager(&self, user_id: Uuid) -> bool {
        self.member(user_id)
            .is_some_and(|member| matches!(member.role.as_str(), "owner" | "admin"))
    }

    pub fn owner(&self) -> Option<Uuid> {
        self.members
            .iter()
            .find(|member| member.role == "owner")
            .map(|member| member.user_id)
    }

    /// Everyone who may hold the key of this vault.
    pub fn holders(&self, vault_id: Uuid) -> Vec<Uuid> {
        let holds = |member: &&RosterMember| member.vault_ids.contains(&vault_id);
        self.members
            .iter()
            .filter(holds)
            .map(|member| member.user_id)
            .collect()
    }

    fn vault_ids(&self) -> Vec<Uuid> {
        self.vaults.iter().map(|vault| vault.id).collect()
    }

    fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a roster always encodes to JSON")
    }
}

fn hash(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn random_bytes() -> [u8; 32] {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes
}

fn key_commit(key: &VaultKey) -> String {
    let purpose = format!("flint key commit v1 {} {}", key.vault_id, key.key_version);
    BASE64.encode(*crypto::derive_subkey(&key.key, &purpose))
}

/// Encrypt a vault key so that only the holder of `box_key` can read it.
/// The result names the vault, the key version and the member, and opens
/// for nothing else.
fn wrap_key(box_key: &[u8; 32], user_id: Uuid, key: &VaultKey) -> Vec<u8> {
    let ephemeral = StaticSecret::from(random_bytes());
    let ephemeral_public = PublicKey::from(&ephemeral).to_bytes();
    let shared = ephemeral.diffie_hellman(&PublicKey::from(*box_key));
    let wrap_key = crypto::derive_subkey(shared.as_bytes(), "flint team wrap v1");
    let context = wrap_context(
        &ephemeral_public,
        box_key,
        user_id,
        key.vault_id,
        key.key_version,
    );
    let blob = crypto::seal(&wrap_key, &context, &key.key);
    [&ephemeral_public[..], &blob.to_bytes()].concat()
}

fn unwrap_key(
    keys: &TeamKeys,
    user_id: Uuid,
    vault_id: Uuid,
    key_version: u64,
    wrapped: &[u8],
) -> Option<[u8; 32]> {
    let (ephemeral_public, rest) = wrapped.split_first_chunk::<32>()?;
    let shared = keys
        .unwrapping()
        .diffie_hellman(&PublicKey::from(*ephemeral_public));
    let wrap_key = crypto::derive_subkey(shared.as_bytes(), "flint team wrap v1");
    let context = wrap_context(
        ephemeral_public,
        &keys.box_key(),
        user_id,
        vault_id,
        key_version,
    );
    let key = crypto::open(&wrap_key, &context, &AeadBlob::from_bytes(rest)?).ok()?;
    key.as_slice().try_into().ok()
}

fn wrap_context(
    ephemeral_public: &[u8; 32],
    box_key: &[u8; 32],
    user_id: Uuid,
    vault_id: Uuid,
    key_version: u64,
) -> Vec<u8> {
    let label = format!("{user_id} {vault_id} {key_version}");
    [&ephemeral_public[..], &box_key[..], label.as_bytes()].concat()
}

/// What an invitee sends with the link's secret: proof that the account
/// asking to join, with these keys, is the one the link was given to.
fn join_mac(
    secret: &[u8; 32],
    team_id: Uuid,
    user_id: Uuid,
    sign_key: &[u8],
    box_key: &[u8],
) -> [u8; 32] {
    let purpose = format!(
        "flint join v1 {team_id} {user_id} {} {}",
        BASE64.encode(sign_key),
        BASE64.encode(box_key)
    );
    *crypto::derive_subkey(secret, &purpose)
}

/// What the inviter sends back: proof that this roster is the one they
/// added the invitee with, so the invitee can start trusting the team there.
fn confirmation(secret: &[u8; 32], roster: &[u8]) -> [u8; 32] {
    *crypto::derive_subkey(secret, &format!("flint confirm v1 {}", hex(&hash(roster))))
}

/// Check `signed` as the roster that follows `prev`. A device runs these
/// checks itself, so the server cannot change a team on its own: the chain
/// is unbroken, the signer is the owner or an admin of `prev`, there is one
/// owner, only the owner hands the team over, and no member's keys change.
/// The server has more rules (who may change roles, which vaults an admin
/// holds); those are not checked here.
fn verify_next(
    prev: &Roster,
    prev_body: &[u8],
    signed: &SignedRoster,
) -> Result<Roster, TeamError> {
    let bad = TeamError::Untrusted;
    let next = Roster::parse(&signed.body).ok_or(bad("a roster is malformed"))?;
    if next.team_id != prev.team_id || next.seq != prev.seq + 1 {
        return Err(bad("the roster chain has a gap"));
    }
    if next.prev_hash.as_deref() != Some(hex(&hash(prev_body)).as_str()) {
        return Err(bad("a roster does not follow the one before it"));
    }
    if !prev.is_manager(signed.signer_user_id) {
        return Err(bad(
            "a roster is signed by someone who may not change the team",
        ));
    }
    let signer = prev
        .member(signed.signer_user_id)
        .ok_or(bad("the signer is unknown"))?;
    if !is_signed_by(&signer.sign_key, &signed.body, &signed.signature) {
        return Err(bad("a roster signature is wrong"));
    }
    if next.owner().is_none() || next.members.iter().filter(|m| m.role == "owner").count() != 1 {
        return Err(bad("a roster has no single owner"));
    }
    if next.owner() != prev.owner() && prev.owner() != Some(signed.signer_user_id) {
        return Err(bad(
            "the team was handed over by someone other than its owner",
        ));
    }
    for member in &next.members {
        let is_changed = prev.member(member.user_id).is_some_and(|before| {
            before.sign_key != member.sign_key || before.box_key != member.box_key
        });
        if is_changed {
            return Err(bad("a member's keys changed"));
        }
    }
    Ok(next)
}

fn is_signed_by(sign_key: &str, body: &[u8], signature: &[u8]) -> bool {
    let Some(key) = BASE64
        .decode(sign_key)
        .ok()
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
    else {
        return false;
    };
    let Ok(signature) = ed25519_dalek::Signature::from_slice(signature) else {
        return false;
    };
    VerifyingKey::from_bytes(&key).is_ok_and(|key| key.verify_strict(body, &signature).is_ok())
}

/// Make sure the account has team keys and return them. A device that
/// signed in after the keys were made fetches them; making them for the
/// first time needs `auth_key`, the proof of the master password.
pub fn ensure_keys(
    api: &Api,
    context: &TeamContext,
    auth_key: Option<&[u8; 32]>,
) -> Result<TeamKeys, TeamError> {
    if let Some(seed) = &context.key_seed {
        return Ok(TeamKeys::from_seed(**seed));
    }
    let token = &context.session_token;
    if let Some(wrapped) = api.key_seed(token)? {
        let seed = AeadBlob::from_bytes(&wrapped)
            .and_then(|blob| crypto::decrypt(&context.seed_key, &blob).ok())
            .and_then(|seed| <[u8; 32]>::try_from(seed.as_slice()).ok())
            .ok_or(TeamError::Untrusted(
                "the team keys on the server do not open",
            ))?;
        return Ok(TeamKeys::from_seed(seed));
    }

    let auth_key = auth_key.ok_or(TeamError::PasswordNeeded)?;
    let keys = TeamKeys::from_seed(random_bytes());
    let wrapped = crypto::encrypt(&context.seed_key, &keys.seed()).to_bytes();
    api.set_keys(token, auth_key, &keys.sign_key(), &keys.box_key(), &wrapped)?;
    Ok(keys)
}

/// Fetch every team of the account and check each against its roster chain.
/// A team that fails the check, or that the server refuses to show, is left
/// out and stays as it was on this device. A network error or an ended
/// session fails the whole refresh.
pub fn refresh(
    api: &Api,
    context: &TeamContext,
    keys: &TeamKeys,
) -> Result<TeamsUpdate, TeamError> {
    let listed = api.teams(&context.session_token)?;
    let mut update = TeamsUpdate {
        key_seed: Some(keys.seed()),
        teams: Vec::new(),
        known: context
            .teams
            .keys()
            .copied()
            .chain(context.pins.iter().map(|pin| pin.team_id))
            .collect(),
        listed: listed.iter().map(|team| team.id).collect(),
        checked: Vec::new(),
        closed_invites: Vec::new(),
    };
    for team in listed {
        match check_team(api, context, keys, team.id, team.name) {
            Ok(Some(team)) => {
                update.checked.push(team.team_id);
                update.teams.push(team);
            }
            // An invite was used and nobody accepted it yet.
            Ok(None) => {}
            Err(TeamError::Api(e)) if !is_refusal_of_one_team(&e) => return Err(e.into()),
            Err(e) => tracing::warn!(team = %team.id, error = %e, "team failed its check"),
        }
    }
    Ok(update)
}

/// The server answered, and said no to this team only: for example the
/// account was removed from it after the list was fetched. Any other status
/// (a busy server, the proxy in front of it) is about the whole refresh.
fn is_refusal_of_one_team(error: &ApiError) -> bool {
    matches!(
        error,
        ApiError::Refused {
            status: 403 | 404,
            ..
        }
    ) && !error.is_session_ended()
}

fn check_team(
    api: &Api,
    context: &TeamContext,
    keys: &TeamKeys,
    team_id: Uuid,
    name: String,
) -> Result<Option<TeamUpdate>, TeamError> {
    let bad = TeamError::Untrusted;
    let token = &context.session_token;
    let details = api.team(token, team_id)?;
    let chain = api.rosters(token, team_id)?;

    let Some(start) = find_anchor(
        context,
        team_id,
        &chain,
        details.join_confirmation.as_deref(),
    )?
    else {
        tracing::debug!(team = %team_id, "no trusted roster or accepted join for this team yet, skipping it");
        return Ok(None);
    };
    let mut body = chain[start].body.clone();
    let mut roster = Roster::parse(&body).ok_or(bad("a roster is malformed"))?;
    if roster.team_id != team_id || roster.seq != start as u64 + 1 {
        return Err(bad(
            "the roster belongs to another team or place in the chain",
        ));
    }
    for signed in &chain[start + 1..] {
        roster = verify_next(&roster, &body, signed)?;
        body = signed.body.clone();
    }

    let me = roster
        .member(context.user_id)
        .ok_or(bad("this account is not in the roster"))?;
    if me.sign_key != BASE64.encode(keys.sign_key()) || me.box_key != BASE64.encode(keys.box_key())
    {
        return Err(bad("the roster lists this account with other keys"));
    }
    let mut vaults = Vec::new();
    for vault_id in &me.vault_ids {
        let listed = roster
            .vaults
            .iter()
            .find(|v| v.id == *vault_id)
            .ok_or(bad("a vault is missing"))?;
        let key = details
            .my_vault_keys
            .iter()
            .find(|k| k.vault_id == *vault_id && k.key_version == listed.key_version)
            .and_then(|k| {
                unwrap_key(
                    keys,
                    context.user_id,
                    *vault_id,
                    k.key_version,
                    &k.wrapped_key,
                )
            })
            .ok_or(bad("a vault key did not arrive or does not open"))?;
        let key = VaultKey {
            vault_id: *vault_id,
            key,
            key_version: listed.key_version,
        };
        if key_commit(&key) != listed.key_commit {
            return Err(bad("a vault key is not the one the roster names"));
        }
        vaults.push(key);
    }

    let present: Vec<Uuid> = details
        .members
        .iter()
        .map(|member| member.user_id)
        .collect();
    let left = roster
        .members
        .iter()
        .map(|m| m.user_id)
        .filter(|id| !present.contains(id))
        .collect();
    let logins = details
        .members
        .into_iter()
        .map(|m| (m.user_id, m.github_login))
        .collect();
    Ok(Some(TeamUpdate {
        team_id,
        // The pin follows the newest roster, so another device of the account
        // cannot be shown the team as it was before someone was removed.
        pin: Some(TeamPin {
            team_id,
            seq: roster.seq,
            hash: hash(&body),
        }),
        state: TeamState {
            name,
            vault_names: vault_names(&roster),
            roster: body,
            logins,
            left,
        },
        vaults,
    }))
}

/// Find the roster this device already trusts in the chain the server sent:
/// the newest of the one it checked before and the one pinned by any device
/// of the account, or else the one the inviter confirmed. Returns its index.
fn find_anchor(
    context: &TeamContext,
    team_id: Uuid,
    chain: &[SignedRoster],
    join_confirmation: Option<&[u8]>,
) -> Result<Option<usize>, TeamError> {
    let seen = context.teams.get(&team_id).and_then(|state| {
        let seq = Roster::parse(&state.roster)?.seq;
        Some((seq, hash(&state.roster)))
    });
    let pinned = context
        .pins
        .iter()
        .find(|pin| pin.team_id == team_id)
        .map(|pin| (pin.seq, pin.hash));
    let trusted = [seen, pinned]
        .into_iter()
        .flatten()
        .max_by_key(|(seq, _)| *seq);

    if let Some((seq, trusted_hash)) = trusted {
        let index = (seq as usize).checked_sub(1);
        return match index.and_then(|index| chain.get(index)) {
            Some(signed) if hash(&signed.body) == trusted_hash => Ok(index),
            _ => Err(TeamError::Untrusted(
                "the server's history of the team does not match what this account saw",
            )),
        };
    }

    let Some(join) = context.joins.iter().find(|join| join.team_id == team_id) else {
        return Ok(None);
    };
    let Some(proof) = join_confirmation else {
        return Ok(None);
    };
    // A link is good for many people, and all of them know its secret. So the
    // roster must also be signed by the person who made the link: their key
    // is part of the link, and nobody else can sign with it. A join saved
    // without that key cannot be confirmed.
    let is_confirmed = |signed: &SignedRoster| {
        bool::from(confirmation(&join.secret, &signed.body)[..].ct_eq(proof))
            && join.inviter_key.is_some_and(|key| {
                is_signed_by(&BASE64.encode(key), &signed.body, &signed.signature)
            })
    };
    match chain.iter().position(is_confirmed) {
        Some(index) => Ok(Some(index)),
        None => Err(TeamError::Untrusted(
            "the inviter's confirmation matches no roster",
        )),
    }
}

fn vault_names(roster: &Roster) -> Vec<(Uuid, Option<String>)> {
    roster
        .vaults
        .iter()
        .map(|vault| (vault.id, vault.name.clone()))
        .collect()
}

fn sign(keys: &TeamKeys, roster: &Roster) -> RosterSubmission {
    let body = roster.to_bytes();
    let signature = keys.signing().sign(&body).to_bytes().to_vec();
    RosterSubmission {
        body,
        signature,
        ..Default::default()
    }
}

/// Create a team with its first vault, the one every new member sees.
pub fn create(
    api: &Api,
    context: &TeamContext,
    keys: &TeamKeys,
    name: &str,
) -> Result<TeamUpdate, TeamError> {
    let team_id = Uuid::new_v4();
    let key = VaultKey {
        vault_id: Uuid::new_v4(),
        key: random_bytes(),
        key_version: 1,
    };
    let roster = Roster {
        team_id,
        seq: 1,
        prev_hash: None,
        vaults: vec![RosterVault {
            id: key.vault_id,
            key_version: 1,
            key_commit: key_commit(&key),
            name: None,
        }],
        members: vec![RosterMember {
            user_id: context.user_id,
            sign_key: BASE64.encode(keys.sign_key()),
            box_key: BASE64.encode(keys.box_key()),
            role: "owner".into(),
            vault_ids: vec![key.vault_id],
        }],
    };
    let mut submission = sign(keys, &roster);
    submission.wraps.push((
        key.vault_id,
        context.user_id,
        wrap_key(&keys.box_key(), context.user_id, &key),
    ));
    api.create_team(&context.session_token, name, &submission)?;

    let pin = TeamPin {
        team_id,
        seq: 1,
        hash: hash(&submission.body),
    };
    Ok(TeamUpdate {
        team_id,
        state: TeamState {
            name: name.to_string(),
            vault_names: vault_names(&roster),
            roster: submission.body,
            logins: Default::default(),
            left: Vec::new(),
        },
        vaults: vec![key],
        pin: Some(pin),
    })
}

/// Make an invite link that any number of people can use until it expires
/// or is revoked. What follows `#` never reaches the server: a secret, so
/// the server cannot use the link itself, and the inviter's signing key, so
/// the people who join can tell the real team from one made up for them.
pub fn invite(
    api: &Api,
    context: &TeamContext,
    keys: &TeamKeys,
    team_id: Uuid,
) -> Result<(Uuid, InviteSecret), TeamError> {
    let token = BASE64_URL.encode(random_bytes());
    let secret = random_bytes();
    let id = api.create_invite(&context.session_token, team_id, &hash(token.as_bytes()))?;
    let link = format!(
        "{INVITE_PREFIX}{token}#{}.{}",
        BASE64_URL.encode(secret),
        BASE64_URL.encode(keys.sign_key())
    );
    Ok((
        id,
        InviteSecret {
            team_id,
            link,
            secret,
        },
    ))
}

/// Use an invite link. This grants nothing yet: the inviter's device has to
/// accept the request. Returns the team's name and what to remember.
pub fn join(
    api: &Api,
    context: &TeamContext,
    keys: &TeamKeys,
    link: &str,
) -> Result<(String, PendingJoin), TeamError> {
    let (token, secret) = link
        .trim()
        .strip_prefix(INVITE_PREFIX)
        .and_then(|rest| rest.split_once('#'))
        .ok_or(TeamError::BadLink)?;
    let key32 = |text: &str| -> Result<[u8; 32], TeamError> {
        BASE64_URL
            .decode(text)
            .ok()
            .and_then(|s| s.try_into().ok())
            .ok_or(TeamError::BadLink)
    };
    let (secret, inviter_key) = secret.split_once('.').ok_or(TeamError::BadLink)?;
    let (secret, inviter_key) = (key32(secret)?, key32(inviter_key)?);
    if token.is_empty()
        || !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(TeamError::BadLink);
    }

    let preview = api.preview_invite(&context.session_token, token)?;
    let mac = join_mac(
        &secret,
        preview.team_id,
        context.user_id,
        &keys.sign_key(),
        &keys.box_key(),
    );
    api.ask_to_join(&context.session_token, token, &mac)?;
    let join = PendingJoin {
        id: Uuid::new_v4(),
        team_id: preview.team_id,
        secret,
        inviter_key: Some(inviter_key),
    };
    Ok((preview.team_name, join))
}

/// People who used an invite link made on this device and wait to be
/// accepted, each with the id of the link they used. A request whose proof
/// does not match the link's secret is left out. Also returns the ids of
/// the team's links that are still open.
pub fn requests(
    api: &Api,
    context: &TeamContext,
    team_id: Uuid,
) -> Result<(Vec<(Uuid, JoinRequest)>, Vec<Uuid>), TeamError> {
    let mut requests = Vec::new();
    let mut open = Vec::new();
    for invite in api.invites(&context.session_token, team_id)? {
        let link_id = invite.parent_id.unwrap_or(invite.id);
        let Some(request) = invite.join_request else {
            open.push(invite.id);
            continue;
        };
        let Some(mine) = context.invites.get(&link_id) else {
            continue;
        };
        let expected = join_mac(
            &mine.secret,
            team_id,
            request.user_id,
            &request.public_sign_key,
            &request.public_box_key,
        );
        if bool::from(request.mac[..].ct_eq(&expected[..])) {
            requests.push((link_id, request));
        } else {
            tracing::warn!(user = %request.user_id, "ignoring a join request with a wrong proof");
        }
    }
    Ok((requests, open))
}

struct Managed {
    roster: Roster,
    body: Vec<u8>,
    vault_keys: Vec<VaultKey>,
}

/// The team as this device last verified it, for signing its next roster.
fn managed(context: &TeamContext, team_id: Uuid) -> Result<Managed, TeamError> {
    let bad = TeamError::Untrusted;
    let state = context
        .teams
        .get(&team_id)
        .ok_or(bad("the team is not known on this device"))?;
    let roster = Roster::parse(&state.roster).ok_or(bad("a roster is malformed"))?;
    if !roster.is_manager(context.user_id) {
        return Err(TeamError::Refused(
            "only the owner and admins can change the team",
        ));
    }
    let vault_keys = context
        .vault_keys
        .iter()
        .filter(|(team, _)| *team == team_id)
        .map(|(_, key)| key.clone())
        .collect();
    Ok(Managed {
        roster,
        body: state.roster.clone(),
        vault_keys,
    })
}

fn next_roster(team: &Managed) -> Roster {
    let mut roster = team.roster.clone();
    roster.seq += 1;
    roster.prev_hash = Some(hex(&hash(&team.body)));
    roster
}

fn box_key_of(member: &RosterMember) -> Result<[u8; 32], TeamError> {
    BASE64
        .decode(&member.box_key)
        .ok()
        .and_then(|key| key.try_into().ok())
        .ok_or(TeamError::Untrusted("a member has a malformed key"))
}

/// Sign `roster` as the one after `team.roster`, with the keys it needs:
/// - a new vault (`new_key`): its key for everyone who holds it;
/// - a vault somebody lost: a new key for everyone who still holds it, and
///   every item encrypted again, because whoever lost it still has the old key;
/// - a vault somebody gained: the current key for them.
///
/// The server checks the same three rules and takes nothing less or more.
fn sign_with_keys(
    api: &Api,
    context: &TeamContext,
    keys: &TeamKeys,
    team: &Managed,
    mut roster: Roster,
    new_key: Option<&VaultKey>,
) -> Result<RosterSubmission, TeamError> {
    let mut wraps = Vec::new();
    let mut rotations = Vec::new();
    for index in 0..roster.vaults.len() {
        let vault_id = roster.vaults[index].id;
        let before = team.roster.holders(vault_id);
        let now = roster.holders(vault_id);
        let is_new = !team.roster.vaults.iter().any(|vault| vault.id == vault_id);
        let has_lost = before.iter().any(|user_id| !now.contains(user_id));
        let gained: Vec<Uuid> = now
            .iter()
            .copied()
            .filter(|id| !before.contains(id))
            .collect();
        if !is_new && !has_lost && gained.is_empty() {
            continue;
        }

        let held = if is_new {
            new_key
        } else {
            team.vault_keys.iter().find(|key| key.vault_id == vault_id)
        };
        let mut key =
            held.filter(|key| key.vault_id == vault_id)
                .cloned()
                .ok_or(TeamError::Untrusted(
                    "a vault key is missing on this device",
                ))?;
        if has_lost {
            let next = VaultKey {
                vault_id,
                key: random_bytes(),
                key_version: key.key_version + 1,
            };
            roster.vaults[index].key_version = next.key_version;
            roster.vaults[index].key_commit = key_commit(&next);
            rotations.push(reencrypt(api, &context.session_token, &key, &next)?);
            key = next;
        }
        let receivers = if is_new || has_lost { now } else { gained };
        for user_id in receivers {
            let member = roster
                .member(user_id)
                .ok_or(TeamError::Refused("no such member"))?;
            wraps.push((
                vault_id,
                user_id,
                wrap_key(&box_key_of(member)?, user_id, &key),
            ));
        }
    }

    let mut submission = sign(keys, &roster);
    submission.wraps = wraps;
    submission.rotations = rotations;
    Ok(submission)
}

/// Accept a join request: sign a roster that adds the member. They see the
/// team's first vault; an owner or admin can give them the others.
pub fn accept(
    api: &Api,
    context: &TeamContext,
    keys: &TeamKeys,
    team_id: Uuid,
    invite_id: Uuid,
    request: &JoinRequest,
) -> Result<(), TeamError> {
    let team = managed(context, team_id)?;
    let secret = context
        .invites
        .get(&invite_id)
        .ok_or(TeamError::Refused("the invite was not made on this device"))?
        .secret;
    let box_key: [u8; 32] = request
        .public_box_key
        .as_slice()
        .try_into()
        .map_err(|_| TeamError::Untrusted("the join request has a malformed key"))?;

    let mut roster = next_roster(&team);
    roster.members.push(RosterMember {
        user_id: request.user_id,
        sign_key: BASE64.encode(&request.public_sign_key),
        box_key: BASE64.encode(box_key),
        role: "member".into(),
        vault_ids: roster
            .vaults
            .first()
            .map(|vault| vault.id)
            .into_iter()
            .collect(),
    });
    let mut submission = sign_with_keys(api, context, keys, &team, roster, None)?;
    submission
        .confirmations
        .push((request.user_id, confirmation(&secret, &submission.body)));
    Ok(api.submit_roster(&context.session_token, team_id, &submission)?)
}

/// Remove members. Every vault they could open gets a new key.
pub fn remove(
    api: &Api,
    context: &TeamContext,
    keys: &TeamKeys,
    team_id: Uuid,
    user_ids: &[Uuid],
) -> Result<(), TeamError> {
    let team = managed(context, team_id)?;
    let mut roster = next_roster(&team);
    roster
        .members
        .retain(|member| !user_ids.contains(&member.user_id));
    let submission = sign_with_keys(api, context, keys, &team, roster, None)?;
    Ok(api.submit_roster(&context.session_token, team_id, &submission)?)
}

/// A vault name as it goes into the roster.
fn vault_name(name: &str) -> Result<String, TeamError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_VAULT_NAME_CHARS {
        return Err(TeamError::Refused("a vault name has 1 to 60 characters"));
    }
    Ok(name.to_string())
}

/// Add a vault to the team. The owner and admins hold every vault, so they
/// get its key; plain members see it once someone gives them access.
pub fn add_vault(
    api: &Api,
    context: &TeamContext,
    keys: &TeamKeys,
    team_id: Uuid,
    name: &str,
) -> Result<(), TeamError> {
    let team = managed(context, team_id)?;
    if team.roster.vaults.len() >= MAX_VAULTS {
        return Err(TeamError::Refused("a team has at most 10 vaults"));
    }
    let key = VaultKey {
        vault_id: Uuid::new_v4(),
        key: random_bytes(),
        key_version: 1,
    };
    let mut roster = next_roster(&team);
    roster.vaults.push(RosterVault {
        id: key.vault_id,
        key_version: 1,
        key_commit: key_commit(&key),
        name: Some(vault_name(name)?),
    });
    for member in roster
        .members
        .iter_mut()
        .filter(|member| member.role != "member")
    {
        member.vault_ids.push(key.vault_id);
    }
    let submission = sign_with_keys(api, context, keys, &team, roster, Some(&key))?;
    Ok(api.submit_roster(&context.session_token, team_id, &submission)?)
}

pub fn rename_vault(
    api: &Api,
    context: &TeamContext,
    keys: &TeamKeys,
    team_id: Uuid,
    vault_id: Uuid,
    name: &str,
) -> Result<(), TeamError> {
    let team = managed(context, team_id)?;
    let mut roster = next_roster(&team);
    let vault = roster
        .vaults
        .iter_mut()
        .find(|vault| vault.id == vault_id)
        .ok_or(TeamError::Refused("no such vault"))?;
    vault.name = Some(vault_name(name)?);
    Ok(api.submit_roster(&context.session_token, team_id, &sign(keys, &roster))?)
}

/// Delete a vault with everything in it. No key changes: the server drops
/// the items, so the old key opens nothing.
pub fn delete_vault(
    api: &Api,
    context: &TeamContext,
    keys: &TeamKeys,
    team_id: Uuid,
    vault_id: Uuid,
) -> Result<(), TeamError> {
    let team = managed(context, team_id)?;
    let roster = without_vault(&team, vault_id)?;
    Ok(api.submit_roster(&context.session_token, team_id, &sign(keys, &roster))?)
}

/// The next roster, with one vault gone.
///
/// The first vault stays. It is the one `accept` gives to a new member, and
/// with it gone the next vault in line would take that place without anyone
/// having chosen it, even one that only a few people were meant to see.
fn without_vault(team: &Managed, vault_id: Uuid) -> Result<Roster, TeamError> {
    let mut roster = next_roster(team);
    let position = roster
        .vaults
        .iter()
        .position(|vault| vault.id == vault_id)
        .ok_or(TeamError::Refused("no such vault"))?;
    if position == 0 {
        return Err(TeamError::Refused(
            "the first vault of a team cannot be deleted",
        ));
    }
    roster.vaults.remove(position);
    for member in &mut roster.members {
        member.vault_ids.retain(|id| *id != vault_id);
    }
    Ok(roster)
}

/// Choose which plain members see a vault: those in `user_ids` do, the
/// others do not. The owner and admins always see every vault.
pub fn set_access(
    api: &Api,
    context: &TeamContext,
    keys: &TeamKeys,
    team_id: Uuid,
    vault_id: Uuid,
    user_ids: &[Uuid],
) -> Result<(), TeamError> {
    let team = managed(context, team_id)?;
    let mut roster = next_roster(&team);
    if !roster.vaults.iter().any(|vault| vault.id == vault_id) {
        return Err(TeamError::Refused("no such vault"));
    }
    if user_ids
        .iter()
        .any(|user_id| roster.member(*user_id).is_none())
    {
        return Err(TeamError::Refused("no such member"));
    }
    for member in roster
        .members
        .iter_mut()
        .filter(|member| member.role == "member")
    {
        member.vault_ids.retain(|id| *id != vault_id);
        if user_ids.contains(&member.user_id) {
            member.vault_ids.push(vault_id);
        }
    }
    let submission = sign_with_keys(api, context, keys, &team, roster, None)?;
    Ok(api.submit_roster(&context.session_token, team_id, &submission)?)
}

/// Fetch every item of a vault, deleted ones too, and encrypt each under
/// the new key.
fn reencrypt(
    api: &Api,
    token: &str,
    old: &VaultKey,
    new: &VaultKey,
) -> Result<Rotation, TeamError> {
    let mut rotation = Rotation {
        vault_id: old.vault_id,
        base_rev: 0,
        items: Vec::new(),
    };
    loop {
        let page = api.pull(token, old.vault_id, rotation.base_rev)?;
        // More to come but nothing in this page: the cursor would never move.
        if page.has_more && page.items.is_empty() {
            return Err(ApiError::BadAnswer.into());
        }
        for item in &page.items {
            let blob = sync::reseal_item(&old.key, &new.key, item.id, &item.blob);
            rotation.items.push((item.id, blob));
            rotation.base_rev = item.rev;
        }
        if !page.has_more {
            return Ok(rotation);
        }
    }
}

/// Make a member an admin, or an admin a plain member again. An admin can
/// invite, accept and remove people; only the owner can change roles.
/// A new admin gets the vaults they did not see: the server refuses an
/// admin without all of them.
pub fn set_role(
    api: &Api,
    context: &TeamContext,
    keys: &TeamKeys,
    team_id: Uuid,
    user_id: Uuid,
    role: &str,
) -> Result<(), TeamError> {
    let team = managed(context, team_id)?;
    let mut roster = next_roster(&team);
    let vault_ids = roster.vault_ids();
    let member = roster
        .members
        .iter_mut()
        .find(|member| member.user_id == user_id && member.role != "owner")
        .ok_or(TeamError::Refused("no such member"))?;
    member.role = role.to_string();
    if role == "admin" {
        member.vault_ids = vault_ids;
    }
    let submission = sign_with_keys(api, context, keys, &team, roster, None)?;
    Ok(api.submit_roster(&context.session_token, team_id, &submission)?)
}

/// Hand the team to another member. The old owner stays in it as an admin.
/// Every device checks that this roster was signed by the owner it replaces.
/// The new owner gets every vault, like a new admin.
pub fn hand_over(
    api: &Api,
    context: &TeamContext,
    keys: &TeamKeys,
    team_id: Uuid,
    new_owner: Uuid,
) -> Result<(), TeamError> {
    let team = managed(context, team_id)?;
    if team.roster.owner() != Some(context.user_id) || team.roster.member(new_owner).is_none() {
        return Err(TeamError::Refused(
            "only the owner can hand the team to a member",
        ));
    }
    let mut roster = next_roster(&team);
    let vault_ids = roster.vault_ids();
    for member in &mut roster.members {
        if member.user_id == new_owner {
            member.role = "owner".into();
            member.vault_ids = vault_ids.clone();
        } else if member.user_id == context.user_id {
            member.role = "admin".into();
        }
    }
    let submission = sign_with_keys(api, context, keys, &team, roster, None)?;
    Ok(api.submit_roster(&context.session_token, team_id, &submission)?)
}

pub fn revoke(
    api: &Api,
    context: &TeamContext,
    team_id: Uuid,
    invite_id: Uuid,
) -> Result<(), TeamError> {
    Ok(api.revoke_invite(&context.session_token, team_id, invite_id)?)
}

pub fn leave(
    api: &Api,
    context: &TeamContext,
    team_id: Uuid,
    auth_key: &[u8; 32],
) -> Result<(), TeamError> {
    Ok(api.leave_team(&context.session_token, team_id, auth_key)?)
}

pub fn delete(
    api: &Api,
    context: &TeamContext,
    team_id: Uuid,
    auth_key: &[u8; 32],
) -> Result<(), TeamError> {
    Ok(api.delete_team(&context.session_token, team_id, auth_key)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(seed: u8) -> TeamKeys {
        TeamKeys::from_seed([seed; 32])
    }

    #[test]
    fn a_wrapped_key_opens_only_for_its_member_vault_and_version() {
        let alice = keys(1);
        let user = Uuid::new_v4();
        let key = VaultKey {
            vault_id: Uuid::new_v4(),
            key: [9; 32],
            key_version: 2,
        };
        let wrapped = wrap_key(&alice.box_key(), user, &key);

        assert_eq!(
            unwrap_key(&alice, user, key.vault_id, 2, &wrapped),
            Some([9; 32])
        );
        assert_eq!(unwrap_key(&keys(2), user, key.vault_id, 2, &wrapped), None);
        assert_eq!(
            unwrap_key(&alice, Uuid::new_v4(), key.vault_id, 2, &wrapped),
            None
        );
        assert_eq!(unwrap_key(&alice, user, Uuid::new_v4(), 2, &wrapped), None);
        assert_eq!(unwrap_key(&alice, user, key.vault_id, 3, &wrapped), None);
        assert!(
            wrapped.len() <= 256,
            "the server refuses a longer wrapped key"
        );
    }

    fn first_roster(owner: &TeamKeys, owner_id: Uuid) -> (Roster, Vec<u8>) {
        let roster = Roster {
            team_id: Uuid::new_v4(),
            seq: 1,
            prev_hash: None,
            vaults: Vec::new(),
            members: vec![RosterMember {
                user_id: owner_id,
                sign_key: BASE64.encode(owner.sign_key()),
                box_key: BASE64.encode(owner.box_key()),
                role: "owner".into(),
                vault_ids: Vec::new(),
            }],
        };
        let body = roster.to_bytes();
        (roster, body)
    }

    fn signed(signer: &TeamKeys, signer_id: Uuid, roster: &Roster) -> SignedRoster {
        let submission = sign(signer, roster);
        SignedRoster {
            body: submission.body,
            signature: submission.signature,
            signer_user_id: signer_id,
        }
    }

    #[test]
    fn a_roster_signed_by_the_owner_follows() {
        let owner = keys(1);
        let owner_id = Uuid::new_v4();
        let (first, body) = first_roster(&owner, owner_id);
        let mut next = first.clone();
        next.seq = 2;
        next.prev_hash = Some(hex(&hash(&body)));

        assert!(verify_next(&first, &body, &signed(&owner, owner_id, &next)).is_ok());
    }

    #[test]
    fn a_roster_the_server_could_make_up_is_refused() {
        let owner = keys(1);
        let owner_id = Uuid::new_v4();
        let (first, body) = first_roster(&owner, owner_id);
        let mut next = first.clone();
        next.seq = 2;
        next.prev_hash = Some(hex(&hash(&body)));

        // Signed with a key that is not the owner's.
        assert!(verify_next(&first, &body, &signed(&keys(2), owner_id, &next)).is_err());
        // Signed by someone the roster does not know.
        assert!(verify_next(&first, &body, &signed(&keys(2), Uuid::new_v4(), &next)).is_err());
        // Does not follow the roster this device trusts.
        let mut elsewhere = next.clone();
        elsewhere.prev_hash = Some(hex(&[0; 32]));
        assert!(verify_next(&first, &body, &signed(&owner, owner_id, &elsewhere)).is_err());
        // Skips a roster.
        let mut skipping = next.clone();
        skipping.seq = 3;
        assert!(verify_next(&first, &body, &signed(&owner, owner_id, &skipping)).is_err());
        // Swaps a member's keys.
        let mut swapped = next.clone();
        swapped.members[0].box_key = BASE64.encode([7; 32]);
        assert!(verify_next(&first, &body, &signed(&owner, owner_id, &swapped)).is_err());
    }

    #[test]
    fn a_member_cannot_sign_a_roster() {
        let owner = keys(1);
        let member = keys(2);
        let (owner_id, member_id) = (Uuid::new_v4(), Uuid::new_v4());
        let (mut first, _) = first_roster(&owner, owner_id);
        first.members.push(RosterMember {
            user_id: member_id,
            sign_key: BASE64.encode(member.sign_key()),
            box_key: BASE64.encode(member.box_key()),
            role: "member".into(),
            vault_ids: Vec::new(),
        });
        let body = first.to_bytes();
        let mut next = first.clone();
        next.seq = 2;
        next.prev_hash = Some(hex(&hash(&body)));
        next.members[1].role = "admin".into();

        assert!(verify_next(&first, &body, &signed(&member, member_id, &next)).is_err());
    }

    #[test]
    fn a_vault_name_is_signed_only_when_set() {
        let key = VaultKey {
            vault_id: Uuid::new_v4(),
            key: [9; 32],
            key_version: 1,
        };
        let mut vault = RosterVault {
            id: key.vault_id,
            key_version: 1,
            key_commit: key_commit(&key),
            name: None,
        };
        let unnamed = serde_json::to_string(&vault).unwrap();
        assert!(!unnamed.contains("name"));
        // A roster signed before vaults had names still reads.
        assert!(serde_json::from_str::<RosterVault>(&unnamed)
            .unwrap()
            .name
            .is_none());

        vault.name = Some("Servers".into());
        let named = serde_json::to_string(&vault).unwrap();
        assert_eq!(
            serde_json::from_str::<RosterVault>(&named)
                .unwrap()
                .name
                .as_deref(),
            Some("Servers")
        );
    }

    #[test]
    fn a_vault_name_is_trimmed_and_has_one_to_sixty_characters() {
        assert_eq!(vault_name("  Servers ").unwrap(), "Servers");
        assert!(vault_name("   ").is_err());
        assert!(vault_name(&"я".repeat(60)).is_ok());
        assert!(vault_name(&"x".repeat(61)).is_err());
    }

    /// A team of an owner and a plain member with two vaults; the member
    /// sees only the first.
    fn two_vault_team(owner: &TeamKeys, member: &TeamKeys) -> (TeamContext, Managed, Uuid) {
        let (owner_id, member_id) = (Uuid::new_v4(), Uuid::new_v4());
        let keys: Vec<VaultKey> = (0..2)
            .map(|_| VaultKey {
                vault_id: Uuid::new_v4(),
                key: random_bytes(),
                key_version: 1,
            })
            .collect();
        let listed = |key: &VaultKey| RosterVault {
            id: key.vault_id,
            key_version: 1,
            key_commit: key_commit(key),
            name: None,
        };
        let person = |keys: &TeamKeys, user_id, role: &str, vault_ids| RosterMember {
            user_id,
            sign_key: BASE64.encode(keys.sign_key()),
            box_key: BASE64.encode(keys.box_key()),
            role: role.into(),
            vault_ids,
        };
        let roster = Roster {
            team_id: Uuid::new_v4(),
            seq: 1,
            prev_hash: None,
            vaults: keys.iter().map(listed).collect(),
            members: vec![
                person(
                    owner,
                    owner_id,
                    "owner",
                    vec![keys[0].vault_id, keys[1].vault_id],
                ),
                person(member, member_id, "member", vec![keys[0].vault_id]),
            ],
        };
        let context = TeamContext {
            session_token: "token".into(),
            user_id: owner_id,
            key_seed: None,
            seed_key: Zeroizing::new([0; 32]),
            teams: Default::default(),
            pins: Vec::new(),
            joins: Vec::new(),
            vault_keys: Vec::new(),
            invites: Default::default(),
        };
        let body = roster.to_bytes();
        (
            context,
            Managed {
                roster,
                body,
                vault_keys: keys,
            },
            member_id,
        )
    }

    #[test]
    fn a_vault_someone_gained_is_wrapped_for_them_alone() {
        let (owner, member) = (keys(1), keys(2));
        let (context, team, member_id) = two_vault_team(&owner, &member);
        let second = team.vault_keys[1].clone();
        let mut roster = next_roster(&team);
        roster.members[1].vault_ids.push(second.vault_id);

        let submission =
            sign_with_keys(&Api::shared(), &context, &owner, &team, roster, None).unwrap();

        assert!(submission.rotations.is_empty());
        assert_eq!(submission.wraps.len(), 1);
        let (vault_id, user_id, wrapped) = &submission.wraps[0];
        assert_eq!((*vault_id, *user_id), (second.vault_id, member_id));
        assert_eq!(
            unwrap_key(&member, member_id, second.vault_id, 1, wrapped),
            Some(second.key)
        );
        // The roster did not change the key it names.
        let signed = Roster::parse(&submission.body).unwrap();
        assert_eq!(
            signed.vaults[1].key_commit,
            team.roster.vaults[1].key_commit
        );
    }

    #[test]
    fn a_new_vault_is_wrapped_for_everyone_who_holds_it_and_nothing_else_is_sent() {
        let (owner, member) = (keys(1), keys(2));
        let (context, team, _) = two_vault_team(&owner, &member);
        let new = VaultKey {
            vault_id: Uuid::new_v4(),
            key: random_bytes(),
            key_version: 1,
        };
        let mut roster = next_roster(&team);
        roster.vaults.push(RosterVault {
            id: new.vault_id,
            key_version: 1,
            key_commit: key_commit(&new),
            name: Some("Servers".into()),
        });
        roster.members[0].vault_ids.push(new.vault_id);

        let submission =
            sign_with_keys(&Api::shared(), &context, &owner, &team, roster, Some(&new)).unwrap();

        assert!(submission.rotations.is_empty());
        let wrapped_for: Vec<(Uuid, Uuid)> = submission
            .wraps
            .iter()
            .map(|(vault_id, user_id, _)| (*vault_id, *user_id))
            .collect();
        assert_eq!(wrapped_for, [(new.vault_id, context.user_id)]);
    }

    #[test]
    fn the_first_vault_of_a_team_cannot_be_deleted() {
        let (owner, member) = (keys(1), keys(2));
        let (_, team, _) = two_vault_team(&owner, &member);
        let (first, second) = (team.vault_keys[0].vault_id, team.vault_keys[1].vault_id);

        assert!(matches!(
            without_vault(&team, first),
            Err(TeamError::Refused(_))
        ));
        assert!(matches!(
            without_vault(&team, Uuid::new_v4()),
            Err(TeamError::Refused(_))
        ));

        let roster = without_vault(&team, second).unwrap();
        assert_eq!(roster.vault_ids(), [first]);
        assert_eq!(roster.seq, team.roster.seq + 1);
        assert!(roster
            .members
            .iter()
            .all(|member| member.vault_ids == [first]));
    }

    #[test]
    fn a_join_is_confirmed_only_by_a_roster_the_inviter_signed() {
        let (owner, member) = (keys(1), keys(2));
        let (mut context, team, _) = two_vault_team(&owner, &member);
        let team_id = team.roster.team_id;
        let secret = [3; 32];
        let chain = [signed(&owner, context.user_id, &team.roster)];
        let proof = confirmation(&secret, &chain[0].body);
        let join = |inviter_key| PendingJoin {
            id: Uuid::new_v4(),
            team_id,
            secret,
            inviter_key,
        };

        context.joins = vec![join(Some(owner.sign_key()))];
        assert_eq!(
            find_anchor(&context, team_id, &chain, Some(&proof[..])).unwrap(),
            Some(0)
        );
        // Not accepted yet.
        assert_eq!(find_anchor(&context, team_id, &chain, None).unwrap(), None);

        // The link named someone else as its maker.
        context.joins = vec![join(Some(member.sign_key()))];
        assert!(find_anchor(&context, team_id, &chain, Some(&proof[..])).is_err());
        // The join was saved without the maker's key.
        context.joins = vec![join(None)];
        assert!(find_anchor(&context, team_id, &chain, Some(&proof[..])).is_err());
    }

    #[test]
    fn only_a_refusal_of_one_team_lets_the_refresh_go_on() {
        let refused = |code: &str| ApiError::Refused {
            status: 403,
            code: code.into(),
            message: String::new(),
        };
        assert!(is_refusal_of_one_team(&refused("forbidden")));
        assert!(!is_refusal_of_one_team(&refused("unauthorized")));
        let bad_gateway = ApiError::Refused {
            status: 502,
            code: String::new(),
            message: String::new(),
        };
        assert!(!is_refusal_of_one_team(&bad_gateway));
        assert!(!is_refusal_of_one_team(&ApiError::Network(
            "timed out".into()
        )));
        assert!(!is_refusal_of_one_team(&ApiError::BadAnswer));
    }

    #[test]
    fn a_join_proof_is_tied_to_the_account_and_its_keys() {
        let secret = [3; 32];
        let (team, user) = (Uuid::new_v4(), Uuid::new_v4());
        let mac = join_mac(&secret, team, user, &[1; 32], &[2; 32]);
        assert_eq!(mac, join_mac(&secret, team, user, &[1; 32], &[2; 32]));
        assert_ne!(
            mac,
            join_mac(&secret, team, Uuid::new_v4(), &[1; 32], &[2; 32])
        );
        assert_ne!(mac, join_mac(&secret, team, user, &[1; 32], &[9; 32]));
        assert_ne!(mac, join_mac(&[4; 32], team, user, &[1; 32], &[2; 32]));
    }
}
