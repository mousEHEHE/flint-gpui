//! The sync account: signing in, and carrying vault changes to the server
//! and back. Everything here blocks on the network or on Argon2id, so it
//! runs on worker threads; the results are installed into the vault on the
//! main thread.

pub mod api;
pub mod oauth;
pub mod team;

use std::sync::atomic::AtomicBool;

use secrecy::{ExposeSecret as _, SecretString};

use crate::vault::format::KdfParams;
use crate::vault::sync::{
    self, AdoptedAccount, RecoveryJob, Registration, SyncJob, SyncResult, SyncState,
};
use crate::vault::{
    crypto, AccountPasswordChange, NewRecoverySlot, NewVaultKeys, PasswordSlot, RecoveryPhrase,
    VaultError, VaultManager,
};

pub use api::{Api, ApiError, GitHubStep};
pub use oauth::OAuthError;

/// The server takes at most 500 changes and 8 MiB in one request, and base64
/// makes the blobs a third larger.
const MAX_CHANGES_PER_PUSH: usize = 500;
const MAX_BYTES_PER_PUSH: usize = 4 * 1024 * 1024;

/// Every flint vault is made with at least the default cost, so an account
/// that asks for less comes from a broken or hostile server and would make
/// the password easier to guess. One that asks for far more would freeze
/// the machine. Tests use the lowest cost the server accepts.
const KDF_MIN_MEMORY: u32 = if cfg!(test) { 19_456 } else { 262_144 };
const KDF_MIN_PASSES: u32 = if cfg!(test) { 1 } else { 3 };
const KDF_MAX_MEMORY: u32 = 1_048_576;
const KDF_MAX_PASSES: u32 = 10;
const KDF_MAX_LANES: u32 = 4;

#[derive(Debug, thiserror::Error)]
pub enum AccountError {
    #[error(transparent)]
    Vault(#[from] VaultError),
    #[error(transparent)]
    Api(#[from] ApiError),
    #[error(transparent)]
    OAuth(#[from] OAuthError),
    #[error("the account asks for key settings this build refuses to use")]
    UnsafeKdf,
}

/// Sign in with GitHub in the browser. The result says who the user is and
/// whether they already have an account; it opens nothing yet.
pub fn github_sign_in(
    api: &Api,
    open_browser: impl FnOnce(String),
    is_cancelled: &AtomicBool,
) -> Result<GitHubStep, AccountError> {
    let config = api.config()?;
    let callback = oauth::authorize(&config.github_client_id, open_browser, is_cancelled)?;
    Ok(api.sign_in_with_github(
        &callback.code,
        &callback.code_verifier,
        &callback.redirect_uri,
    )?)
}

/// Make an account out of this vault: its password becomes the account's
/// password. The recovery phrase is replaced, because the server needs a
/// proof made from the phrase and the old one is not known here.
pub fn register(
    api: &Api,
    step: &GitHubStep,
    slot: &PasswordSlot,
    password: &SecretString,
) -> Result<(Registration, RecoveryPhrase), AccountError> {
    let opened = slot.open(password)?;
    let (phrase, recovery_key) = RecoveryPhrase::generate();
    let enc_dek_by_recovery = crypto::encrypt(&recovery_key, opened.dek.as_ref());

    let session = api.register(&api::NewAccount {
        ticket: &step.ticket,
        kdf_salt: &slot.kdf_salt(),
        kdf_params: slot.kdf_params(),
        auth_key: &opened.auth_key,
        recovery_auth_key: &sync::recovery_auth_key(&recovery_key),
        enc_dek_by_password: slot.enc_dek_by_password(),
        enc_dek_by_recovery: &enc_dek_by_recovery,
    })?;

    let registration = Registration {
        replaces: slot.enc_dek_by_password().clone(),
        enc_dek_by_recovery,
        state: SyncState::new(
            session.user_id,
            step.github_login.clone(),
            session.session_token,
            session.vault_id,
        ),
    };
    Ok((registration, phrase))
}

/// Unlike `register`, this keeps the vault's own recovery phrase.
pub fn register_new_vault(
    api: &Api,
    step: &GitHubStep,
    keys: &NewVaultKeys,
) -> Result<SyncState, AccountError> {
    let session = api.register(&api::NewAccount {
        ticket: &step.ticket,
        kdf_salt: &keys.kdf_salt,
        kdf_params: &keys.kdf_params,
        auth_key: &keys.auth_key,
        recovery_auth_key: &keys.recovery_auth_key,
        enc_dek_by_password: &keys.enc_dek_by_password,
        enc_dek_by_recovery: &keys.enc_dek_by_recovery,
    })?;
    Ok(SyncState::new(
        session.user_id,
        step.github_login.clone(),
        session.session_token,
        session.vault_id,
    ))
}

/// Sign in to an existing account with its password. A wrong password comes
/// back as the server's `wrong_password` refusal.
pub fn sign_in(
    api: &Api,
    step: &GitHubStep,
    password: &SecretString,
) -> Result<AdoptedAccount, AccountError> {
    let (kdf_salt, kdf_params) = step.kdf.clone().ok_or(ApiError::BadAnswer)?;
    check_kdf(&kdf_params)?;
    let kek = crypto::derive_kek(password.expose_secret().as_bytes(), &kdf_salt, &kdf_params)?;

    let answer = api.login(&step.ticket, &sync::auth_key(&kek))?;
    let dek = VaultManager::unwrap_dek(&kek, &answer.enc_dek_by_password)?;

    Ok(AdoptedAccount {
        dek,
        kdf_salt,
        kdf_params,
        enc_dek_by_password: answer.enc_dek_by_password,
        enc_dek_by_recovery: answer.enc_dek_by_recovery,
        state: SyncState::new(
            answer.session.user_id,
            step.github_login.clone(),
            answer.session.session_token,
            answer.session.vault_id,
        ),
    })
}

fn check_kdf(params: &KdfParams) -> Result<(), AccountError> {
    let is_safe = (KDF_MIN_MEMORY..=KDF_MAX_MEMORY).contains(&params.m_cost)
        && (KDF_MIN_PASSES..=KDF_MAX_PASSES).contains(&params.t_cost)
        && (1..=KDF_MAX_LANES).contains(&params.p_cost);
    if is_safe {
        Ok(())
    } else {
        Err(AccountError::UnsafeKdf)
    }
}

/// One sync round: fetch what changed on the server, then send what changed
/// here.
///
/// Everything is sent, even an item the pull just brought a newer version
/// of: the server refuses that one by its `base_rev` and answers with its
/// own version, which the vault then settles like a pulled one.
pub fn run_sync(api: &Api, job: SyncJob) -> Result<SyncResult, ApiError> {
    let token = &job.session_token;
    let mut cursor = job.cursor;
    let mut pulled = Vec::new();
    loop {
        let page = api.pull(token, job.vault_id, cursor)?;
        // An empty page does not move the cursor, so asking again would
        // never end.
        let is_last = !page.has_more || page.items.is_empty();
        cursor = page.items.last().map_or(cursor, |item| item.rev);
        pulled.extend(page.items);
        if is_last {
            break;
        }
    }

    let mut pushed = Vec::new();
    for batch in batches(&job.changes) {
        let answer = api.push(token, job.vault_id, job.key_version, batch)?;
        pulled.extend(answer.conflicts);
        for (id, rev) in answer.applied {
            if let Some(change) = batch.iter().find(|change| change.id == id) {
                pushed.push(change.accepted_at(rev));
            }
        }
    }

    Ok(SyncResult {
        vault_id: job.vault_id,
        started_at: job.cursor,
        pulled,
        cursor,
        pushed,
    })
}

/// Split changes into requests the server accepts.
fn batches(changes: &[sync::OutgoingChange]) -> Vec<&[sync::OutgoingChange]> {
    let mut batches = Vec::new();
    let mut start = 0;
    let mut bytes = 0;
    for (index, change) in changes.iter().enumerate() {
        let is_full = index - start == MAX_CHANGES_PER_PUSH
            || (index > start && bytes + change.blob.len() > MAX_BYTES_PER_PUSH);
        if is_full {
            batches.push(&changes[start..index]);
            start = index;
            bytes = 0;
        }
        bytes += change.blob.len();
    }
    if start < changes.len() {
        batches.push(&changes[start..]);
    }
    batches
}

/// Give the account the new password. Every other device is signed out.
pub fn change_password(
    api: &Api,
    session_token: &str,
    change: &AccountPasswordChange,
) -> Result<(), ApiError> {
    api.change_password(
        session_token,
        &change.current_auth_key,
        &api::NewPassword {
            kdf_salt: &change.kdf_salt,
            kdf_params: &change.kdf_params,
            auth_key: &change.new_auth_key,
            enc_dek_by_password: &change.enc_dek_by_password,
        },
    )
}

/// Make a new recovery phrase for the vault, and for its account when
/// `session_token` is given. The account is told first: if it refuses,
/// nothing has changed anywhere.
pub fn new_recovery_phrase(
    api: &Api,
    session_token: Option<&str>,
    slot: &PasswordSlot,
    password: &SecretString,
) -> Result<(NewRecoverySlot, RecoveryPhrase), AccountError> {
    let opened = slot.open(password)?;
    let (phrase, recovery_key) = RecoveryPhrase::generate();
    let enc_dek_by_recovery = crypto::encrypt(&recovery_key, opened.dek.as_ref());
    if let Some(token) = session_token {
        api.replace_recovery_phrase(
            token,
            &opened.auth_key,
            &sync::recovery_auth_key(&recovery_key),
            &enc_dek_by_recovery,
        )?;
    }
    let slot = NewRecoverySlot {
        replaces: slot.enc_dek_by_password().clone(),
        enc_dek_by_recovery,
    };
    Ok((slot, phrase))
}

/// Delete the account on the server. The vault on this device is not touched.
pub fn delete_account(
    api: &Api,
    session_token: &str,
    slot: &PasswordSlot,
    password: &SecretString,
) -> Result<(), AccountError> {
    let opened = slot.open(password)?;
    Ok(api.delete_account(session_token, &opened.auth_key)?)
}

/// Tell the account about a recovery done on this device. Returns the new
/// session, or `None` when the account does not know the old phrase any
/// more: it was recovered from another device since.
pub fn tell_recovery(api: &Api, job: &RecoveryJob) -> Result<Option<String>, ApiError> {
    let ticket = match api.start_recovery(&job.old_recovery_auth_key) {
        Ok(ticket) => ticket,
        Err(e) if e.code() == Some("wrong_recovery_phrase") => return Ok(None),
        Err(e) => return Err(e),
    };
    let session = api.finish_recovery(
        &ticket,
        &api::NewPassword {
            kdf_salt: &job.kdf_salt,
            kdf_params: &job.kdf_params,
            auth_key: &job.new_auth_key,
            enc_dek_by_password: &job.enc_dek_by_password,
        },
        &job.new_recovery_auth_key,
        &job.enc_dek_by_recovery,
    )?;
    Ok(Some(session.session_token))
}

#[cfg(test)]
mod tests {
    use rand::Rng as _;

    use super::*;
    use crate::vault::format::AuthMethod;
    use crate::vault::ServerEntry;

    /// The lowest cost the server accepts, so the tests stay quick.
    fn fast_kdf() -> KdfParams {
        KdfParams {
            m_cost: 19_456,
            t_cost: 1,
            p_cost: 1,
        }
    }

    fn secret(text: &str) -> SecretString {
        SecretString::new(text.into())
    }

    fn new_vault(dir: &tempfile::TempDir, name: &str, password: &str) -> VaultManager {
        VaultManager::create_with_params(&dir.path().join(name), &secret(password), fast_kdf())
            .unwrap()
            .0
    }

    fn host(label: &str) -> ServerEntry {
        ServerEntry {
            id: uuid::Uuid::new_v4(),
            label: label.into(),
            host: format!("{label}.example.com"),
            port: 22,
            username: "root".into(),
            auth: AuthMethod::Password("hunter2".into()),
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

    /// The test server takes the code "id:login" in place of a real GitHub one.
    fn github(api: &Api, github_id: u32) -> GitHubStep {
        let code = format!("{github_id}:tester");
        api.sign_in_with_github(&code, &"v".repeat(43), "http://127.0.0.1:1/callback")
            .unwrap()
    }

    fn sync_once(api: &Api, vault: &mut VaultManager) -> usize {
        let result = run_sync(api, vault.begin_sync().remove(0)).unwrap();
        vault.finish_round(vec![result]).unwrap()
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

    /// Needs a sync server in test mode, reachable through `FLINT_API_URL`:
    /// `FLINT_API_URL=http://127.0.0.1:8765/api/v1 cargo test account -- --ignored`
    #[test]
    #[ignore = "needs a running test server"]
    fn two_devices_share_one_account() {
        let api = Api::new();
        let dir = tempfile::tempdir().unwrap();
        let github_id: u32 = rand::thread_rng().gen_range(1..u32::MAX / 2);

        // The first device makes the account out of its vault.
        let mut first = new_vault(&dir, "first.bin", "first password");
        let step = github(&api, github_id);
        assert!(step.is_new);
        let (registration, phrase) = register(
            &api,
            &step,
            &first.password_slot(),
            &secret("first password"),
        )
        .unwrap();
        first.install_registration(registration).unwrap();
        first.add_entry(host("web")).unwrap();
        sync_once(&api, &mut first);
        assert!(!first.has_unsynced_changes());

        // The second device has its own vault, password and a host.
        let mut second = new_vault(&dir, "second.bin", "second password");
        second.add_entry(host("laptop-only")).unwrap();
        let step = github(&api, github_id);
        assert!(!step.is_new);

        let wrong = sign_in(&api, &step, &secret("second password"))
            .err()
            .unwrap();
        assert!(matches!(&wrong, AccountError::Api(e) if e.code() == Some("wrong_password")));

        second
            .adopt_account(sign_in(&api, &step, &secret("first password")).unwrap())
            .unwrap();
        sync_once(&api, &mut second);
        assert_eq!(labels(&second), ["laptop-only", "web"]);
        sync_once(&api, &mut first);
        assert_eq!(labels(&first), ["laptop-only", "web"]);

        // The second vault now opens with the account's password and phrase.
        let second_path = dir.path().join("second.bin");
        second.lock();
        let mut second =
            VaultManager::unlock_with_password(&second_path, &secret("first password")).unwrap();

        // A password change on the first device signs the second one out.
        let slot = first.password_slot();
        let new_slot = slot
            .rewrap(&secret("first password"), &secret("changed"))
            .unwrap();
        let token = first.sync_state().unwrap().session_token.clone().unwrap();
        change_password(&api, &token, &new_slot.account_change(slot.kdf_params())).unwrap();
        first.install_password_slot(new_slot).unwrap();

        let refused = run_sync(&api, second.begin_sync().remove(0)).err().unwrap();
        assert!(refused.is_session_ended());
        let ended = second.sync_state().unwrap().session_token.clone().unwrap();
        second.end_session(&ended).unwrap();
        assert!(second.begin_sync().is_empty());

        // Signing in again with the new password continues the same sync.
        let step = github(&api, github_id);
        second
            .adopt_account(sign_in(&api, &step, &secret("changed")).unwrap())
            .unwrap();
        assert!(!second.has_unsynced_changes());
        second
            .delete_entry(second.list_entries().unwrap()[0].id)
            .unwrap();
        sync_once(&api, &mut second);
        sync_once(&api, &mut first);
        assert_eq!(labels(&first).len(), 1);

        // A recovery on the first device reaches the account on the next sync.
        let first_path = dir.path().join("first.bin");
        first.lock();
        let (mut first, _) = VaultManager::unlock_with_recovery_with_params(
            &first_path,
            &phrase,
            &secret("after recovery"),
            fast_kdf(),
        )
        .unwrap();
        let new_token = tell_recovery(&api, &first.recovery_job().unwrap()).unwrap();
        assert!(new_token.is_some());
        first.finish_recovery(new_token).unwrap();
        sync_once(&api, &mut first);

        let step = github(&api, github_id);
        assert!(sign_in(&api, &step, &secret("after recovery")).is_ok());
        // The old phrase is gone from the account too.
        let stale = first_recovery_job(&phrase, &first);
        assert!(tell_recovery(&api, &stale).unwrap().is_none());
    }

    /// A signed-in vault of a fresh account, with its team keys made.
    fn member(api: &Api, dir: &tempfile::TempDir, name: &str) -> (VaultManager, team::TeamKeys) {
        let github_id: u32 = rand::thread_rng().gen_range(1..u32::MAX / 2);
        let mut vault = new_vault(dir, name, "pw");
        let slot = vault.password_slot();
        let (registration, _) =
            register(api, &github(api, github_id), &slot, &secret("pw")).unwrap();
        vault.install_registration(registration).unwrap();

        let context = vault.team_context().unwrap();
        assert!(matches!(
            team::ensure_keys(api, &context, None),
            Err(team::TeamError::PasswordNeeded)
        ));
        let auth_key = slot.open(&secret("pw")).unwrap().auth_key;
        let keys = team::ensure_keys(api, &context, Some(&auth_key)).unwrap();
        // A device without the seed fetches the same keys from the server.
        assert_eq!(
            team::ensure_keys(api, &context, None).unwrap().seed(),
            keys.seed()
        );
        (vault, keys)
    }

    fn refresh_teams(api: &Api, vault: &mut VaultManager, keys: &team::TeamKeys) {
        let update = team::refresh(api, &vault.team_context().unwrap(), keys).unwrap();
        vault.apply_teams(update).unwrap();
    }

    fn sync_all(api: &Api, vault: &mut VaultManager) {
        let results = vault
            .begin_sync()
            .into_iter()
            .map(|job| run_sync(api, job).unwrap())
            .collect();
        vault.finish_round(results).unwrap();
    }

    /// Two rounds: the first may only learn of a deletion the second writes over.
    fn settle(api: &Api, vault: &mut VaultManager) {
        sync_all(api, vault);
        sync_all(api, vault);
        assert!(!vault.has_unsynced_changes());
    }

    fn vault_names(vault: &VaultManager, team_id: uuid::Uuid) -> Vec<String> {
        vault
            .team_vaults(team_id)
            .into_iter()
            .map(|(_, name)| name)
            .collect()
    }

    fn id_of(vault: &VaultManager, label: &str) -> uuid::Uuid {
        vault
            .list_entries()
            .unwrap()
            .into_iter()
            .find(|e| e.label == label)
            .unwrap()
            .id
    }

    fn key_version(vault: &VaultManager, vault_id: uuid::Uuid) -> u64 {
        let keys = vault.team_context().unwrap().vault_keys;
        keys.iter()
            .find(|(_, key)| key.vault_id == vault_id)
            .unwrap()
            .1
            .key_version
    }

    /// The invite, the request and the owner's yes, in one go.
    fn let_in(
        api: &Api,
        team_id: uuid::Uuid,
        (owner, owner_keys): (&mut VaultManager, &team::TeamKeys),
        (guest, guest_keys): (&mut VaultManager, &team::TeamKeys),
    ) {
        let (invite_id, invite) =
            team::invite(api, &owner.team_context().unwrap(), owner_keys, team_id).unwrap();
        owner.add_invite(invite_id, invite.clone()).unwrap();
        let (_, join) = team::join(
            api,
            &guest.team_context().unwrap(),
            guest_keys,
            &invite.link,
        )
        .unwrap();
        guest.add_join(join).unwrap();
        let context = owner.team_context().unwrap();
        let (requests, _) = team::requests(api, &context, team_id).unwrap();
        let (request_invite, request) = &requests[0];
        team::accept(api, &context, owner_keys, team_id, *request_invite, request).unwrap();
        team::revoke(api, &context, team_id, invite_id).unwrap();
        refresh_teams(api, owner, owner_keys);
    }

    /// A team "ops" of an owner and a guest with two vaults: "web" is in the
    /// first, which the guest sees, and "db" in "Servers", which it does not.
    struct TwoVaults {
        owner: VaultManager,
        owner_keys: team::TeamKeys,
        guest: VaultManager,
        guest_keys: team::TeamKeys,
        team_id: uuid::Uuid,
        general: uuid::Uuid,
        servers: uuid::Uuid,
        web: ServerEntry,
        db: ServerEntry,
    }

    fn two_vaults(api: &Api, dir: &tempfile::TempDir) -> TwoVaults {
        let (mut owner, owner_keys) = member(api, dir, "owner.bin");
        let (mut guest, guest_keys) = member(api, dir, "guest.bin");

        let created =
            team::create(api, &owner.team_context().unwrap(), &owner_keys, "ops").unwrap();
        let team_id = created.team_id;
        owner.add_team(created).unwrap();
        let general = owner.team_vaults(team_id)[0].0;
        let (web, db) = (host("web"), host("db"));
        owner.add_entry(web.clone()).unwrap();
        owner.add_entry(db.clone()).unwrap();
        sync_all(api, &mut owner);
        owner.share_entry(web.id, general).unwrap();
        let_in(
            api,
            team_id,
            (&mut owner, &owner_keys),
            (&mut guest, &guest_keys),
        );

        // The owner holds a new vault at once; nobody else is given it.
        team::add_vault(
            api,
            &owner.team_context().unwrap(),
            &owner_keys,
            team_id,
            " Servers ",
        )
        .unwrap();
        refresh_teams(api, &mut owner, &owner_keys);
        assert_eq!(vault_names(&owner, team_id), ["General", "Servers"]);
        let servers = owner.team_vaults(team_id)[1].0;
        owner.share_entry(db.id, servers).unwrap();
        sync_all(api, &mut owner);
        assert_eq!(owner.sharing_label(db.id).as_deref(), Some("ops / Servers"));

        refresh_teams(api, &mut guest, &guest_keys);
        sync_all(api, &mut guest);
        assert_eq!(vault_names(&guest, team_id), ["General"]);
        assert_eq!(labels(&guest), ["web"]);
        let guest_token = guest.sync_state().unwrap().session_token.clone().unwrap();
        assert!(api.pull(&guest_token, servers, 0).is_err());

        TwoVaults {
            owner,
            owner_keys,
            guest,
            guest_keys,
            team_id,
            general,
            servers,
            web,
            db,
        }
    }

    /// Needs a test sync server, like `two_devices_share_one_account`.
    #[test]
    #[ignore = "needs a running test server"]
    fn a_team_keeps_several_vaults_and_chooses_who_sees_each() {
        let api = Api::new();
        let dir = tempfile::tempdir().unwrap();
        let TwoVaults {
            mut owner,
            owner_keys,
            mut guest,
            guest_keys,
            team_id,
            general,
            servers,
            web,
            db,
        } = two_vaults(&api, &dir);
        let guest_id = guest.sync_state().unwrap().user_id;
        let guest_token = guest.sync_state().unwrap().session_token.clone().unwrap();

        // Ticked: the guest's device gets the key and the vault's hosts.
        let context = owner.team_context().unwrap();
        team::set_access(&api, &context, &owner_keys, team_id, servers, &[guest_id]).unwrap();
        refresh_teams(&api, &mut owner, &owner_keys);
        refresh_teams(&api, &mut guest, &guest_keys);
        sync_all(&api, &mut guest);
        assert_eq!(labels(&guest), ["db", "web"]);
        assert_eq!(guest.vault_of(db.id), Some(servers));
        // Nobody lost anything, so the key did not change and the owner is not behind.
        sync_all(&api, &mut owner);
        assert!(!owner.is_behind_on_teams());
        assert_eq!(key_version(&owner, servers), 1);

        // The guest can write there, and a host moved between the vaults follows.
        guest
            .update_entry(ServerEntry {
                label: "db-2".into(),
                updated_at: 9,
                ..db.clone()
            })
            .unwrap();
        sync_all(&api, &mut guest);
        sync_all(&api, &mut owner);
        assert_eq!(labels(&owner), ["db-2", "web"]);
        owner.share_entry(web.id, servers).unwrap();
        sync_all(&api, &mut owner);
        assert!(!owner.has_unsynced_changes());
        sync_all(&api, &mut guest);
        assert_eq!(guest.vault_of(web.id), Some(servers));
        owner.share_entry(web.id, general).unwrap();
        settle(&api, &mut owner);
        sync_all(&api, &mut guest);
        assert_eq!(guest.vault_of(web.id), Some(general));

        // A new name reaches everyone through the roster.
        let context = owner.team_context().unwrap();
        team::rename_vault(&api, &context, &owner_keys, team_id, servers, "Prod").unwrap();
        assert!(team::rename_vault(&api, &context, &owner_keys, team_id, servers, " ").is_err());
        refresh_teams(&api, &mut owner, &owner_keys);
        refresh_teams(&api, &mut guest, &guest_keys);
        assert_eq!(guest.sharing_label(db.id).as_deref(), Some("ops / Prod"));

        // Unticked: the vault gets a new key and every item with it.
        let context = owner.team_context().unwrap();
        team::set_access(&api, &context, &owner_keys, team_id, servers, &[]).unwrap();
        sync_all(&api, &mut owner);
        assert!(owner.is_behind_on_teams());
        refresh_teams(&api, &mut owner, &owner_keys);
        assert_eq!(
            (key_version(&owner, general), key_version(&owner, servers)),
            (1, 2)
        );
        sync_all(&api, &mut owner);
        assert_eq!(labels(&owner), ["db-2", "web"]);
        owner
            .update_entry(ServerEntry {
                label: "db-3".into(),
                updated_at: 12,
                ..db.clone()
            })
            .unwrap();
        owner.add_entry(host("cache")).unwrap();
        let cache = id_of(&owner, "cache");
        owner.share_entry(cache, general).unwrap();
        settle(&api, &mut owner);

        // The guest keeps what it had as a personal copy, loses the vault,
        // and still follows the one it was left with.
        refresh_teams(&api, &mut guest, &guest_keys);
        assert_eq!(vault_names(&guest, team_id), ["General"]);
        assert_ne!(
            guest.get_entry(db.id).unwrap().id,
            db.id,
            "the kept copy has an id of its own"
        );
        assert_eq!(guest.vault_of(id_of(&guest, "db-2")), None);
        assert!(api.pull(&guest_token, servers, 0).is_err());
        settle(&api, &mut guest);
        assert_eq!(labels(&guest), ["cache", "db-2", "web"]);
        assert_eq!(guest.vault_of(cache), Some(general));
    }

    /// Needs a test sync server, like `two_devices_share_one_account`.
    #[test]
    #[ignore = "needs a running test server"]
    fn managers_hold_every_vault_and_a_deleted_vault_leaves_copies() {
        let api = Api::new();
        let dir = tempfile::tempdir().unwrap();
        let TwoVaults {
            mut owner,
            owner_keys,
            mut guest,
            guest_keys,
            team_id,
            general,
            servers,
            web,
            db,
        } = two_vaults(&api, &dir);
        let owner_id = owner.sync_state().unwrap().user_id;
        let guest_id = guest.sync_state().unwrap().user_id;
        let guest_token = guest.sync_state().unwrap().session_token.clone().unwrap();

        // Someone who saw only the first vault is removed: only that vault
        // gets a new key, and the guest still reads it.
        let (mut passer, passer_keys) = member(&api, &dir, "passer.bin");
        let passer_id = passer.sync_state().unwrap().user_id;
        let_in(
            &api,
            team_id,
            (&mut owner, &owner_keys),
            (&mut passer, &passer_keys),
        );
        team::remove(
            &api,
            &owner.team_context().unwrap(),
            &owner_keys,
            team_id,
            &[passer_id],
        )
        .unwrap();
        refresh_teams(&api, &mut owner, &owner_keys);
        assert_eq!(
            (key_version(&owner, general), key_version(&owner, servers)),
            (2, 1)
        );
        refresh_teams(&api, &mut guest, &guest_keys);
        sync_all(&api, &mut guest);
        assert_eq!(labels(&guest), ["web"]);

        // An admin holds every vault: promoting the guest hands over the second.
        let context = owner.team_context().unwrap();
        team::set_role(&api, &context, &owner_keys, team_id, guest_id, "admin").unwrap();
        refresh_teams(&api, &mut owner, &owner_keys);
        refresh_teams(&api, &mut guest, &guest_keys);
        sync_all(&api, &mut guest);
        assert_eq!(vault_names(&guest, team_id), ["General", "Servers"]);
        assert_eq!(guest.vault_of(db.id), Some(servers));
        // An admin cannot be unticked.
        let context = owner.team_context().unwrap();
        team::set_access(&api, &context, &owner_keys, team_id, servers, &[]).unwrap();
        refresh_teams(&api, &mut owner, &owner_keys);
        refresh_teams(&api, &mut guest, &guest_keys);
        assert_eq!(vault_names(&guest, team_id), ["General", "Servers"]);

        // A plain member again keeps both vaults until unticked. Handing the
        // team to someone who sees one vault gives them the other as well.
        let context = owner.team_context().unwrap();
        team::set_role(&api, &context, &owner_keys, team_id, guest_id, "member").unwrap();
        refresh_teams(&api, &mut owner, &owner_keys);
        let context = owner.team_context().unwrap();
        team::set_access(&api, &context, &owner_keys, team_id, servers, &[]).unwrap();
        refresh_teams(&api, &mut owner, &owner_keys);
        refresh_teams(&api, &mut guest, &guest_keys);
        assert_eq!(vault_names(&guest, team_id), ["General"]);
        assert_ne!(
            guest.get_entry(db.id).unwrap().id,
            db.id,
            "the kept copy has an id of its own"
        );
        team::hand_over(
            &api,
            &owner.team_context().unwrap(),
            &owner_keys,
            team_id,
            guest_id,
        )
        .unwrap();
        refresh_teams(&api, &mut owner, &owner_keys);
        refresh_teams(&api, &mut guest, &guest_keys);
        assert_eq!(vault_names(&guest, team_id), ["General", "Servers"]);
        team::hand_over(
            &api,
            &guest.team_context().unwrap(),
            &guest_keys,
            team_id,
            owner_id,
        )
        .unwrap();
        refresh_teams(&api, &mut guest, &guest_keys);
        refresh_teams(&api, &mut owner, &owner_keys);
        sync_all(&api, &mut owner);
        sync_all(&api, &mut guest);
        assert_eq!(guest.vault_of(db.id), Some(servers));

        // The admin deletes the second vault. Its hosts stay with both as
        // personal copies, and the first vault goes on.
        let context = guest.team_context().unwrap();
        team::delete_vault(&api, &context, &guest_keys, team_id, servers).unwrap();
        refresh_teams(&api, &mut guest, &guest_keys);
        refresh_teams(&api, &mut owner, &owner_keys);
        assert_eq!(vault_names(&owner, team_id), ["General"]);
        assert_ne!(owner.get_entry(db.id).unwrap().id, db.id);
        assert_eq!(owner.vault_of(id_of(&owner, "db")), None);
        assert_eq!(owner.sharing_label(web.id).as_deref(), Some("ops"));
        assert!(api.pull(&guest_token, servers, 0).is_err());
        settle(&api, &mut owner);
        settle(&api, &mut guest);
        assert_eq!(labels(&owner), ["db", "web"]);
        // The guest lost the vault twice, and has the one copy from the first time.
        assert_eq!(labels(&guest), ["db", "web"]);

        // The last vault stays.
        let context = owner.team_context().unwrap();
        assert!(matches!(
            team::delete_vault(&api, &context, &owner_keys, team_id, general),
            Err(team::TeamError::Refused(_))
        ));
        owner
            .update_entry(ServerEntry {
                label: "web-2".into(),
                updated_at: 20,
                ..web.clone()
            })
            .unwrap();
        settle(&api, &mut owner);
        sync_all(&api, &mut guest);
        assert_eq!(guest.get_entry(web.id).unwrap().label, "web-2");
    }

    /// Needs a test sync server, like `two_devices_share_one_account`.
    #[test]
    #[ignore = "needs a running test server"]
    fn a_deletion_and_a_move_not_sent_yet_survive_a_new_vault_key() {
        let api = Api::new();
        let dir = tempfile::tempdir().unwrap();
        let TwoVaults {
            mut owner,
            owner_keys,
            guest,
            team_id,
            general,
            servers,
            db,
            ..
        } = two_vaults(&api, &dir);
        let guest_id = guest.sync_state().unwrap().user_id;
        let context = owner.team_context().unwrap();
        team::set_access(&api, &context, &owner_keys, team_id, servers, &[guest_id]).unwrap();
        refresh_teams(&api, &mut owner, &owner_keys);
        owner.add_entry(host("old")).unwrap();
        let old = id_of(&owner, "old");
        owner.share_entry(old, servers).unwrap();
        settle(&api, &mut owner);

        // Neither change is sent before the vault gets its new key, which
        // gives every item a new revision.
        owner.delete_entry(old).unwrap();
        owner.share_entry(db.id, general).unwrap();
        let context = owner.team_context().unwrap();
        team::set_access(&api, &context, &owner_keys, team_id, servers, &[]).unwrap();
        refresh_teams(&api, &mut owner, &owner_keys);
        settle(&api, &mut owner);

        assert_eq!(labels(&owner), ["db", "web"]);
        assert_eq!(owner.vault_of(db.id), Some(general));
        let token = owner.sync_state().unwrap().session_token.clone().unwrap();
        let left = api.pull(&token, servers, 0).unwrap().items;
        assert!(!left.is_empty() && left.iter().all(|item| item.is_deleted));
    }

    /// Needs a test sync server, like `two_devices_share_one_account`.
    #[test]
    #[ignore = "needs a running test server"]
    fn a_team_shares_a_host_and_takes_it_back_from_a_removed_member() {
        let api = Api::new();
        let dir = tempfile::tempdir().unwrap();
        let (mut owner, owner_keys) = member(&api, &dir, "owner.bin");
        let (mut guest, guest_keys) = member(&api, &dir, "guest.bin");
        let guest_id = guest.sync_state().unwrap().user_id;

        // The owner makes a team and shares a host that logs in with a key.
        let created =
            team::create(&api, &owner.team_context().unwrap(), &owner_keys, "ops").unwrap();
        let team_id = created.team_id;
        owner
            .apply_teams(crate::vault::sync::TeamsUpdate {
                key_seed: Some(owner_keys.seed()),
                teams: vec![created],
                known: Vec::new(),
                listed: vec![team_id],
                checked: vec![team_id],
                closed_invites: Vec::new(),
            })
            .unwrap();
        let web = host("web");
        owner.add_entry(web.clone()).unwrap();
        owner.add_entry(host("private")).unwrap();
        sync_all(&api, &mut owner);
        let general = owner.team_vaults(team_id)[0].0;
        owner.share_entry(web.id, general).unwrap();
        sync_all(&api, &mut owner);
        assert!(!owner.has_unsynced_changes());
        assert_eq!(owner.team_of(web.id), Some(team_id));

        // The guest uses the invite link; nothing is granted yet.
        let (invite_id, invite) =
            team::invite(&api, &owner.team_context().unwrap(), &owner_keys, team_id).unwrap();
        owner.add_invite(invite_id, invite.clone()).unwrap();
        let (name, join) = team::join(
            &api,
            &guest.team_context().unwrap(),
            &guest_keys,
            &invite.link,
        )
        .unwrap();
        assert_eq!(name, "ops");
        guest.add_join(join).unwrap();
        refresh_teams(&api, &mut guest, &guest_keys);
        sync_all(&api, &mut guest);
        assert!(labels(&guest).is_empty());

        // The owner's device checks the request and signs the guest in.
        let (requests, _) = team::requests(&api, &owner.team_context().unwrap(), team_id).unwrap();
        assert_eq!(requests.len(), 1);
        let (request_invite, request) = &requests[0];
        assert_eq!(request.user_id, guest_id);
        team::accept(
            &api,
            &owner.team_context().unwrap(),
            &owner_keys,
            team_id,
            *request_invite,
            request,
        )
        .unwrap();
        refresh_teams(&api, &mut owner, &owner_keys);

        // The guest trusts the team through the inviter's confirmation and sees the host.
        refresh_teams(&api, &mut guest, &guest_keys);
        sync_all(&api, &mut guest);
        assert_eq!(labels(&guest), ["web"]);
        assert_eq!(guest.team_of(web.id), Some(team_id));

        // An edit by the guest reaches the owner.
        guest
            .update_entry(ServerEntry {
                label: "web-2".into(),
                updated_at: 9,
                ..web.clone()
            })
            .unwrap();
        sync_all(&api, &mut guest);
        sync_all(&api, &mut owner);
        assert_eq!(labels(&owner), ["private", "web-2"]);

        // The owner makes the guest an admin, hands the team over and gets it back.
        let owner_id = owner.sync_state().unwrap().user_id;
        team::set_role(
            &api,
            &owner.team_context().unwrap(),
            &owner_keys,
            team_id,
            guest_id,
            "admin",
        )
        .unwrap();
        refresh_teams(&api, &mut owner, &owner_keys);
        team::hand_over(
            &api,
            &owner.team_context().unwrap(),
            &owner_keys,
            team_id,
            guest_id,
        )
        .unwrap();
        refresh_teams(&api, &mut owner, &owner_keys);
        refresh_teams(&api, &mut guest, &guest_keys);
        let roster = |vault: &VaultManager| {
            team::Roster::parse(&vault.team_context().unwrap().teams[&team_id].roster).unwrap()
        };
        assert_eq!(roster(&guest).owner(), Some(guest_id));
        assert!(roster(&owner).is_manager(owner_id));
        // A former owner cannot take the team back on their own.
        assert!(team::hand_over(
            &api,
            &owner.team_context().unwrap(),
            &owner_keys,
            team_id,
            owner_id
        )
        .is_err());
        team::hand_over(
            &api,
            &guest.team_context().unwrap(),
            &guest_keys,
            team_id,
            owner_id,
        )
        .unwrap();
        refresh_teams(&api, &mut guest, &guest_keys);
        refresh_teams(&api, &mut owner, &owner_keys);
        assert_eq!(roster(&owner).owner(), Some(owner_id));

        // The guest is removed: the vault key changes and every item with it.
        team::remove(
            &api,
            &owner.team_context().unwrap(),
            &owner_keys,
            team_id,
            &[guest_id],
        )
        .unwrap();
        // Until the owner fetches the new key, a pull under the old one is set aside.
        sync_all(&api, &mut owner);
        assert!(owner.is_behind_on_teams());
        refresh_teams(&api, &mut owner, &owner_keys);
        sync_all(&api, &mut owner);
        assert_eq!(labels(&owner), ["private", "web-2"]);
        owner
            .update_entry(ServerEntry {
                label: "web-3".into(),
                updated_at: 12,
                ..web.clone()
            })
            .unwrap();
        sync_all(&api, &mut owner);
        assert!(!owner.has_unsynced_changes());

        // The guest's device learns it is out. What it already had stays, as
        // a personal copy that no longer follows the team.
        refresh_teams(&api, &mut guest, &guest_keys);
        assert_eq!(labels(&guest), ["web-2"]);
        assert_ne!(
            guest.get_entry(web.id).unwrap().id,
            web.id,
            "the kept copy has an id of its own"
        );
        assert!(guest.team_context().unwrap().teams.is_empty());
        sync_all(&api, &mut guest);
        sync_all(&api, &mut guest);
        assert!(!guest.has_unsynced_changes());

        // The owner still has everything, and the host can go back to personal.
        owner.unshare_entry(web.id).unwrap();
        // The personal vault still holds the host's old deletion; the first
        // round learns of it and the second sends the host on top.
        sync_all(&api, &mut owner);
        sync_all(&api, &mut owner);
        assert!(!owner.has_unsynced_changes());
        owner.share_entry(web.id, general).unwrap();
        sync_all(&api, &mut owner);
        sync_all(&api, &mut owner);
        assert!(!owner.has_unsynced_changes());
        owner.unshare_entry(web.id).unwrap();
        sync_all(&api, &mut owner);
        sync_all(&api, &mut owner);
        assert!(!owner.has_unsynced_changes());
        assert_eq!(labels(&owner), ["private", "web-3"]);
        assert_eq!(owner.team_of(web.id), None);
    }

    /// A recovery job that claims the given phrase is still the account's.
    fn first_recovery_job(phrase: &RecoveryPhrase, vault: &VaultManager) -> RecoveryJob {
        let slot = vault.password_slot();
        RecoveryJob {
            old_recovery_auth_key: sync::recovery_auth_key(&phrase.to_bytes()),
            new_auth_key: [0; 32],
            new_recovery_auth_key: [0; 32],
            kdf_salt: slot.kdf_salt(),
            kdf_params: slot.kdf_params().clone(),
            enc_dek_by_password: slot.enc_dek_by_password().clone(),
            enc_dek_by_recovery: slot.enc_dek_by_password().clone(),
        }
    }

    #[test]
    fn changes_are_split_into_requests_the_server_accepts() {
        let dir = tempfile::tempdir().unwrap();
        let mut vault = new_vault(&dir, "vault.bin", "pw");
        let state = SyncState::new(uuid::Uuid::nil(), "a".into(), "t".into(), uuid::Uuid::nil());
        vault
            .install_registration(Registration {
                replaces: vault.password_slot().enc_dek_by_password().clone(),
                enc_dek_by_recovery: vault.password_slot().enc_dek_by_password().clone(),
                state,
            })
            .unwrap();
        vault
            .batch(|vault| {
                for index in 0..1_100 {
                    vault.add_entry(host(&format!("host-{index}"))).unwrap();
                }
            })
            .unwrap();

        let job = vault.begin_sync().remove(0);
        let sizes: Vec<usize> = batches(&job.changes)
            .iter()
            .map(|batch| batch.len())
            .collect();
        assert_eq!(sizes, [500, 500, 100]);
    }

    #[test]
    fn weak_or_absurd_key_settings_are_refused() {
        assert!(check_kdf(&KdfParams::default()).is_ok());
        assert!(check_kdf(&KdfParams {
            m_cost: 8,
            t_cost: 1,
            p_cost: 1
        })
        .is_err());
        assert!(check_kdf(&KdfParams {
            m_cost: 4_000_000,
            t_cost: 3,
            p_cost: 1
        })
        .is_err());
        assert!(check_kdf(&KdfParams {
            m_cost: 262_144,
            t_cost: 99,
            p_cost: 1
        })
        .is_err());
    }
}
