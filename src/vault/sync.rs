//! What the vault knows about sync: the account it is signed in to, which
//! records the server already has, and how a record becomes a server item.
//!
//! Nothing here touches the network. [`VaultManager::begin_sync`] hands out
//! what has to be sent, the caller talks to the server, and
//! [`VaultManager::finish_round`] takes the answers.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;
use zeroize::Zeroizing;

use super::format::{
    AeadBlob, Group, KdfParams, ProxyDef, ProxyRouting, ServerEntry, Snippet, SshKey, Tag,
    VaultPayload,
};
use super::{crypto, AuthMethod, NewVaultKeys, VaultError, VaultManager};

/// The server refuses a larger item, and one refused item fails its whole batch.
pub const MAX_ITEM_BYTES: usize = 64 * 1024;

const ITEM_FORMAT: u8 = 1;
const ITEM_KEY_PURPOSE: &str = "flint items v1";
const AUTH_KEY_PURPOSE: &str = "flint auth v1";
const RECOVERY_AUTH_KEY_PURPOSE: &str = "flint recovery auth v1";
const KEY_SEED_PURPOSE: &str = "flint key seed v1";
const COPY_ID_PURPOSE: &str = "flint personal copy id v1";

/// The proxy rule is one record with no id of its own, so it syncs under this one.
const PROXY_ROUTING_ID: Uuid = Uuid::from_u128(0x666c696e_7470_4000_8000_726f7574696e);

/// What a team vault without a name is called: the one made with the team.
pub const UNNAMED_VAULT: &str = "General";

/// What the server checks instead of the password. It cannot be turned back
/// into the key that opens the vault.
pub(crate) fn auth_key(kek: &[u8; 32]) -> [u8; 32] {
    *crypto::derive_subkey(kek, AUTH_KEY_PURPOSE)
}

/// The same for the recovery phrase.
pub(crate) fn recovery_auth_key(recovery_key: &[u8; 32]) -> [u8; 32] {
    *crypto::derive_subkey(recovery_key, RECOVERY_AUTH_KEY_PURPOSE)
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SyncState {
    pub user_id: Uuid,
    pub github_login: String,
    /// `None` after the server refused the session: the user signs in again.
    pub session_token: Option<String>,
    pub vault_id: Uuid,
    /// Missing in older vault files. Such a vault starts from an empty
    /// ledger, and the next round matches its records to the server's by id.
    #[serde(default)]
    personal: Ledger,
    /// Set by a recovery done on this device: the server still has the old
    /// password and phrase, and is told about the new ones on the next sync.
    #[serde(default)]
    pending_recovery: Option<PendingRecovery>,
    /// What this account's team keys are made from. `None` until the account
    /// first needs them.
    #[serde(default)]
    pub(crate) key_seed: Option<[u8; 32]>,
    /// Teams as last checked against their signed rosters.
    #[serde(default)]
    pub teams: BTreeMap<Uuid, TeamState>,
    #[serde(default)]
    team_vaults: BTreeMap<Uuid, TeamVault>,
    /// Which team vault a record belongs to. A record not listed is personal.
    #[serde(default)]
    shared: BTreeMap<Uuid, Uuid>,
    /// Invite links made on this device. Only it can check who used them.
    #[serde(default)]
    pub invites: BTreeMap<Uuid, InviteSecret>,
    /// Hosts and keys that went on under a new id when their team vault was
    /// lost: old id to current id. An open session still names its host by
    /// the old one.
    #[serde(default)]
    renamed: BTreeMap<Uuid, Uuid>,
    /// A team changed its keys and this device has not caught up.
    #[serde(skip)]
    is_behind_on_teams: bool,
}

impl SyncState {
    pub fn new(user_id: Uuid, github_login: String, session_token: String, vault_id: Uuid) -> Self {
        Self {
            user_id,
            github_login,
            session_token: Some(session_token),
            vault_id,
            personal: Ledger::default(),
            pending_recovery: None,
            key_seed: None,
            teams: BTreeMap::new(),
            team_vaults: BTreeMap::new(),
            shared: BTreeMap::new(),
            invites: BTreeMap::new(),
            renamed: BTreeMap::new(),
            is_behind_on_teams: false,
        }
    }

    fn ledger_mut(&mut self, vault_id: Uuid) -> Option<&mut Ledger> {
        if vault_id == self.vault_id {
            return Some(&mut self.personal);
        }
        self.team_vaults
            .get_mut(&vault_id)
            .map(|vault| &mut vault.ledger)
    }
}

impl std::fmt::Debug for SyncState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncState")
            .field("user_id", &self.user_id)
            .field("github_login", &self.github_login)
            .field("teams", &self.teams.len())
            .finish_non_exhaustive()
    }
}

/// What the server holds of one vault, as far as this device knows.
#[derive(Clone, Default, Serialize, Deserialize)]
struct Ledger {
    /// Every server change up to this revision has been applied here.
    cursor: u64,
    items: BTreeMap<Uuid, SyncedItem>,
}

/// A team this account is in. Everything except `name` and the logins comes
/// from the signed roster; those two are labels the server supplied.
#[derive(Clone, Serialize, Deserialize)]
pub struct TeamState {
    pub name: String,
    /// The newest roster that passed every check, as signed.
    pub roster: Vec<u8>,
    pub logins: BTreeMap<Uuid, String>,
    /// Listed in the roster but gone from the team: an admin's device must
    /// sign them out of it and change the keys.
    pub left: Vec<Uuid>,
    /// Every vault of the team in the roster's order, with its name if it
    /// has one. Copied from the roster, so lists need not parse it.
    #[serde(default)]
    pub vault_names: Vec<(Uuid, Option<String>)>,
}

impl TeamState {
    pub fn vault_name(&self, vault_id: Uuid) -> String {
        let name = self
            .vault_names
            .iter()
            .find(|(id, _)| *id == vault_id)
            .and_then(|(_, name)| name.clone());
        name.unwrap_or_else(|| UNNAMED_VAULT.to_string())
    }

    /// How a vault is called next to a host: by the team's name, and by its
    /// own too when the team has more than one vault.
    pub fn vault_label(&self, vault_id: Uuid) -> String {
        if self.vault_names.len() < 2 {
            return self.name.clone();
        }
        format!("{} / {}", self.name, self.vault_name(vault_id))
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct TeamVault {
    team_id: Uuid,
    key: [u8; 32],
    key_version: u64,
    ledger: Ledger,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct InviteSecret {
    pub team_id: Uuid,
    pub link: String,
    pub secret: [u8; 32],
}

/// The roster this account joined a team at. Synced, so another device of
/// the account trusts the same starting point and not the server's word.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TeamPin {
    pub team_id: Uuid,
    pub seq: u64,
    pub hash: [u8; 32],
}

/// An invite this account used and an admin has not accepted yet.
#[derive(Clone, Serialize, Deserialize)]
pub struct PendingJoin {
    pub id: Uuid,
    pub team_id: Uuid,
    pub secret: [u8; 32],
    /// The signing key of whoever made the link. The roster that lets this
    /// account in must be signed with it.
    #[serde(default)]
    pub inviter_key: Option<[u8; 32]>,
}

impl std::fmt::Debug for PendingJoin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingJoin")
            .field("team_id", &self.team_id)
            .finish_non_exhaustive()
    }
}

/// The key of one team vault, for the code that talks to the server.
#[derive(Clone)]
pub struct VaultKey {
    pub vault_id: Uuid,
    pub key: [u8; 32],
    pub key_version: u64,
}

/// A record as the server last saw it.
#[derive(Clone, Serialize, Deserialize)]
struct SyncedItem {
    rev: u64,
    hash: [u8; 32],
}

#[derive(Clone, Serialize, Deserialize)]
struct PendingRecovery {
    old_recovery_auth_key: [u8; 32],
    new_auth_key: [u8; 32],
    new_recovery_auth_key: [u8; 32],
}

/// One vault record in the form that is encrypted and sent.
#[derive(Clone, Serialize, Deserialize)]
enum Record {
    Entry(ServerEntry),
    Key(SshKey),
    Snippet(Snippet),
    Group(Group),
    Tag(Tag),
    Proxy(ProxyDef),
    ProxyRouting(ProxyRouting),
    Team(TeamPin),
    Join(PendingJoin),
    /// The marker a deleted item carries. The server refuses an empty blob.
    Deleted,
}

impl Record {
    fn hash(&self) -> [u8; 32] {
        Sha256::digest(self.to_cbor()).into()
    }

    fn to_cbor(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        ciborium::into_writer(self, &mut bytes).expect("a record always encodes to CBOR");
        bytes
    }

    /// A record this large is never sent, so it must not count as waiting.
    fn is_too_large(&self) -> bool {
        // Format byte, nonce and tag around the CBOR.
        self.to_cbor().len() + 1 + 24 + 16 > MAX_ITEM_BYTES
    }

    fn seal(&self, item_key: &[u8; 32], id: Uuid) -> Vec<u8> {
        // Bound to the id, so the server cannot hand out one item as another.
        let blob = crypto::seal(item_key, id.as_bytes(), &self.to_cbor());
        [&[ITEM_FORMAT][..], &blob.to_bytes()].concat()
    }

    /// `None` for an item this build cannot read: damaged, or written by a
    /// newer flint.
    fn open(item_key: &[u8; 32], id: Uuid, bytes: &[u8]) -> Option<Self> {
        let (format, rest) = bytes.split_first()?;
        if *format != ITEM_FORMAT {
            return None;
        }
        let blob = AeadBlob::from_bytes(rest)?;
        let cbor = crypto::open(item_key, id.as_bytes(), &blob).ok()?;
        let record: Record = ciborium::from_reader(&cbor[..]).ok()?;
        (record.id().is_none_or(|record_id| record_id == id)).then_some(record)
    }

    fn id(&self) -> Option<Uuid> {
        match self {
            Record::Entry(entry) => Some(entry.id),
            Record::Key(key) => Some(key.id),
            Record::Snippet(snippet) => Some(snippet.id),
            Record::Group(group) => Some(group.id),
            Record::Tag(tag) => Some(tag.id),
            Record::Proxy(proxy) => Some(proxy.id),
            Record::ProxyRouting(_) => Some(PROXY_ROUTING_ID),
            Record::Team(pin) => Some(pin.team_id),
            Record::Join(join) => Some(join.id),
            Record::Deleted => None,
        }
    }

    /// Whether a local edit beats the server's version of the same record.
    /// Only hosts and snippets know when they were edited; for the rest the
    /// server's version wins.
    fn is_newer_than(&self, server: &Record) -> bool {
        match (self, server) {
            (Record::Entry(mine), Record::Entry(theirs)) => mine.updated_at > theirs.updated_at,
            (Record::Snippet(mine), Record::Snippet(theirs)) => mine.updated_at > theirs.updated_at,
            // A pin only ever moves forward along the team's roster chain.
            (Record::Team(mine), Record::Team(theirs)) => mine.seq > theirs.seq,
            _ => false,
        }
    }
}

impl VaultPayload {
    /// Every record as it is shared. What only this device cares about (the
    /// folders last shown, when a host was opened, where a snippet ran) is
    /// left out, so using a host does not count as editing it.
    fn records(&self) -> BTreeMap<Uuid, Record> {
        let mut records = BTreeMap::new();
        for entry in &self.entries {
            records.insert(entry.id, shared_entry(entry));
        }
        for snippet in &self.snippets {
            records.insert(snippet.id, shared_snippet(snippet));
        }
        for key in &self.keys {
            records.insert(key.id, Record::Key(key.clone()));
        }
        for group in &self.groups {
            records.insert(group.id, Record::Group(group.clone()));
        }
        for tag in &self.tags {
            records.insert(tag.id, Record::Tag(tag.clone()));
        }
        for proxy in &self.proxies {
            records.insert(proxy.id, Record::Proxy(proxy.clone()));
        }
        if self.has_proxy_rule() {
            records.insert(
                PROXY_ROUTING_ID,
                Record::ProxyRouting(self.proxy_routing.clone()),
            );
        }
        for pin in &self.team_pins {
            records.insert(pin.team_id, Record::Team(pin.clone()));
        }
        for join in &self.joins {
            records.insert(join.id, Record::Join(join.clone()));
        }
        records
    }

    /// The records of one vault: a team vault's, or with `None` the personal ones.
    fn records_in(&self, team_vault: Option<Uuid>) -> BTreeMap<Uuid, Record> {
        let shared = self.sync.as_ref().map(|state| &state.shared);
        let mut records = self.records();
        records.retain(|id, _| shared.and_then(|shared| shared.get(id)).copied() == team_vault);
        records
    }

    /// One record in its shared form, without copying the rest of the vault.
    fn record(&self, id: Uuid) -> Option<Record> {
        if id == PROXY_ROUTING_ID {
            return self
                .has_proxy_rule()
                .then(|| Record::ProxyRouting(self.proxy_routing.clone()));
        }
        if let Some(entry) = self.entries.iter().find(|e| e.id == id) {
            return Some(shared_entry(entry));
        }
        if let Some(snippet) = self.snippets.iter().find(|s| s.id == id) {
            return Some(shared_snippet(snippet));
        }
        let key = self
            .keys
            .iter()
            .find(|k| k.id == id)
            .cloned()
            .map(Record::Key);
        let group = || {
            self.groups
                .iter()
                .find(|g| g.id == id)
                .cloned()
                .map(Record::Group)
        };
        let tag = || {
            self.tags
                .iter()
                .find(|t| t.id == id)
                .cloned()
                .map(Record::Tag)
        };
        let proxy = || {
            self.proxies
                .iter()
                .find(|p| p.id == id)
                .cloned()
                .map(Record::Proxy)
        };
        let pin = || {
            self.team_pins
                .iter()
                .find(|p| p.team_id == id)
                .cloned()
                .map(Record::Team)
        };
        let join = || {
            self.joins
                .iter()
                .find(|j| j.id == id)
                .cloned()
                .map(Record::Join)
        };
        key.or_else(group)
            .or_else(tag)
            .or_else(proxy)
            .or_else(pin)
            .or_else(join)
    }

    /// Give a host or key the id `new` and repoint whatever referred to it.
    ///
    /// A host in `still_shared` is left as it is: it belongs to a team vault
    /// this account still sees, and pointing it at a personal copy would
    /// break it for everyone else there.
    fn give_new_id(&mut self, old: Uuid, new: Uuid, still_shared: &BTreeSet<Uuid>) {
        // The copy is here already: another device of the account made it,
        // or this vault was lost before. That one stays and this one goes.
        let is_kept_already =
            self.keys.iter().any(|key| key.id == new) || self.entries.iter().any(|e| e.id == new);
        if is_kept_already {
            self.keys.retain(|key| key.id != old);
            self.entries.retain(|entry| entry.id != old);
        }
        for key in &mut self.keys {
            if key.id == old {
                key.id = new;
            }
        }
        for entry in &mut self.entries {
            if entry.id == old {
                entry.id = new;
            }
            if still_shared.contains(&entry.id) {
                continue;
            }
            if entry.jump_host == Some(old) {
                entry.jump_host = Some(new);
            }
            if matches!(entry.auth, AuthMethod::PrivateKey { key_id } if key_id == old) {
                entry.auth = AuthMethod::PrivateKey { key_id: new };
            }
        }
        for id in &mut self.proxy_routing.server_set {
            if *id == old {
                *id = new;
            }
        }
    }

    /// Whether a host here still sits in this folder or carries this tag.
    fn is_label_in_use(&self, id: Uuid) -> bool {
        self.entries
            .iter()
            .any(|e| e.group == Some(id) || e.tags.contains(&id))
            || self.groups.iter().any(|g| g.parent_id == Some(id))
    }

    /// A rule nobody has set is not worth an item, and sending it from a new
    /// device would fight the rule already on the server.
    fn has_proxy_rule(&self) -> bool {
        let rule = &self.proxy_routing;
        let is_untouched = rule.default_proxy.is_none() && rule.server_set.is_empty();
        let is_known = self
            .sync
            .as_ref()
            .is_some_and(|s| s.personal.items.contains_key(&PROXY_ROUTING_ID));
        is_known || !is_untouched
    }

    fn put_record(&mut self, record: Record) {
        match record {
            Record::Entry(mut entry) => {
                if let Some(mine) = self.entries.iter().find(|e| e.id == entry.id) {
                    entry.last_opened_at = mine.last_opened_at;
                    entry.last_remote_dir = mine.last_remote_dir.clone();
                    entry.last_local_dir = mine.last_local_dir.clone();
                }
                upsert(&mut self.entries, entry, |e| e.id);
            }
            Record::Snippet(mut snippet) => {
                if let Some(mine) = self.snippets.iter().find(|s| s.id == snippet.id) {
                    snippet.usages = mine.usages.clone();
                }
                upsert(&mut self.snippets, snippet, |s| s.id);
            }
            Record::Key(key) => upsert(&mut self.keys, key, |k| k.id),
            Record::Group(group) => upsert(&mut self.groups, group, |g| g.id),
            Record::Tag(tag) => upsert(&mut self.tags, tag, |t| t.id),
            Record::Proxy(proxy) => upsert(&mut self.proxies, proxy, |p| p.id),
            Record::ProxyRouting(rule) => self.proxy_routing = rule,
            Record::Team(pin) => upsert(&mut self.team_pins, pin, |p| p.team_id),
            Record::Join(join) => upsert(&mut self.joins, join, |j| j.id),
            Record::Deleted => {}
        }
    }

    fn remove_record(&mut self, id: Uuid) {
        if id == PROXY_ROUTING_ID {
            self.proxy_routing = ProxyRouting::default();
            return;
        }
        self.entries.retain(|e| e.id != id);
        self.snippets.retain(|s| s.id != id);
        self.keys.retain(|k| k.id != id);
        self.groups.retain(|g| g.id != id);
        self.tags.retain(|t| t.id != id);
        self.proxies.retain(|p| p.id != id);
        self.team_pins.retain(|p| p.team_id != id);
        self.joins.retain(|j| j.id != id);
    }
}

fn is_edited(ledger: &Ledger, records: &BTreeMap<Uuid, Record>) -> bool {
    ledger.items.keys().any(|id| !records.contains_key(id))
        || records.iter().any(|(id, record)| {
            let is_edited = ledger
                .items
                .get(id)
                .is_none_or(|known| known.hash != record.hash());
            is_edited && !record.is_too_large()
        })
}

/// The changes that bring the server's copy of a vault up to `records`.
fn outgoing(
    ledger: &Ledger,
    records: &BTreeMap<Uuid, Record>,
    item_key: &[u8; 32],
) -> Vec<OutgoingChange> {
    let mut changes = Vec::new();
    for (id, record) in records {
        let known = ledger.items.get(id);
        let hash = record.hash();
        if known.is_some_and(|known| known.hash == hash) {
            continue;
        }
        if record.is_too_large() {
            tracing::warn!(%id, "record is too large to sync, skipping it");
            continue;
        }
        let blob = record.seal(item_key, *id);
        let base_rev = known.map_or(0, |known| known.rev);
        changes.push(OutgoingChange {
            id: *id,
            base_rev,
            is_deleted: false,
            blob,
            hash,
        });
    }
    for (id, known) in &ledger.items {
        if records.contains_key(id) {
            continue;
        }
        changes.push(OutgoingChange {
            id: *id,
            base_rev: known.rev,
            is_deleted: true,
            blob: Record::Deleted.seal(item_key, *id),
            hash: [0; 32],
        });
    }
    changes
}

/// Re-encrypt one item of a team vault under the vault's next key.
///
/// An item that does not open under the old key is replaced by a deletion
/// marker. Refusing instead would let one damaged item, which any member can
/// write, stop the team from ever changing its key.
pub fn reseal_item(old_key: &[u8; 32], new_key: &[u8; 32], id: Uuid, blob: &[u8]) -> Vec<u8> {
    let old_item_key = crypto::derive_subkey(old_key, ITEM_KEY_PURPOSE);
    let new_item_key = crypto::derive_subkey(new_key, ITEM_KEY_PURPOSE);
    let record = Record::open(&old_item_key, id, blob).unwrap_or_else(|| {
        tracing::warn!(%id, "an item does not open under the old team key and is dropped");
        Record::Deleted
    });
    record.seal(&new_item_key, id)
}

fn shared_entry(entry: &ServerEntry) -> Record {
    let mut entry = entry.clone();
    entry.last_opened_at = None;
    entry.last_remote_dir = None;
    entry.last_local_dir = None;
    Record::Entry(entry)
}

fn shared_snippet(snippet: &Snippet) -> Record {
    let mut snippet = snippet.clone();
    snippet.usages.clear();
    Record::Snippet(snippet)
}

fn upsert<T>(records: &mut Vec<T>, record: T, id: impl Fn(&T) -> Uuid) {
    match records.iter().position(|r| id(r) == id(&record)) {
        Some(position) => records[position] = record,
        None => records.push(record),
    }
}

/// What one sync round sends. Made by [`VaultManager::begin_sync`].
pub struct SyncJob {
    pub session_token: String,
    pub vault_id: Uuid,
    /// The version of the vault key the changes are encrypted under.
    pub key_version: u64,
    pub cursor: u64,
    pub changes: Vec<OutgoingChange>,
}

pub struct OutgoingChange {
    pub id: Uuid,
    /// The server revision this edit was made on top of; 0 for a new item.
    pub base_rev: u64,
    pub is_deleted: bool,
    pub blob: Vec<u8>,
    hash: [u8; 32],
}

impl OutgoingChange {
    pub fn accepted_at(&self, rev: u64) -> PushedItem {
        PushedItem {
            id: self.id,
            rev,
            is_deleted: self.is_deleted,
            hash: self.hash,
        }
    }
}

pub struct PulledItem {
    pub id: Uuid,
    pub rev: u64,
    pub key_version: u64,
    pub is_deleted: bool,
    pub blob: Vec<u8>,
}

pub struct PushedItem {
    id: Uuid,
    rev: u64,
    is_deleted: bool,
    hash: [u8; 32],
}

/// What the server answered to a [`SyncJob`].
pub struct SyncResult {
    pub vault_id: Uuid,
    /// The cursor the round started from.
    pub started_at: u64,
    /// In revision order.
    pub pulled: Vec<PulledItem>,
    pub cursor: u64,
    pub pushed: Vec<PushedItem>,
}

pub struct TeamContext {
    pub session_token: String,
    pub user_id: Uuid,
    pub key_seed: Option<Zeroizing<[u8; 32]>>,
    /// Wraps the key seed for the copy the server keeps.
    pub seed_key: Zeroizing<[u8; 32]>,
    pub teams: BTreeMap<Uuid, TeamState>,
    pub pins: Vec<TeamPin>,
    pub joins: Vec<PendingJoin>,
    /// Team id and vault key, for every team vault this device can open.
    pub vault_keys: Vec<(Uuid, VaultKey)>,
    pub invites: BTreeMap<Uuid, InviteSecret>,
}

/// One team after its roster chain was checked.
pub struct TeamUpdate {
    pub team_id: Uuid,
    pub state: TeamState,
    pub vaults: Vec<VaultKey>,
    /// Set when this check is the one that first trusted the team.
    pub pin: Option<TeamPin>,
}

pub struct TeamsUpdate {
    pub key_seed: Option<[u8; 32]>,
    pub teams: Vec<TeamUpdate>,
    /// The teams this device knew when the check started.
    pub known: Vec<Uuid>,
    /// Every team the server lists for the account.
    pub listed: Vec<Uuid>,
    /// The listed teams that passed their check.
    pub checked: Vec<Uuid>,
    /// Invites made here that the server no longer holds: used up or expired.
    pub closed_invites: Vec<Uuid>,
}

/// Keys for telling the server about a recovery done on this device.
pub struct RecoveryJob {
    pub old_recovery_auth_key: [u8; 32],
    pub new_auth_key: [u8; 32],
    pub new_recovery_auth_key: [u8; 32],
    pub kdf_salt: [u8; 32],
    pub kdf_params: KdfParams,
    pub enc_dek_by_password: AeadBlob,
    pub enc_dek_by_recovery: AeadBlob,
}

/// A new account made from this vault, ready for
/// [`VaultManager::install_registration`].
pub struct Registration {
    /// The password slot the account was made from. A vault whose password
    /// has changed since must not take it.
    pub replaces: AeadBlob,
    pub enc_dek_by_recovery: AeadBlob,
    pub state: SyncState,
}

/// An existing account this vault switches to, ready for
/// [`VaultManager::adopt_account`].
pub struct AdoptedAccount {
    pub dek: Zeroizing<[u8; 32]>,
    pub kdf_salt: [u8; 32],
    pub kdf_params: KdfParams,
    pub enc_dek_by_password: AeadBlob,
    pub enc_dek_by_recovery: AeadBlob,
    pub state: SyncState,
}

impl VaultManager {
    pub fn sync_state(&self) -> Option<&SyncState> {
        self.payload.sync.as_ref()
    }

    /// The key items of a vault are encrypted under: made from the vault key
    /// for the personal vault, and from the team's key for a team vault.
    fn item_key(&self, team_key: Option<&[u8; 32]>) -> Zeroizing<[u8; 32]> {
        let key = team_key.unwrap_or(self.dek.as_ref());
        crypto::derive_subkey(key, ITEM_KEY_PURPOSE)
    }

    /// Whether any record differs from what the server has.
    pub fn has_unsynced_changes(&self) -> bool {
        let Some(state) = &self.payload.sync else {
            return false;
        };
        is_edited(&state.personal, &self.payload.records_in(None))
            || state
                .team_vaults
                .iter()
                .any(|(id, vault)| is_edited(&vault.ledger, &self.payload.records_in(Some(*id))))
    }

    /// Everything that changed here since the last sync, encrypted and ready
    /// to send: one job for the personal vault, then one per team vault.
    /// Empty when the vault is not signed in.
    pub fn begin_sync(&self) -> Vec<SyncJob> {
        let Some(state) = &self.payload.sync else {
            return Vec::new();
        };
        let Some(session_token) = state.session_token.clone() else {
            return Vec::new();
        };

        let mut jobs = vec![SyncJob {
            session_token: session_token.clone(),
            vault_id: state.vault_id,
            key_version: 1,
            cursor: state.personal.cursor,
            changes: outgoing(
                &state.personal,
                &self.payload.records_in(None),
                &self.item_key(None),
            ),
        }];
        for (vault_id, vault) in &state.team_vaults {
            jobs.push(SyncJob {
                session_token: session_token.clone(),
                vault_id: *vault_id,
                key_version: vault.key_version,
                cursor: vault.ledger.cursor,
                changes: outgoing(
                    &vault.ledger,
                    &self.payload.records_in(Some(*vault_id)),
                    &self.item_key(Some(&vault.key)),
                ),
            });
        }
        jobs
    }

    /// Take in the answers of one round, the personal vault's first, and
    /// note what the server accepted. Returns how many records changed here.
    ///
    /// Records may have been edited while the round was running, so nothing
    /// from `begin_sync` is trusted: every decision looks at the vault as it
    /// is now.
    ///
    /// A host moved between two vaults leaves one and arrives in the other,
    /// and the answers come in no useful order. So an item that has to wait
    /// for its host to leave is tried once more after every vault was seen.
    pub fn finish_round(&mut self, results: Vec<SyncResult>) -> Result<usize, VaultError> {
        let mut changed = 0;
        let mut waiting = Vec::new();
        for result in results {
            let (count, held) = self.take_answer(result)?;
            changed += count;
            waiting.extend(held);
        }
        for result in waiting {
            changed += self.take_answer(result)?.0;
        }
        Ok(changed)
    }

    /// Apply one answer. Returns how many records changed, and the items
    /// that have to wait, as an answer that can be taken again.
    fn take_answer(
        &mut self,
        result: SyncResult,
    ) -> Result<(usize, Option<SyncResult>), VaultError> {
        let Some(state) = &mut self.payload.sync else {
            return Ok((0, None));
        };
        let team_key = state
            .team_vaults
            .get(&result.vault_id)
            .map(|vault| (vault.key, vault.key_version));
        let scope = team_key.map(|_| result.vault_id);
        // Signed out, or in again, or out of the team while the round was
        // running: its answer describes a state this vault no longer has.
        let Some(ledger) = state.ledger_mut(result.vault_id) else {
            return Ok((0, None));
        };
        if ledger.cursor != result.started_at {
            return Ok((0, None));
        }
        // Items under a key this device does not have yet cannot be read, and
        // skipping them would lose them for good. Catch up on the team first.
        let key_version = team_key.map_or(1, |(_, key_version)| key_version);
        if result
            .pulled
            .iter()
            .any(|item| item.key_version != key_version)
        {
            state.is_behind_on_teams = true;
            return Ok((0, None));
        }
        let mut ledger = std::mem::take(ledger);
        let item_key = self.item_key(team_key.as_ref().map(|(key, _)| key));

        let mut changed = 0;
        let mut held = Vec::new();
        for item in result.pulled {
            if ledger
                .items
                .get(&item.id)
                .is_some_and(|known| known.rev >= item.rev)
            {
                continue; // our own change coming back
            }
            if self.lives_elsewhere(&ledger, scope, &item) {
                tracing::debug!(vault = %result.vault_id, id = %item.id, "a team item waits for its record to leave another vault");
                held.push(item);
                continue;
            }
            if self.take_in(&mut ledger, scope, &item_key, &item) {
                changed += 1;
            }
        }

        let has_pushed = !result.pushed.is_empty();
        for pushed in result.pushed {
            if pushed.is_deleted {
                ledger.items.remove(&pushed.id);
            } else {
                ledger.items.insert(
                    pushed.id,
                    SyncedItem {
                        rev: pushed.rev,
                        hash: pushed.hash,
                    },
                );
            }
        }
        // The cursor stops short of an item that waits, so the server sends
        // it again. The rest of the vault goes on.
        let before_held = held.iter().map(|item| item.rev.saturating_sub(1)).min();
        let reached = before_held.map_or(result.cursor, |rev| rev.min(result.cursor));
        ledger.cursor = ledger.cursor.max(reached);
        let started_at = ledger.cursor;

        self.put_back(result.vault_id, ledger);
        // Most rounds bring nothing new, and a save flushes the disk.
        if changed > 0 || has_pushed || started_at != result.started_at {
            self.save()?;
        }
        let held = (!held.is_empty()).then(|| SyncResult {
            vault_id: result.vault_id,
            started_at,
            pulled: held,
            cursor: result.cursor,
            pushed: Vec::new(),
        });
        Ok((changed, held))
    }

    /// A team item under the id of a record that lives elsewhere and was
    /// never in this vault. From this account's other device, or from another
    /// vault of the team, it is a host on its way in, and its deletion from
    /// the old vault is still to come. From another member it would pull a
    /// host of ours into the team. Wait and see: the item is not taken until
    /// the record has left its place.
    fn lives_elsewhere(&self, ledger: &Ledger, scope: Option<Uuid>, item: &PulledItem) -> bool {
        let home = self
            .payload
            .sync
            .as_ref()
            .and_then(|s| s.shared.get(&item.id))
            .copied();
        scope.is_some()
            && !item.is_deleted
            && !ledger.items.contains_key(&item.id)
            && home != scope
            && self.payload.record(item.id).is_some()
    }

    fn put_back(&mut self, vault_id: Uuid, ledger: Ledger) {
        let slot = self
            .payload
            .sync
            .as_mut()
            .and_then(|state| state.ledger_mut(vault_id));
        if let Some(slot) = slot {
            *slot = ledger;
        }
    }

    /// Apply one server item. Returns whether a record changed here.
    fn take_in(
        &mut self,
        ledger: &mut Ledger,
        scope: Option<Uuid>,
        item_key: &[u8; 32],
        item: &PulledItem,
    ) -> bool {
        let Some(theirs) = Record::open(item_key, item.id, &item.blob) else {
            tracing::warn!(id = %item.id, "skipping a synced item this build cannot read");
            return false;
        };
        // The flag is not encrypted, so the server could set it on a live
        // item. Only the marker inside the blob deletes a record.
        if item.is_deleted != matches!(theirs, Record::Deleted) {
            tracing::warn!(id = %item.id, "skipping a synced item whose deleted flag does not match it");
            return false;
        }
        // A team vault holds hosts and their keys, nothing else.
        if scope.is_some() && !matches!(theirs, Record::Entry(_) | Record::Key(_) | Record::Deleted)
        {
            return false;
        }
        let mine = self.payload.record(item.id);
        let home = self
            .payload
            .sync
            .as_ref()
            .and_then(|state| state.shared.get(&item.id))
            .copied();

        if item.is_deleted {
            // Moved to another vault since: this deletion is the old copy going away.
            if home != scope {
                ledger.items.remove(&item.id);
                return false;
            }
            // Another device dropped a folder or tag it no longer used, while a
            // host here took it up. Keeping it marks it as edited on top of the
            // deletion, so the next round puts it back for everyone.
            //
            // The same goes for a record that is here and was never synced to
            // this vault: it came back after an older deletion, for example a
            // host taken out of a team into a vault it had left before.
            let is_back = mine.is_some() && !ledger.items.contains_key(&item.id);
            if is_back || (mine.is_some() && self.payload.is_label_in_use(item.id)) {
                ledger.items.insert(
                    item.id,
                    SyncedItem {
                        rev: item.rev,
                        hash: [0; 32],
                    },
                );
                return false;
            }
            ledger.items.remove(&item.id);
            self.payload.remove_record(item.id);
            if let Some(state) = &mut self.payload.sync {
                state.shared.remove(&item.id);
            }
            return mine.is_some();
        }

        let known_hash = ledger.items.get(&item.id).map(|known| known.hash);
        // The record left this vault here, and its deletion is still to be
        // sent: someone edited it meanwhile, or the vault got a new key. The
        // move stands, and the deletion goes on top of this revision.
        let has_left = home != scope && known_hash.is_some();
        // The version this device already knows, under a new revision: a new
        // vault key gives every item one. Nobody edited it there, so whatever
        // was done to it here, a deletion too, is sent on top.
        let is_unchanged_there = known_hash == Some(theirs.hash());
        if has_left || is_unchanged_there {
            ledger.items.insert(
                item.id,
                SyncedItem {
                    rev: item.rev,
                    hash: theirs.hash(),
                },
            );
            return false;
        }
        let is_edited_here = mine.as_ref().map(Record::hash) != known_hash;
        let keeps_mine = mine
            .as_ref()
            .is_some_and(|mine| is_edited_here && mine.is_newer_than(&theirs));
        if keeps_mine {
            // Remembering the server's version leaves the record marked as
            // edited, so the next round sends it on top of this revision.
            ledger.items.insert(
                item.id,
                SyncedItem {
                    rev: item.rev,
                    hash: theirs.hash(),
                },
            );
            return false;
        }

        self.payload.put_record(theirs);
        if let Some(state) = &mut self.payload.sync {
            match scope {
                Some(vault_id) => state.shared.insert(item.id, vault_id),
                None => state.shared.remove(&item.id),
            };
        }
        // Hashed as stored here, not as received: a field this build does not
        // know is dropped on the way in and must not read as a local edit.
        if let Some(stored) = self.payload.record(item.id) {
            ledger.items.insert(
                item.id,
                SyncedItem {
                    rev: item.rev,
                    hash: stored.hash(),
                },
            );
        }
        true
    }

    /// What the code that talks to the server needs to know about this
    /// account's teams. `None` when the vault is not signed in.
    pub fn team_context(&self) -> Option<TeamContext> {
        let state = self.payload.sync.as_ref()?;
        Some(TeamContext {
            session_token: state.session_token.clone()?,
            user_id: state.user_id,
            key_seed: state.key_seed.map(Zeroizing::new),
            seed_key: crypto::derive_subkey(self.dek.as_ref(), KEY_SEED_PURPOSE),
            teams: state.teams.clone(),
            pins: self.payload.team_pins.clone(),
            joins: self.payload.joins.clone(),
            vault_keys: state
                .team_vaults
                .iter()
                .map(|(id, vault)| {
                    let key = VaultKey {
                        vault_id: *id,
                        key: vault.key,
                        key_version: vault.key_version,
                    };
                    (vault.team_id, key)
                })
                .collect(),
            invites: state.invites.clone(),
        })
    }

    /// A team changed its keys and this device has to fetch them before the
    /// team's vaults can sync again.
    pub fn is_behind_on_teams(&self) -> bool {
        self.payload
            .sync
            .as_ref()
            .is_some_and(|state| state.is_behind_on_teams)
    }

    pub fn note_behind_on_teams(&mut self) {
        if let Some(state) = &mut self.payload.sync {
            state.is_behind_on_teams = true;
        }
    }

    /// Take in the teams as just checked against their rosters. A team the
    /// server no longer lists is dropped, and so is a vault of a checked
    /// team that came without a key: deleted, or no longer for this account.
    pub fn apply_teams(&mut self, update: TeamsUpdate) -> Result<(), VaultError> {
        let Some(state) = &mut self.payload.sync else {
            return Ok(());
        };
        state.is_behind_on_teams = false;
        if let Some(seed) = update.key_seed {
            state.key_seed = Some(seed);
        }

        let mut kept_vaults = Vec::new();
        for team in update.teams {
            for key in team.vaults {
                // A vault id is random. One that is already another vault's is
                // an attempt to put this team's key over that vault's.
                let is_taken = key.vault_id == state.vault_id
                    || state
                        .team_vaults
                        .get(&key.vault_id)
                        .is_some_and(|v| v.team_id != team.team_id);
                if is_taken {
                    tracing::warn!(team = %team.team_id, "ignoring a team vault with a taken id");
                    continue;
                }
                kept_vaults.push(key.vault_id);
                let vault = state
                    .team_vaults
                    .entry(key.vault_id)
                    .or_insert_with(|| TeamVault {
                        team_id: team.team_id,
                        key: key.key,
                        key_version: key.key_version,
                        ledger: Ledger::default(),
                    });
                vault.key = key.key;
                vault.key_version = key.key_version;
            }
            state.teams.insert(team.team_id, team.state);
            if let Some(pin) = team.pin {
                self.payload
                    .joins
                    .retain(|join| join.team_id != pin.team_id);
                let is_newer = self
                    .payload
                    .team_pins
                    .iter()
                    .all(|old| old.team_id != pin.team_id || old.seq < pin.seq);
                if is_newer {
                    upsert(&mut self.payload.team_pins, pin, |p| p.team_id);
                }
            }
        }

        // A team goes only when the check knew it and the server no longer
        // lists it. One that failed its check stays as it was, and so does
        // one this device learned of while the check was running.
        let is_gone = |id: &Uuid| update.known.contains(id) && !update.listed.contains(id);
        state.teams.retain(|id, _| !is_gone(id));
        state
            .invites
            .retain(|id, invite| !is_gone(&invite.team_id) && !update.closed_invites.contains(id));
        self.payload.team_pins.retain(|pin| !is_gone(&pin.team_id));
        let teams = &state.teams;
        state.team_vaults.retain(|id, vault| {
            teams.contains_key(&vault.team_id)
                && (kept_vaults.contains(id) || !update.checked.contains(&vault.team_id))
        });

        // What a lost vault held stays here as personal records, whether the
        // whole team is gone or only this account's access to one vault. The
        // account could read them until now, so removing them would protect
        // nothing, and some of them were this user's own hosts and keys.
        //
        // Each gets a new id. Under the old one the team may still hold the
        // record, and if this account is let back in, the team's copy must
        // not be mistaken for the personal one. The new id is made from the
        // old one, so every device of the account ends up with the same copy.
        let vaults = &state.team_vaults;
        let lost: Vec<Uuid> = state
            .shared
            .iter()
            .filter(|(_, v)| !vaults.contains_key(v))
            .map(|(id, _)| *id)
            .collect();
        state
            .shared
            .retain(|_, vault_id| vaults.contains_key(vault_id));
        let still_shared: BTreeSet<Uuid> = state.shared.keys().copied().collect();
        for id in lost {
            let copy_id = self.personal_copy_id(id);
            self.payload.give_new_id(id, copy_id, &still_shared);
            self.note_renamed(id, copy_id);
        }
        self.save()
    }

    fn note_renamed(&mut self, old: Uuid, new: Uuid) {
        let Some(state) = &mut self.payload.sync else {
            return;
        };
        // A record renamed before and now again: its first name follows it.
        for current in state.renamed.values_mut() {
            if *current == old {
                *current = new;
            }
        }
        state.renamed.insert(old, new);
    }

    /// The id a host or key goes by now. It differs from `id` only for a
    /// record that was renamed when its team vault was lost; whoever still
    /// holds the old id, such as an open session, reaches the record with it.
    pub fn current_id(&self, id: Uuid) -> Uuid {
        // Let back into the team, the account sees the team's record under
        // the old id again. That one is then meant, not the personal copy.
        let is_live = self.payload.entries.iter().any(|entry| entry.id == id)
            || self.payload.keys.iter().any(|key| key.id == id);
        if is_live {
            return id;
        }
        let renamed = self
            .payload
            .sync
            .as_ref()
            .and_then(|state| state.renamed.get(&id));
        renamed.copied().unwrap_or(id)
    }

    /// The id a record of a lost team vault goes on under. It is made with
    /// the account's key: the same on each of its devices, and not known to
    /// the team, who could otherwise write an item under it in advance.
    fn personal_copy_id(&self, id: Uuid) -> Uuid {
        let key = crypto::derive_subkey(self.dek.as_ref(), COPY_ID_PURPOSE);
        let digest = Sha256::new()
            .chain_update(key.as_ref())
            .chain_update(id.as_bytes())
            .finalize();
        let mut bytes = [0; 16];
        bytes.copy_from_slice(&digest[..16]);
        uuid::Builder::from_random_bytes(bytes).into_uuid()
    }

    pub fn set_key_seed(&mut self, seed: [u8; 32]) -> Result<(), VaultError> {
        if let Some(state) = &mut self.payload.sync {
            state.key_seed = Some(seed);
        }
        self.save()
    }

    /// Take in a team this device just created, next to the ones it has.
    pub fn add_team(&mut self, team: TeamUpdate) -> Result<(), VaultError> {
        let checked = vec![team.team_id];
        self.apply_teams(TeamsUpdate {
            key_seed: None,
            teams: vec![team],
            known: Vec::new(),
            listed: checked.clone(),
            checked,
            closed_invites: Vec::new(),
        })
    }

    pub fn pending_joins(&self) -> usize {
        self.payload.joins.len()
    }

    pub fn add_invite(&mut self, id: Uuid, invite: InviteSecret) -> Result<(), VaultError> {
        if let Some(state) = &mut self.payload.sync {
            state.invites.insert(id, invite);
        }
        self.save()
    }

    pub fn add_join(&mut self, join: PendingJoin) -> Result<(), VaultError> {
        self.payload
            .joins
            .retain(|other| other.team_id != join.team_id);
        self.payload.joins.push(join);
        self.save()
    }

    /// The vaults of a team this device holds a key for, in the roster's
    /// order, each with the name it is shown by.
    pub fn team_vaults(&self, team_id: Uuid) -> Vec<(Uuid, String)> {
        let Some(state) = &self.payload.sync else {
            return Vec::new();
        };
        let Some(team) = state.teams.get(&team_id) else {
            return Vec::new();
        };
        let held = |id: &Uuid| {
            state
                .team_vaults
                .get(id)
                .is_some_and(|vault| vault.team_id == team_id)
        };
        let listed: Vec<Uuid> = team.vault_names.iter().map(|(id, _)| *id).collect();
        // A vault the roster does not name yet comes after the named ones.
        let unlisted = state
            .team_vaults
            .keys()
            .copied()
            .filter(|id| !listed.contains(id));
        let ids: Vec<Uuid> = listed
            .iter()
            .copied()
            .chain(unlisted)
            .filter(held)
            .collect();
        ids.into_iter()
            .map(|id| (id, team.vault_name(id)))
            .collect()
    }

    /// Every team vault a host can be put into from this device: team id,
    /// vault id and the label of the vault. Teams by name, then the vaults
    /// in the roster's order.
    pub fn sharing_targets(&self) -> Vec<(Uuid, Uuid, String)> {
        let Some(state) = &self.payload.sync else {
            return Vec::new();
        };
        let mut teams: Vec<(&Uuid, &TeamState)> = state.teams.iter().collect();
        teams.sort_by_key(|(_, team)| team.name.to_lowercase());
        let mut targets = Vec::new();
        for (team_id, team) in teams {
            for (vault_id, _) in self.team_vaults(*team_id) {
                targets.push((*team_id, vault_id, team.vault_label(vault_id)));
            }
        }
        targets
    }

    /// The team vault a host is shared in, if any.
    pub fn vault_of(&self, entry_id: Uuid) -> Option<Uuid> {
        self.payload.sync.as_ref()?.shared.get(&entry_id).copied()
    }

    /// The team a host is shared with, if any.
    pub fn team_of(&self, entry_id: Uuid) -> Option<Uuid> {
        let state = self.payload.sync.as_ref()?;
        state
            .team_vaults
            .get(&self.vault_of(entry_id)?)
            .map(|vault| vault.team_id)
    }

    /// Where a shared host is, as shown next to it: "team", or "team / vault"
    /// when the team has several vaults. `None` for a personal host.
    pub fn sharing_label(&self, entry_id: Uuid) -> Option<String> {
        let vault_id = self.vault_of(entry_id)?;
        let team = self
            .payload
            .sync
            .as_ref()?
            .teams
            .get(&self.team_of(entry_id)?)?;
        Some(team.vault_label(vault_id))
    }

    /// Put a host into a team vault, with the key it logs in with. Everyone
    /// who can open that vault then sees both. A host that was in another
    /// vault leaves it.
    pub fn share_entry(&mut self, entry_id: Uuid, vault_id: Uuid) -> Result<(), VaultError> {
        let entry = self.get_entry(entry_id)?;
        let Some(state) = &mut self.payload.sync else {
            return Ok(());
        };
        if !state.team_vaults.contains_key(&vault_id) {
            return Ok(());
        }
        state.shared.insert(entry_id, vault_id);
        if let AuthMethod::PrivateKey { key_id } = entry.auth {
            self.bring_key(entry_id, key_id, vault_id);
        }
        self.save()
    }

    /// The key that saving `entry` would hand to its team, with the name of
    /// the key and of the team vault. `None` for a personal host, a host that
    /// does not log in with a key, and a key the team vault already holds.
    pub fn key_shared_by_saving(&self, entry: &ServerEntry) -> Option<(String, String)> {
        let AuthMethod::PrivateKey { key_id } = entry.auth else {
            return None;
        };
        let id = self.current_id(entry.id);
        let vault_id = self.vault_of(id)?;
        if self.payload.sync.as_ref()?.shared.get(&key_id) == Some(&vault_id) {
            return None;
        }
        let key = self.payload.keys.iter().find(|key| key.id == key_id)?;
        Some((key.name.clone(), self.sharing_label(id)?))
    }

    /// Put the key a shared host logs in with into the host's vault.
    ///
    /// A personal key moves there. A key that is in another team vault never
    /// leaves it, because the people who see that vault may use it. The host
    /// then takes a copy of the key along. So whoever sees a host sees its key.
    pub(super) fn bring_key(&mut self, entry_id: Uuid, key_id: Uuid, vault_id: Uuid) {
        let Some(state) = &mut self.payload.sync else {
            return;
        };
        let Some(key) = self
            .payload
            .keys
            .iter()
            .find(|key| key.id == key_id)
            .cloned()
        else {
            return;
        };
        match state.shared.get(&key_id) {
            None => {
                state.shared.insert(key_id, vault_id);
                return;
            }
            Some(home) if *home == vault_id => return,
            Some(_) => {}
        }

        // An earlier move may have left the same key in this vault already.
        let is_twin = |other: &&SshKey| {
            state.shared.get(&other.id) == Some(&vault_id)
                && other.pem == key.pem
                && other.passphrase == key.passphrase
        };
        let twin = self.payload.keys.iter().find(is_twin).map(|other| other.id);
        let copy_id = twin.unwrap_or_else(|| {
            let copy = SshKey {
                id: Uuid::new_v4(),
                ..key
            };
            state.shared.insert(copy.id, vault_id);
            self.payload.keys.push(copy.clone());
            copy.id
        });
        if let Some(entry) = self
            .payload
            .entries
            .iter_mut()
            .find(|entry| entry.id == entry_id)
        {
            entry.auth = AuthMethod::PrivateKey { key_id: copy_id };
        }
    }

    /// Take a host back out of its team. The key stays with the team: other
    /// shared hosts may use it.
    pub fn unshare_entry(&mut self, entry_id: Uuid) -> Result<(), VaultError> {
        if let Some(state) = &mut self.payload.sync {
            state.shared.remove(&entry_id);
        }
        self.save()
    }

    /// The server refused the session. The account stays known, so signing in
    /// again picks up where sync stopped.
    ///
    /// `session_token` is the one that was refused: an answer to an older
    /// session must not end the one opened since.
    pub fn end_session(&mut self, session_token: &str) -> Result<(), VaultError> {
        let Some(state) = &mut self.payload.sync else {
            return Ok(());
        };
        if state.session_token.as_deref() != Some(session_token) {
            return Ok(());
        }
        state.session_token = None;
        self.save()
    }

    /// Forget the account. Every record stays in the vault.
    pub fn sign_out(&mut self) -> Result<(), VaultError> {
        self.payload.sync = None;
        self.save()
    }

    /// Attach the account just made from this vault. The recovery phrase is
    /// replaced by the one the account was registered with.
    pub fn install_registration(&mut self, registration: Registration) -> Result<(), VaultError> {
        if registration.replaces != self.enc_dek_by_password {
            return Err(VaultError::StalePasswordSlot);
        }
        let old_recovery = std::mem::replace(
            &mut self.enc_dek_by_recovery,
            registration.enc_dek_by_recovery,
        );
        let old_sync = self.payload.sync.replace(registration.state);

        let saved = self.save();
        if saved.is_err() {
            self.enc_dek_by_recovery = old_recovery;
            self.payload.sync = old_sync;
        }
        saved
    }

    /// Switch this vault to an existing account: its key, password and
    /// recovery phrase replace the local ones. Records already here stay and
    /// are sent to the account on the next sync.
    pub fn adopt_account(&mut self, account: AdoptedAccount) -> Result<(), VaultError> {
        let mut state = account.state;
        // Signing in again to the same account continues from where it was.
        if let Some(old) = &self.payload.sync {
            if old.vault_id == state.vault_id {
                let session_token = state.session_token.take();
                state = SyncState {
                    session_token,
                    ..old.clone()
                };
            }
        }

        let old_dek = Zeroizing::new(*self.dek);
        let old_salt = std::mem::replace(&mut self.kdf_salt, account.kdf_salt);
        let old_params = std::mem::replace(&mut self.kdf_params, account.kdf_params);
        let old_password =
            std::mem::replace(&mut self.enc_dek_by_password, account.enc_dek_by_password);
        let old_recovery =
            std::mem::replace(&mut self.enc_dek_by_recovery, account.enc_dek_by_recovery);
        let old_sync = self.payload.sync.replace(state);
        // Written in place: the box is the page that is locked against swapping.
        *self.dek = *account.dek;

        let saved = self.save();
        if saved.is_err() {
            *self.dek = *old_dek;
            self.kdf_salt = old_salt;
            self.kdf_params = old_params;
            self.enc_dek_by_password = old_password;
            self.enc_dek_by_recovery = old_recovery;
            self.payload.sync = old_sync;
        }
        saved
    }

    /// Opens with the account password and recovery phrase.
    pub fn create_from_account(path: &Path, account: AdoptedAccount) -> Result<Self, VaultError> {
        Self::write_new(
            path,
            &account.dek,
            account.kdf_salt,
            account.kdf_params,
            account.enc_dek_by_password,
            account.enc_dek_by_recovery,
            Some(account.state),
        )
    }

    /// Borrows `keys`, so a failed write can retry with the same phrase.
    pub fn create_registered(
        path: &Path,
        keys: &NewVaultKeys,
        state: &SyncState,
    ) -> Result<Self, VaultError> {
        Self::write_new(
            path,
            &keys.dek,
            keys.kdf_salt,
            keys.kdf_params.clone(),
            keys.enc_dek_by_password.clone(),
            keys.enc_dek_by_recovery.clone(),
            Some(state.clone()),
        )
    }

    /// Record that the vault was just recovered with the phrase, so the account
    /// can be told. Called before the first save under the new password.
    pub(super) fn note_recovery(
        payload: &mut VaultPayload,
        old_recovery_key: &[u8; 32],
        new_kek: &[u8; 32],
        new_recovery_key: &[u8; 32],
    ) {
        let Some(state) = &mut payload.sync else {
            return;
        };
        // A second recovery before the server heard of the first: the server
        // still knows only the oldest phrase.
        let old_recovery_auth_key = match &state.pending_recovery {
            Some(pending) => pending.old_recovery_auth_key,
            None => recovery_auth_key(old_recovery_key),
        };
        state.pending_recovery = Some(PendingRecovery {
            old_recovery_auth_key,
            new_auth_key: auth_key(new_kek),
            new_recovery_auth_key: recovery_auth_key(new_recovery_key),
        });
    }

    /// `Some` when the account still has to hear about a recovery.
    pub fn recovery_job(&self) -> Option<RecoveryJob> {
        let pending = self.payload.sync.as_ref()?.pending_recovery.as_ref()?;
        Some(RecoveryJob {
            old_recovery_auth_key: pending.old_recovery_auth_key,
            new_auth_key: pending.new_auth_key,
            new_recovery_auth_key: pending.new_recovery_auth_key,
            kdf_salt: self.kdf_salt,
            kdf_params: self.kdf_params.clone(),
            enc_dek_by_password: self.enc_dek_by_password.clone(),
            enc_dek_by_recovery: self.enc_dek_by_recovery.clone(),
        })
    }

    /// The account took the recovery and opened `session_token`; or, with
    /// `None`, it did not know the old phrase and the user has to sign in.
    pub fn finish_recovery(&mut self, session_token: Option<String>) -> Result<(), VaultError> {
        if let Some(state) = &mut self.payload.sync {
            state.pending_recovery = None;
            state.session_token = session_token;
        }
        self.save()
    }
}

#[cfg(test)]
mod tests {
    use secrecy::SecretString;

    use super::*;
    use crate::vault::RecoveryPhrase;

    fn fast_kdf() -> KdfParams {
        KdfParams {
            m_cost: 256,
            t_cost: 1,
            p_cost: 1,
        }
    }

    /// A vault signed in to a made-up account.
    fn synced_vault(dir: &tempfile::TempDir, name: &str) -> VaultManager {
        let path = dir.path().join(name);
        let (mut vault, _) =
            VaultManager::create_with_params(&path, &SecretString::new("pw".into()), fast_kdf())
                .unwrap();
        vault.payload.sync = Some(SyncState::new(
            Uuid::nil(),
            "alice".into(),
            "token".into(),
            Uuid::nil(),
        ));
        vault
    }

    /// A second device of the same account: the same key, its own file.
    fn second_device(dir: &tempfile::TempDir, first: &VaultManager) -> VaultManager {
        let mut vault = synced_vault(dir, "second.bin");
        *vault.dek = *first.dek;
        vault
    }

    fn host(label: &str) -> ServerEntry {
        ServerEntry {
            id: Uuid::new_v4(),
            label: label.into(),
            host: format!("{label}.example.com"),
            port: 22,
            username: "root".into(),
            auth: AuthMethod::Agent,
            notes: None,
            os: None,
            last_opened_at: None,
            host_key_fingerprint: None,
            last_remote_dir: None,
            last_local_dir: None,
            group: None,
            tags: Vec::new(),
            proxy: Default::default(),
            jump_host: None,
            forwards: Vec::new(),
            startup_snippet: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    /// A server that accepts everything, for moving changes between vaults.
    #[derive(Default)]
    struct FakeServer {
        latest_rev: u64,
        items: BTreeMap<Uuid, PulledItem>,
    }

    impl FakeServer {
        fn sync(&mut self, vault: &mut VaultManager) -> usize {
            let job = vault.begin_sync().remove(0);
            let result = self.answer(job);
            vault.finish_round(vec![result]).unwrap()
        }

        fn answer(&mut self, job: SyncJob) -> SyncResult {
            let mut pulled: Vec<PulledItem> = self
                .items
                .values()
                .filter(|item| item.rev > job.cursor)
                .map(|item| PulledItem {
                    id: item.id,
                    rev: item.rev,
                    key_version: 1,
                    is_deleted: item.is_deleted,
                    blob: item.blob.clone(),
                })
                .collect();
            pulled.sort_by_key(|item| item.rev);
            let cursor = self.latest_rev;

            let mut pushed = Vec::new();
            for change in &job.changes {
                let current_rev = self.items.get(&change.id).map_or(0, |item| item.rev);
                if current_rev != change.base_rev {
                    continue; // a conflict: the pull of the next round settles it
                }
                self.latest_rev += 1;
                self.items.insert(
                    change.id,
                    PulledItem {
                        id: change.id,
                        rev: self.latest_rev,
                        key_version: 1,
                        is_deleted: change.is_deleted,
                        blob: change.blob.clone(),
                    },
                );
                pushed.push(change.accepted_at(self.latest_rev));
            }

            SyncResult {
                vault_id: job.vault_id,
                started_at: job.cursor,
                pulled,
                cursor,
                pushed,
            }
        }

        /// What a new vault key does: every item gets a new revision.
        fn rotate(&mut self) {
            for item in self.items.values_mut() {
                self.latest_rev += 1;
                item.rev = self.latest_rev;
            }
        }
    }

    /// One fake server per vault, the personal one included.
    #[derive(Default)]
    struct FakeServers(BTreeMap<Uuid, FakeServer>);

    impl FakeServers {
        /// The answers of one round, in the order the jobs were made.
        fn answers(&mut self, vault: &VaultManager) -> Vec<SyncResult> {
            let jobs = vault.begin_sync();
            jobs.into_iter()
                .map(|job| self.0.entry(job.vault_id).or_default().answer(job))
                .collect()
        }

        fn sync(&mut self, vault: &mut VaultManager) {
            let answers = self.answers(vault);
            vault.finish_round(answers).unwrap();
        }
    }

    /// Put the vault in a team called "ops" that has these vaults, all of
    /// which this device holds the key for.
    fn join_team(vault: &mut VaultManager, team_id: Uuid, vaults: &[(Uuid, Option<&str>)]) {
        let held: Vec<Uuid> = vaults.iter().map(|(id, _)| *id).collect();
        see_team(vault, team_id, vaults, &held);
    }

    /// The same, with keys for the vaults in `held` only.
    fn see_team(
        vault: &mut VaultManager,
        team_id: Uuid,
        vaults: &[(Uuid, Option<&str>)],
        held: &[Uuid],
    ) {
        let state = TeamState {
            name: "ops".into(),
            roster: Vec::new(),
            logins: BTreeMap::new(),
            left: Vec::new(),
            vault_names: vaults
                .iter()
                .map(|(id, name)| (*id, name.map(str::to_string)))
                .collect(),
        };
        let keys = held.iter().map(|id| VaultKey {
            vault_id: *id,
            key: [7; 32],
            key_version: 1,
        });
        let team = TeamUpdate {
            team_id,
            state,
            vaults: keys.collect(),
            pin: None,
        };
        vault
            .apply_teams(TeamsUpdate {
                key_seed: None,
                teams: vec![team],
                known: vec![team_id],
                listed: vec![team_id],
                checked: vec![team_id],
                closed_invites: Vec::new(),
            })
            .unwrap();
    }

    fn ssh_key(name: &str) -> SshKey {
        SshKey {
            id: Uuid::new_v4(),
            name: name.into(),
            created_at: 1,
            fingerprint: "SHA256:x".into(),
            key_type: "ed25519".into(),
            pem: format!("pem of {name}"),
            passphrase: None,
        }
    }

    fn key_of(vault: &VaultManager, entry_id: Uuid) -> Uuid {
        match vault.get_entry(entry_id).unwrap().auth {
            AuthMethod::PrivateKey { key_id } => key_id,
            _ => panic!("the host does not log in with a key"),
        }
    }

    #[test]
    fn a_shared_host_is_labelled_with_its_vault_only_when_the_team_has_several() {
        let dir = tempfile::tempdir().unwrap();
        let mut vault = synced_vault(&dir, "first.bin");
        let (team, general, servers) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let (web, db) = (host("web"), host("db"));
        vault.add_entry(web.clone()).unwrap();
        vault.add_entry(db.clone()).unwrap();

        join_team(&mut vault, team, &[(general, None)]);
        vault.share_entry(web.id, general).unwrap();
        assert_eq!(vault.sharing_label(web.id).as_deref(), Some("ops"));
        assert_eq!(vault.sharing_label(db.id), None);

        join_team(
            &mut vault,
            team,
            &[(general, None), (servers, Some("Servers"))],
        );
        vault.share_entry(db.id, servers).unwrap();
        assert_eq!(
            vault.sharing_label(web.id).as_deref(),
            Some("ops / General")
        );
        assert_eq!(vault.sharing_label(db.id).as_deref(), Some("ops / Servers"));
        let names: Vec<String> = vault
            .team_vaults(team)
            .into_iter()
            .map(|(_, name)| name)
            .collect();
        assert_eq!(names, ["General", "Servers"]);
        let summary = vault
            .list_entries()
            .unwrap()
            .into_iter()
            .find(|e| e.id == db.id)
            .unwrap();
        assert_eq!(summary.team_name.as_deref(), Some("ops / Servers"));
    }

    #[test]
    fn a_host_is_not_shared_into_a_vault_this_device_cannot_open() {
        let dir = tempfile::tempdir().unwrap();
        let mut vault = synced_vault(&dir, "first.bin");
        let web = host("web");
        vault.add_entry(web.clone()).unwrap();

        vault.share_entry(web.id, Uuid::new_v4()).unwrap();
        assert_eq!(vault.vault_of(web.id), None);
    }

    #[test]
    fn a_host_moved_to_another_vault_leaves_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = synced_vault(&dir, "first.bin");
        let mut second = second_device(&dir, &first);
        let (team, general, servers) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let vaults = [(general, None), (servers, Some("Servers"))];
        join_team(&mut first, team, &vaults);
        join_team(&mut second, team, &vaults);
        let mut servers_of = FakeServers::default();

        let web = host("web");
        first.add_entry(web.clone()).unwrap();
        first.share_entry(web.id, general).unwrap();
        servers_of.sync(&mut first);
        servers_of.sync(&mut second);
        assert_eq!(second.vault_of(web.id), Some(general));

        first.share_entry(web.id, servers).unwrap();
        let jobs = first.begin_sync();
        let changes = |vault_id| {
            &jobs
                .iter()
                .find(|job| job.vault_id == vault_id)
                .unwrap()
                .changes
        };
        assert!(changes(general)[0].is_deleted);
        assert!(!changes(servers)[0].is_deleted);
        servers_of.sync(&mut first);
        assert!(!first.has_unsynced_changes());

        // The vault the host arrives in is answered first, the one it left last.
        let mut answers = servers_of.answers(&second);
        answers.sort_by_key(|answer| answer.vault_id == general);
        second.finish_round(answers).unwrap();
        assert_eq!(second.vault_of(web.id), Some(servers));
        assert_eq!(labels(&second), ["web"]);
        assert!(!second.has_unsynced_changes());
    }

    #[test]
    fn a_key_in_a_team_vault_stays_there_and_the_moved_host_takes_a_copy() {
        let dir = tempfile::tempdir().unwrap();
        let mut vault = synced_vault(&dir, "first.bin");
        let (team, general, servers) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        join_team(
            &mut vault,
            team,
            &[(general, None), (servers, Some("Servers"))],
        );
        let key = ssh_key("deploy");
        vault.payload.keys.push(key.clone());
        let web = ServerEntry {
            auth: AuthMethod::PrivateKey { key_id: key.id },
            ..host("web")
        };
        vault.add_entry(web.clone()).unwrap();

        // A personal key goes into the vault with the host.
        vault.share_entry(web.id, general).unwrap();
        assert_eq!(vault.vault_of(key.id), Some(general));
        assert_eq!(key_of(&vault, web.id), key.id);

        vault.share_entry(web.id, servers).unwrap();
        let copy = key_of(&vault, web.id);
        assert_ne!(copy, key.id);
        assert_eq!(vault.vault_of(copy), Some(servers));
        assert_eq!(vault.vault_of(key.id), Some(general));
        assert_eq!(
            vault
                .payload
                .keys
                .iter()
                .find(|k| k.id == copy)
                .unwrap()
                .pem,
            key.pem
        );

        // Back again: the key that stayed is used, and no third one is made.
        vault.share_entry(web.id, general).unwrap();
        assert_eq!(key_of(&vault, web.id), key.id);
        assert_eq!(vault.payload.keys.len(), 2);
    }

    #[test]
    fn losing_one_vault_of_a_team_keeps_its_records_and_the_rest_of_the_team() {
        let dir = tempfile::tempdir().unwrap();
        let mut vault = synced_vault(&dir, "first.bin");
        let (team, general, servers) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let vaults = [(general, None), (servers, Some("Servers"))];
        join_team(&mut vault, team, &vaults);
        let mut servers_of = FakeServers::default();

        let key = ssh_key("deploy");
        vault.payload.keys.push(key.clone());
        let web = host("web");
        let db = ServerEntry {
            auth: AuthMethod::PrivateKey { key_id: key.id },
            ..host("db")
        };
        vault.add_entry(web.clone()).unwrap();
        vault.add_entry(db.clone()).unwrap();
        vault.share_entry(web.id, general).unwrap();
        vault.share_entry(db.id, servers).unwrap();
        servers_of.sync(&mut vault);

        // The roster still lists both vaults; the key for one did not come.
        see_team(&mut vault, team, &vaults, &[general]);

        assert_eq!(labels(&vault), ["db", "web"]);
        assert_ne!(
            vault.get_entry(db.id).unwrap().id,
            db.id,
            "the kept copy has an id of its own"
        );
        let kept = vault
            .list_entries()
            .unwrap()
            .into_iter()
            .find(|e| e.label == "db")
            .unwrap();
        assert_eq!(vault.vault_of(kept.id), None);
        assert_ne!(key_of(&vault, kept.id), key.id);
        assert!(vault
            .payload
            .keys
            .iter()
            .any(|k| k.id == key_of(&vault, kept.id)));

        assert_eq!(vault.vault_of(web.id), Some(general));
        assert_eq!(vault.team_vaults(team).len(), 1);
        let jobs = vault.begin_sync();
        assert!(jobs.iter().any(|job| job.vault_id == general));
        assert!(jobs.iter().all(|job| job.vault_id != servers));
        // The copies go to the personal vault; the vault that stayed has nothing to send.
        servers_of.sync(&mut vault);
        assert!(!vault.has_unsynced_changes());
    }

    #[test]
    fn a_host_in_a_kept_vault_is_not_edited_when_the_vault_of_its_key_is_lost() {
        let dir = tempfile::tempdir().unwrap();
        let mut vault = synced_vault(&dir, "first.bin");
        let (team, general, servers) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let vaults = [(general, None), (servers, Some("Servers"))];
        join_team(&mut vault, team, &vaults);

        let key = ssh_key("deploy");
        vault.payload.keys.push(key.clone());
        let web = ServerEntry {
            auth: AuthMethod::PrivateKey { key_id: key.id },
            ..host("web")
        };
        vault.add_entry(web.clone()).unwrap();
        vault.share_entry(web.id, general).unwrap();
        // Someone pointed the host at a key that lives in the other vault.
        vault
            .payload
            .sync
            .as_mut()
            .unwrap()
            .shared
            .insert(key.id, servers);

        see_team(&mut vault, team, &vaults, &[general]);

        assert_eq!(key_of(&vault, web.id), key.id);
        assert!(vault.payload.keys.iter().all(|k| k.id != key.id));
    }

    /// Two devices of one account in a team with two vaults, with "web" in
    /// the first vault and "db" in the second, synced.
    struct TwoDevices {
        first: VaultManager,
        second: VaultManager,
        servers_of: FakeServers,
        team: Uuid,
        general: Uuid,
        servers: Uuid,
        web: ServerEntry,
        db: ServerEntry,
    }

    fn two_devices(dir: &tempfile::TempDir) -> TwoDevices {
        let mut first = synced_vault(dir, "first.bin");
        let mut second = second_device(dir, &first);
        let (team, general, servers) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let vaults = [(general, None), (servers, Some("Servers"))];
        join_team(&mut first, team, &vaults);
        join_team(&mut second, team, &vaults);
        let mut servers_of = FakeServers::default();

        let (web, db) = (host("web"), host("db"));
        first.add_entry(web.clone()).unwrap();
        first.add_entry(db.clone()).unwrap();
        first.share_entry(web.id, general).unwrap();
        first.share_entry(db.id, servers).unwrap();
        servers_of.sync(&mut first);
        servers_of.sync(&mut second);
        assert_eq!(second.vault_of(db.id), Some(servers));
        TwoDevices {
            first,
            second,
            servers_of,
            team,
            general,
            servers,
            web,
            db,
        }
    }

    #[test]
    fn two_hosts_swapped_between_two_vaults_arrive_on_the_other_device() {
        let dir = tempfile::tempdir().unwrap();
        let TwoDevices {
            mut first,
            mut second,
            mut servers_of,
            general,
            servers,
            web,
            db,
            ..
        } = two_devices(&dir);

        first.share_entry(web.id, servers).unwrap();
        first.share_entry(db.id, general).unwrap();
        servers_of.sync(&mut first);
        assert!(!first.has_unsynced_changes());

        // Each vault's answer has a host that still lives in the other one.
        servers_of.sync(&mut second);
        assert_eq!(second.vault_of(web.id), Some(servers));
        assert_eq!(second.vault_of(db.id), Some(general));
        assert_eq!(labels(&second), ["db", "web"]);
        assert!(!second.has_unsynced_changes());

        // Both vaults go on afterwards.
        let cache = host("cache");
        first.add_entry(cache.clone()).unwrap();
        first.share_entry(cache.id, general).unwrap();
        servers_of.sync(&mut first);
        servers_of.sync(&mut second);
        assert_eq!(second.vault_of(cache.id), Some(general));
    }

    #[test]
    fn a_host_moved_after_an_edit_elsewhere_still_leaves_its_old_vault() {
        let dir = tempfile::tempdir().unwrap();
        let TwoDevices {
            mut first,
            mut second,
            mut servers_of,
            general,
            servers,
            web,
            ..
        } = two_devices(&dir);

        second
            .update_entry(ServerEntry {
                notes: Some("edited".into()),
                updated_at: 5,
                ..web.clone()
            })
            .unwrap();
        servers_of.sync(&mut second);
        // The first device moves the host before it has seen the edit.
        first.share_entry(web.id, servers).unwrap();
        servers_of.sync(&mut first);
        servers_of.sync(&mut first);
        assert!(!first.has_unsynced_changes());
        assert_eq!(first.vault_of(web.id), Some(servers));

        servers_of.sync(&mut second);
        assert_eq!(second.vault_of(web.id), Some(servers));
        assert!(!second.has_unsynced_changes());
        let server = &servers_of.0[&general];
        assert!(
            server.items[&web.id].is_deleted,
            "the host is gone from the vault it left"
        );

        // The vault it left still takes changes on both devices.
        let cache = host("cache");
        second.add_entry(cache.clone()).unwrap();
        second.share_entry(cache.id, general).unwrap();
        servers_of.sync(&mut second);
        servers_of.sync(&mut first);
        assert_eq!(first.vault_of(cache.id), Some(general));
    }

    #[test]
    fn a_host_taken_out_of_a_vault_that_then_got_a_new_key_still_leaves_it() {
        let dir = tempfile::tempdir().unwrap();
        let TwoDevices {
            mut first,
            mut servers_of,
            general,
            web,
            ..
        } = two_devices(&dir);

        first.unshare_entry(web.id).unwrap();
        servers_of.0.get_mut(&general).unwrap().rotate();
        servers_of.sync(&mut first);
        servers_of.sync(&mut first);
        // The personal vault had the host's older deletion to write over.
        servers_of.sync(&mut first);

        assert!(!first.has_unsynced_changes());
        assert_eq!(first.vault_of(web.id), None);
        assert!(servers_of.0[&general].items[&web.id].is_deleted);
    }

    #[test]
    fn a_host_deleted_before_a_new_vault_key_stays_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let TwoDevices {
            mut first,
            mut servers_of,
            general,
            web,
            ..
        } = two_devices(&dir);

        first.delete_entry(web.id).unwrap();
        servers_of.0.get_mut(&general).unwrap().rotate();
        servers_of.sync(&mut first);
        assert_eq!(labels(&first), ["db"]);
        assert!(
            first.has_unsynced_changes(),
            "the deletion is still to be sent"
        );
        servers_of.sync(&mut first);

        assert!(!first.has_unsynced_changes());
        assert!(servers_of.0[&general].items[&web.id].is_deleted);
    }

    #[test]
    fn an_item_under_the_id_of_a_host_elsewhere_holds_up_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let TwoDevices {
            mut first,
            mut second,
            mut servers_of,
            general,
            servers,
            web,
            ..
        } = two_devices(&dir);

        // Another member writes into the second vault under the id of a host
        // that lives in the first.
        let server = servers_of.0.get_mut(&servers).unwrap();
        server.latest_rev += 1;
        let claimed = ServerEntry {
            label: "theirs".into(),
            ..web.clone()
        };
        let blob =
            Record::Entry(claimed).seal(&crypto::derive_subkey(&[7; 32], ITEM_KEY_PURPOSE), web.id);
        let item = PulledItem {
            id: web.id,
            rev: server.latest_rev,
            key_version: 1,
            is_deleted: false,
            blob,
        };
        server.items.insert(web.id, item);

        let cache = host("cache");
        first.add_entry(cache.clone()).unwrap();
        first.share_entry(cache.id, servers).unwrap();
        servers_of.sync(&mut first);
        servers_of.sync(&mut second);

        for vault in [&first, &second] {
            assert_eq!(vault.vault_of(web.id), Some(general));
            assert_eq!(vault.get_entry(web.id).unwrap().label, "web");
            assert!(!vault.has_unsynced_changes());
        }
        assert_eq!(second.vault_of(cache.id), Some(servers));
    }

    #[test]
    fn a_held_item_does_not_keep_back_the_items_that_came_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let TwoDevices {
            mut second,
            general,
            servers,
            web,
            ..
        } = two_devices(&dir);
        let cursor = |vault: &VaultManager| {
            vault
                .begin_sync()
                .into_iter()
                .find(|job| job.vault_id == servers)
                .unwrap()
                .cursor
        };
        let started_at = cursor(&second);
        let item_key = crypto::derive_subkey(&[7; 32], ITEM_KEY_PURPOSE);
        let item = |entry: &ServerEntry, rev: u64| PulledItem {
            id: entry.id,
            rev,
            key_version: 1,
            is_deleted: false,
            blob: Record::Entry(entry.clone()).seal(&item_key, entry.id),
        };

        // One answer brings an item under the id of a host that lives in the
        // other vault, and after it an ordinary new host.
        let claimed = ServerEntry {
            label: "theirs".into(),
            ..web.clone()
        };
        let cache = host("cache");
        let answer = SyncResult {
            vault_id: servers,
            started_at,
            pulled: vec![item(&claimed, started_at + 1), item(&cache, started_at + 2)],
            cursor: started_at + 2,
            pushed: Vec::new(),
        };
        second.finish_round(vec![answer]).unwrap();

        // The new host is in, the claimed one is untouched...
        assert_eq!(second.vault_of(cache.id), Some(servers));
        assert_eq!(second.get_entry(web.id).unwrap().label, "web");
        assert_eq!(second.vault_of(web.id), Some(general));
        // ...and the vault is asked again from just before the held item.
        assert_eq!(cursor(&second), started_at);
        assert!(!second.has_unsynced_changes());
    }

    #[test]
    fn two_devices_that_lose_a_vault_keep_one_copy_between_them() {
        let dir = tempfile::tempdir().unwrap();
        let TwoDevices {
            mut first,
            mut second,
            mut servers_of,
            team,
            general,
            servers,
            db,
            ..
        } = two_devices(&dir);
        let vaults = [(general, None), (servers, Some("Servers"))];

        see_team(&mut first, team, &vaults, &[general]);
        servers_of.sync(&mut first);
        // The second device hears of the first one's copy before it learns
        // that the vault is gone.
        servers_of.sync(&mut second);
        see_team(&mut second, team, &vaults, &[general]);
        servers_of.sync(&mut second);
        servers_of.sync(&mut first);

        for vault in [&first, &second] {
            assert_eq!(labels(vault), ["db", "web"]);
            assert_ne!(
                vault.get_entry(db.id).unwrap().id,
                db.id,
                "the kept copy has an id of its own"
            );
            assert!(!vault.has_unsynced_changes());
        }
        let copy = |vault: &VaultManager| {
            vault
                .list_entries()
                .unwrap()
                .into_iter()
                .find(|e| e.label == "db")
                .unwrap()
                .id
        };
        assert_eq!(copy(&first), copy(&second));
    }

    #[test]
    fn a_vault_lost_a_second_time_leaves_the_copy_already_kept() {
        let dir = tempfile::tempdir().unwrap();
        let TwoDevices {
            mut first,
            mut servers_of,
            team,
            general,
            servers,
            ..
        } = two_devices(&dir);
        let vaults = [(general, None), (servers, Some("Servers"))];

        see_team(&mut first, team, &vaults, &[general]);
        // Let back in: the team's copy comes again under its old id.
        join_team(&mut first, team, &vaults);
        servers_of.sync(&mut first);
        assert_eq!(labels(&first), ["db", "db", "web"]);
        see_team(&mut first, team, &vaults, &[general]);

        assert_eq!(labels(&first), ["db", "web"]);
    }

    #[test]
    fn a_shared_host_given_another_key_takes_the_key_into_its_vault() {
        let dir = tempfile::tempdir().unwrap();
        let mut vault = synced_vault(&dir, "first.bin");
        let (team, general, servers) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        join_team(
            &mut vault,
            team,
            &[(general, None), (servers, Some("Servers"))],
        );
        let (mine, theirs) = (ssh_key("mine"), ssh_key("theirs"));
        vault.payload.keys.extend([mine.clone(), theirs.clone()]);
        vault
            .payload
            .sync
            .as_mut()
            .unwrap()
            .shared
            .insert(theirs.id, servers);
        let web = host("web");
        vault.add_entry(web.clone()).unwrap();
        vault.share_entry(web.id, general).unwrap();

        // A personal key moves in with the host.
        let auth = AuthMethod::PrivateKey { key_id: mine.id };
        vault
            .update_entry(ServerEntry {
                auth,
                ..web.clone()
            })
            .unwrap();
        assert_eq!(vault.vault_of(mine.id), Some(general));

        // A key of another vault stays there; the host gets a copy.
        let auth = AuthMethod::PrivateKey { key_id: theirs.id };
        vault
            .update_entry(ServerEntry {
                auth,
                ..web.clone()
            })
            .unwrap();
        let copy = key_of(&vault, web.id);
        assert_ne!(copy, theirs.id);
        assert_eq!(vault.vault_of(copy), Some(general));
        assert_eq!(vault.vault_of(theirs.id), Some(servers));
    }

    fn labels(vault: &VaultManager) -> Vec<String> {
        let mut labels: Vec<String> = vault
            .list_entries()
            .unwrap()
            .into_iter()
            .map(|e| e.label)
            .collect();
        labels.sort();
        labels
    }

    #[test]
    fn a_new_host_reaches_the_other_device() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = synced_vault(&dir, "first.bin");
        let mut second = second_device(&dir, &first);
        let mut server = FakeServer::default();

        first.add_entry(host("web")).unwrap();
        assert!(first.has_unsynced_changes());
        server.sync(&mut first);
        assert!(!first.has_unsynced_changes());

        assert_eq!(server.sync(&mut second), 1);
        assert_eq!(labels(&second), ["web"]);
        assert!(!second.has_unsynced_changes());
    }

    #[test]
    fn a_synced_vault_has_nothing_to_send() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = synced_vault(&dir, "first.bin");
        let mut server = FakeServer::default();
        first.add_entry(host("web")).unwrap();
        server.sync(&mut first);

        assert!(first.begin_sync()[0].changes.is_empty());
        // Our own change comes back on the next pull and must not count.
        assert_eq!(server.sync(&mut first), 0);
    }

    #[test]
    fn an_edit_and_a_delete_reach_the_other_device() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = synced_vault(&dir, "first.bin");
        let mut second = second_device(&dir, &first);
        let mut server = FakeServer::default();

        let web = host("web");
        let db = host("db");
        first.add_entry(web.clone()).unwrap();
        first.add_entry(db.clone()).unwrap();
        server.sync(&mut first);
        server.sync(&mut second);

        second
            .update_entry(ServerEntry {
                label: "web-2".into(),
                updated_at: 5,
                ..web
            })
            .unwrap();
        second.delete_entry(db.id).unwrap();
        server.sync(&mut second);
        server.sync(&mut first);

        assert_eq!(labels(&first), ["web-2"]);
        assert!(!first.has_unsynced_changes());
    }

    #[test]
    fn opening_a_host_is_not_an_edit_and_survives_a_pull() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = synced_vault(&dir, "first.bin");
        let mut second = second_device(&dir, &first);
        let mut server = FakeServer::default();

        let web = host("web");
        first.add_entry(web.clone()).unwrap();
        server.sync(&mut first);
        server.sync(&mut second);

        second.touch_entry_opened(web.id).unwrap();
        assert!(!second.has_unsynced_changes());

        first
            .update_entry(ServerEntry {
                label: "web-2".into(),
                updated_at: 5,
                ..web.clone()
            })
            .unwrap();
        server.sync(&mut first);
        server.sync(&mut second);

        let entry = second.get_entry(web.id).unwrap();
        assert_eq!(entry.label, "web-2");
        assert!(entry.last_opened_at.is_some());
    }

    #[test]
    fn the_later_edit_of_one_host_wins_on_both_devices() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = synced_vault(&dir, "first.bin");
        let mut second = second_device(&dir, &first);
        let mut server = FakeServer::default();

        let web = host("web");
        first.add_entry(web.clone()).unwrap();
        server.sync(&mut first);
        server.sync(&mut second);

        first
            .update_entry(ServerEntry {
                label: "older".into(),
                updated_at: 5,
                ..web.clone()
            })
            .unwrap();
        second
            .update_entry(ServerEntry {
                label: "newer".into(),
                updated_at: 9,
                ..web.clone()
            })
            .unwrap();
        server.sync(&mut first);
        // The pull brings "older"; the local edit is newer and stays.
        server.sync(&mut second);
        assert_eq!(labels(&second), ["newer"]);
        // The next round sends it on top of the server's version.
        server.sync(&mut second);
        server.sync(&mut first);

        assert_eq!(labels(&first), ["newer"]);
        assert!(!first.has_unsynced_changes());
        assert!(!second.has_unsynced_changes());
    }

    #[test]
    fn an_older_local_edit_gives_way_to_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = synced_vault(&dir, "first.bin");
        let mut second = second_device(&dir, &first);
        let mut server = FakeServer::default();

        let web = host("web");
        first.add_entry(web.clone()).unwrap();
        server.sync(&mut first);
        server.sync(&mut second);

        first
            .update_entry(ServerEntry {
                label: "newer".into(),
                updated_at: 9,
                ..web.clone()
            })
            .unwrap();
        second
            .update_entry(ServerEntry {
                label: "older".into(),
                updated_at: 5,
                ..web.clone()
            })
            .unwrap();
        server.sync(&mut first);
        server.sync(&mut second);

        assert_eq!(labels(&second), ["newer"]);
        assert!(!second.has_unsynced_changes());
    }

    #[test]
    fn a_host_deleted_here_and_edited_there_comes_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = synced_vault(&dir, "first.bin");
        let mut second = second_device(&dir, &first);
        let mut server = FakeServer::default();

        let web = host("web");
        first.add_entry(web.clone()).unwrap();
        server.sync(&mut first);
        server.sync(&mut second);

        first
            .update_entry(ServerEntry {
                label: "edited".into(),
                updated_at: 9,
                ..web.clone()
            })
            .unwrap();
        second.delete_entry(web.id).unwrap();
        server.sync(&mut first);
        server.sync(&mut second);

        assert_eq!(labels(&second), ["edited"]);
        assert!(!second.has_unsynced_changes());
    }

    #[test]
    fn an_edit_made_during_a_round_is_sent_by_the_next_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = synced_vault(&dir, "first.bin");

        let web = host("web");
        first.add_entry(web.clone()).unwrap();
        let job = first.begin_sync().remove(0);
        first
            .update_entry(ServerEntry {
                label: "edited".into(),
                updated_at: 9,
                ..web
            })
            .unwrap();
        let pushed = vec![job.changes[0].accepted_at(1)];
        first
            .finish_round(vec![SyncResult {
                vault_id: job.vault_id,
                started_at: 0,
                pulled: Vec::new(),
                cursor: 0,
                pushed,
            }])
            .unwrap();

        let next = first.begin_sync().remove(0);
        assert_eq!(next.changes.len(), 1);
        assert_eq!(next.changes[0].base_rev, 1);
    }

    #[test]
    fn an_item_moved_to_another_id_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = synced_vault(&dir, "first.bin");
        let mut second = second_device(&dir, &first);

        first.add_entry(host("web")).unwrap();
        let job = first.begin_sync().remove(0);
        let moved = PulledItem {
            id: Uuid::new_v4(),
            rev: 1,
            key_version: 1,
            is_deleted: false,
            blob: job.changes[0].blob.clone(),
        };
        let result = SyncResult {
            vault_id: job.vault_id,
            started_at: 0,
            pulled: vec![moved],
            cursor: 1,
            pushed: Vec::new(),
        };

        assert_eq!(second.finish_round(vec![result]).unwrap(), 0);
        assert!(labels(&second).is_empty());
    }

    #[test]
    fn a_deleted_flag_set_by_the_server_deletes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = synced_vault(&dir, "first.bin");
        let mut server = FakeServer::default();
        let web = host("web");
        first.add_entry(web.clone()).unwrap();
        server.sync(&mut first);

        // The live blob again, at a later revision, marked as deleted.
        let forged = PulledItem {
            id: web.id,
            rev: 2,
            key_version: 1,
            is_deleted: true,
            blob: server.items[&web.id].blob.clone(),
        };
        let result = SyncResult {
            vault_id: Uuid::nil(),
            started_at: 0,
            pulled: vec![forged],
            cursor: 2,
            pushed: Vec::new(),
        };

        assert_eq!(first.finish_round(vec![result]).unwrap(), 0);
        assert_eq!(labels(&first), ["web"]);
    }

    #[test]
    fn a_tag_dropped_there_and_taken_up_here_is_put_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = synced_vault(&dir, "first.bin");
        let mut second = second_device(&dir, &first);
        let mut server = FakeServer::default();

        let tag = first.find_or_create_tag("prod").unwrap();
        let web = ServerEntry {
            tags: vec![tag],
            ..host("web")
        };
        let db = host("db");
        first.add_entry(web.clone()).unwrap();
        first.add_entry(db.clone()).unwrap();
        server.sync(&mut first);
        server.sync(&mut second);

        // The first device stops using the tag, which removes it there...
        first
            .update_entry(ServerEntry {
                tags: Vec::new(),
                updated_at: 5,
                ..web
            })
            .unwrap();
        // ...while the second gives it to another host.
        second
            .update_entry(ServerEntry {
                tags: vec![tag],
                updated_at: 6,
                ..db
            })
            .unwrap();
        server.sync(&mut first);
        server.sync(&mut second);
        server.sync(&mut second);
        server.sync(&mut first);

        assert_eq!(second.list_tags().len(), 1);
        assert_eq!(first.list_tags().len(), 1);
        assert!(!first.has_unsynced_changes());
        assert!(!second.has_unsynced_changes());
    }

    #[test]
    fn a_record_too_large_to_send_does_not_count_as_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = synced_vault(&dir, "first.bin");
        let huge = ServerEntry {
            notes: Some("x".repeat(MAX_ITEM_BYTES)),
            ..host("huge")
        };
        first.add_entry(huge).unwrap();

        assert!(first.begin_sync()[0].changes.is_empty());
        assert!(!first.has_unsynced_changes());
    }

    #[test]
    fn sync_state_without_a_ledger_still_loads() {
        #[derive(Serialize)]
        struct OldState {
            user_id: Uuid,
            github_login: String,
            session_token: Option<String>,
            vault_id: Uuid,
            cursor: u64,
            items: BTreeMap<Uuid, SyncedItem>,
        }
        let old = OldState {
            user_id: Uuid::nil(),
            github_login: "alice".into(),
            session_token: Some("token".into()),
            vault_id: Uuid::nil(),
            cursor: 7,
            items: BTreeMap::from([(
                Uuid::new_v4(),
                SyncedItem {
                    rev: 7,
                    hash: [1; 32],
                },
            )]),
        };
        let mut bytes = Vec::new();
        ciborium::into_writer(&old, &mut bytes).unwrap();

        let state: SyncState = ciborium::from_reader(&bytes[..]).unwrap();
        assert_eq!(state.github_login, "alice");
        assert_eq!(state.personal.cursor, 0);
    }

    #[test]
    fn saving_a_shared_host_with_a_personal_key_is_announced() {
        let dir = tempfile::tempdir().unwrap();
        let mut vault = synced_vault(&dir, "first.bin");
        let (team, team_vault) = (Uuid::new_v4(), Uuid::new_v4());
        see_team(&mut vault, team, &[(team_vault, None)], &[team_vault]);
        let web = host("web");
        vault.add_entry(web.clone()).unwrap();
        let key_id = Uuid::new_v4();
        vault.payload.keys.push(SshKey {
            id: key_id,
            name: "deploy".into(),
            created_at: 0,
            fingerprint: String::new(),
            key_type: String::new(),
            pem: "pem".into(),
            passphrase: None,
        });
        let with_key = ServerEntry {
            auth: AuthMethod::PrivateKey { key_id },
            ..web.clone()
        };

        // Personal host: nothing is shared by saving it.
        assert!(vault.key_shared_by_saving(&with_key).is_none());

        vault.share_entry(web.id, team_vault).unwrap();
        let (key_name, label) = vault.key_shared_by_saving(&with_key).unwrap();
        assert_eq!((key_name.as_str(), label.as_str()), ("deploy", "ops"));
        // A host that logs in without a key shares none.
        assert!(vault.key_shared_by_saving(&web).is_none());

        // Once the key is in the team's vault there is nothing left to announce.
        vault.update_entry(with_key.clone()).unwrap();
        assert!(vault.key_shared_by_saving(&with_key).is_none());
    }

    #[test]
    fn a_renamed_host_is_still_reached_by_its_old_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut vault = synced_vault(&dir, "first.bin");
        let web = host("web");
        vault.add_entry(web.clone()).unwrap();
        let (second, third) = (Uuid::new_v4(), Uuid::new_v4());

        vault.payload.give_new_id(web.id, second, &BTreeSet::new());
        vault.note_renamed(web.id, second);
        assert_eq!(vault.current_id(web.id), second);
        assert_eq!(vault.get_entry(web.id).unwrap().id, second);
        vault.set_entry_os(web.id, "Debian".into()).unwrap();
        assert_eq!(
            vault.get_entry(second).unwrap().os.as_deref(),
            Some("Debian")
        );

        // Renamed once more: the first id follows it there.
        vault.payload.give_new_id(second, third, &BTreeSet::new());
        vault.note_renamed(second, third);
        assert_eq!(vault.get_entry(web.id).unwrap().id, third);
        let unknown = Uuid::new_v4();
        assert_eq!(vault.current_id(unknown), unknown);
    }

    #[test]
    fn an_untouched_proxy_rule_is_not_sent() {
        let dir = tempfile::tempdir().unwrap();
        let first = synced_vault(&dir, "first.bin");
        assert!(first.begin_sync()[0].changes.is_empty());
    }

    #[test]
    fn sync_state_survives_a_lock_and_unlock() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = synced_vault(&dir, "first.bin");
        let mut server = FakeServer::default();
        first.add_entry(host("web")).unwrap();
        server.sync(&mut first);
        let path = first.path.clone();
        first.lock();

        let reopened =
            VaultManager::unlock_with_password(&path, &SecretString::new("pw".into())).unwrap();
        assert!(!reopened.has_unsynced_changes());
        assert_eq!(reopened.sync_state().unwrap().github_login, "alice");
    }

    #[test]
    fn a_recovery_is_remembered_for_the_account() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        let (mut vault, phrase) =
            VaultManager::create_with_params(&path, &SecretString::new("pw".into()), fast_kdf())
                .unwrap();
        vault.payload.sync = Some(SyncState::new(
            Uuid::nil(),
            "alice".into(),
            "token".into(),
            Uuid::nil(),
        ));
        vault.save().unwrap();
        let words = phrase.expose_words().to_string();
        vault.lock();

        let phrase = RecoveryPhrase::from_words(&words).unwrap();
        let (mut vault, _) = VaultManager::unlock_with_recovery_with_params(
            &path,
            &phrase,
            &SecretString::new("new".into()),
            fast_kdf(),
        )
        .unwrap();

        let job = vault.recovery_job().unwrap();
        assert_eq!(
            job.old_recovery_auth_key,
            recovery_auth_key(&phrase.to_bytes())
        );
        vault.finish_recovery(Some("fresh".into())).unwrap();
        assert!(vault.recovery_job().is_none());
        assert_eq!(
            vault.sync_state().unwrap().session_token.as_deref(),
            Some("fresh")
        );
    }

    #[test]
    fn a_vault_made_from_an_account_opens_with_its_password_and_phrase() {
        let dir = tempfile::tempdir().unwrap();
        let password = SecretString::new("pw".into());
        let (account_vault, phrase) =
            VaultManager::create_with_params(&dir.path().join("a.bin"), &password, fast_kdf())
                .unwrap();
        let slot = account_vault.password_slot();
        let account = AdoptedAccount {
            dek: slot.open(&password).unwrap().dek,
            kdf_salt: slot.kdf_salt(),
            kdf_params: slot.kdf_params().clone(),
            enc_dek_by_password: slot.enc_dek_by_password().clone(),
            enc_dek_by_recovery: account_vault.enc_dek_by_recovery.clone(),
            state: SyncState::new(Uuid::nil(), "alice".into(), "token".into(), Uuid::nil()),
        };

        let path = dir.path().join("b.bin");
        let vault = VaultManager::create_from_account(&path, account).unwrap();
        assert!(vault.sync_state().is_some());
        vault.lock();

        VaultManager::unlock_with_password(&path, &password).unwrap();
        VaultManager::unlock_with_recovery_with_params(
            &path,
            &phrase,
            &SecretString::new("new".into()),
            fast_kdf(),
        )
        .unwrap();
    }

    #[test]
    fn a_registered_vault_opens_with_its_password_and_phrase() {
        let dir = tempfile::tempdir().unwrap();
        let password = SecretString::new("pw".into());
        let (keys, phrase) = NewVaultKeys::generate_with_params(&password, fast_kdf()).unwrap();
        let user_id = Uuid::new_v4();
        let state = SyncState::new(user_id, "alice".into(), "token".into(), Uuid::new_v4());

        let path = dir.path().join("vault.bin");
        let vault = VaultManager::create_registered(&path, &keys, &state).unwrap();
        assert_eq!(vault.sync_state().unwrap().user_id, user_id);
        vault.lock();

        VaultManager::unlock_with_password(&path, &password).unwrap();
        VaultManager::unlock_with_recovery_with_params(
            &path,
            &phrase,
            &SecretString::new("new".into()),
            fast_kdf(),
        )
        .unwrap();
    }
}
