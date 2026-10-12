pub mod error;
pub mod format;

pub(crate) mod crypto;
pub(crate) mod memory;
pub mod recovery;
pub(crate) mod storage;
pub mod sync;

pub use error::VaultError;
pub use format::{
    resolve_jump_chain, resolve_proxy, ssh_command, AuthMethod, Group, PortForward, ProxyAuth,
    ProxyChoice, ProxyDef, ProxyDefSummary, ProxyKind, ProxyMode, ProxyRouting, ServerEntry,
    ServerEntrySummary, Snippet, SnippetBody, SnippetUsage, SshKey, SshKeySummary, Tag,
    MAX_JUMP_HOSTS,
};
pub use recovery::RecoveryPhrase;

use std::path::{Path, PathBuf};

use russh::keys;
use secrecy::{ExposeSecret, SecretString};
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

use format::{AeadBlob, KdfParams, VaultFile, VaultPayload, VAULT_VERSION};

/// The folder that holds the vault and the logs. The names differ by
/// platform because early versions took them from the `directories` crate,
/// and the data of existing users is there.
pub fn app_data_dir() -> Option<PathBuf> {
    let base = dirs::data_dir()?;
    if cfg!(target_os = "macos") {
        return Some(base.join("dev.flint.flint.flint"));
    }
    if cfg!(windows) {
        return Some(base.join("flint").join("flint").join("data"));
    }
    Some(base.join("flint"))
}

pub fn default_vault_path() -> Result<PathBuf, VaultError> {
    app_data_dir()
        .map(|dir| dir.join("vault.bin"))
        .ok_or_else(|| {
            VaultError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "could not determine application data directory",
            ))
        })
}

/// Remove the vault file and the temporary file a broken save may have left
/// next to it. Files that are already gone are not an error.
pub fn delete_vault_files(path: &Path) -> Result<(), VaultError> {
    // The vault itself last: after any error it is still there, so the
    // caller can keep the open vault without writing a deleted one back.
    remove_file_if_present(&storage::tmp_path(path))?;
    remove_file_if_present(path)?;
    Ok(())
}

fn remove_file_if_present(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// A saved server that signs in with a given key.
pub struct KeyUser {
    pub label: String,
    pub username: String,
    pub host: String,
}

pub struct VaultManager {
    path: PathBuf,
    /// DEK on the heap so its address is stable for mlock.
    dek: Box<[u8; 32]>,
    kdf_salt: [u8; 32],
    kdf_params: KdfParams,
    enc_dek_by_password: AeadBlob,
    enc_dek_by_recovery: AeadBlob,
    payload: VaultPayload,
    /// Set while [`VaultManager::batch`] runs: record edits skip the file and
    /// one write happens at the end.
    is_deferring_saves: bool,
}

impl std::fmt::Debug for VaultManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaultManager")
            .field("path", &self.path)
            .field("dek", &"[redacted]")
            .finish_non_exhaustive()
    }
}

impl Drop for VaultManager {
    fn drop(&mut self) {
        // Wipe while the page is still locked, so the key cannot be swapped
        // out between the two steps.
        (*self.dek).zeroize();
        memory::unlock_memory(self.dek.as_ptr(), 32);
    }
}

/// What it takes to check the master password and wrap the DEK again: the
/// salt, the KDF cost and the DEK as encrypted under the current password.
/// No key material in the clear, so it is safe to hand to a worker thread.
pub struct PasswordSlot {
    kdf_salt: [u8; 32],
    kdf_params: KdfParams,
    enc_dek_by_password: AeadBlob,
}

/// A password slot for a new master password, ready for
/// [`VaultManager::install_password_slot`].
pub struct NewPasswordSlot {
    /// The slot this one was made from. A vault whose slot has moved on since
    /// must not take it.
    replaces: AeadBlob,
    kdf_salt: [u8; 32],
    enc_dek_by_password: AeadBlob,
    /// What a sync account checks instead of the two passwords.
    current_auth_key: [u8; 32],
    new_auth_key: [u8; 32],
}

/// A recovery slot for a new phrase, ready for
/// [`VaultManager::install_recovery_slot`].
pub struct NewRecoverySlot {
    /// The password slot it was made from; see [`NewPasswordSlot`].
    pub replaces: AeadBlob,
    pub enc_dek_by_recovery: AeadBlob,
}

/// What a sync account needs to hear about a password change.
pub struct AccountPasswordChange {
    pub current_auth_key: [u8; 32],
    pub new_auth_key: [u8; 32],
    pub kdf_salt: [u8; 32],
    pub kdf_params: KdfParams,
    pub enc_dek_by_password: AeadBlob,
}

/// The vault key and the proof of the password, for making a sync account
/// out of this vault.
pub struct OpenedSlot {
    pub dek: Zeroizing<[u8; 32]>,
    pub auth_key: [u8; 32],
}

impl PasswordSlot {
    /// Check `current` against the slot and wrap the same DEK under `new`,
    /// with a fresh salt and the same KDF cost. Runs Argon2id twice, so call
    /// it off the UI thread.
    ///
    /// A wrong `current` gives `InvalidPassword`, an empty `new` gives
    /// `EmptyPassword`.
    pub fn rewrap(
        &self,
        current: &SecretString,
        new: &SecretString,
    ) -> Result<NewPasswordSlot, VaultError> {
        // The unlock form refuses an empty password, so a vault changed to one
        // could only ever be opened with the recovery phrase.
        if new.expose_secret().is_empty() {
            return Err(VaultError::EmptyPassword);
        }

        let current_kek = crypto::derive_kek(
            current.expose_secret().as_bytes(),
            &self.kdf_salt,
            &self.kdf_params,
        )?;
        let dek = VaultManager::unwrap_dek(&current_kek, &self.enc_dek_by_password)
            .map_err(|_| VaultError::InvalidPassword)?;

        let kdf_salt = crypto::generate_salt();
        let new_kek =
            crypto::derive_kek(new.expose_secret().as_bytes(), &kdf_salt, &self.kdf_params)?;

        Ok(NewPasswordSlot {
            replaces: self.enc_dek_by_password.clone(),
            kdf_salt,
            enc_dek_by_password: crypto::encrypt(&new_kek, dek.as_ref()),
            current_auth_key: sync::auth_key(&current_kek),
            new_auth_key: sync::auth_key(&new_kek),
        })
    }

    /// Check `password` and open the slot. Runs Argon2id, so call it off the
    /// UI thread. A wrong password gives `InvalidPassword`.
    pub fn open(&self, password: &SecretString) -> Result<OpenedSlot, VaultError> {
        let kek = crypto::derive_kek(
            password.expose_secret().as_bytes(),
            &self.kdf_salt,
            &self.kdf_params,
        )?;
        let dek = VaultManager::unwrap_dek(&kek, &self.enc_dek_by_password)
            .map_err(|_| VaultError::InvalidPassword)?;
        Ok(OpenedSlot {
            dek,
            auth_key: sync::auth_key(&kek),
        })
    }

    pub fn kdf_salt(&self) -> [u8; 32] {
        self.kdf_salt
    }

    pub fn kdf_params(&self) -> &KdfParams {
        &self.kdf_params
    }

    pub fn enc_dek_by_password(&self) -> &AeadBlob {
        &self.enc_dek_by_password
    }
}

impl NewPasswordSlot {
    pub fn account_change(&self, kdf_params: &KdfParams) -> AccountPasswordChange {
        AccountPasswordChange {
            current_auth_key: self.current_auth_key,
            new_auth_key: self.new_auth_key,
            kdf_salt: self.kdf_salt,
            kdf_params: kdf_params.clone(),
            enc_dek_by_password: self.enc_dek_by_password.clone(),
        }
    }
}

impl VaultManager {
    /// Create a new vault. Returns the manager and a 24-word recovery phrase.
    /// Show the phrase to the user exactly once.
    pub fn create(
        path: &Path,
        password: &SecretString,
    ) -> Result<(Self, RecoveryPhrase), VaultError> {
        Self::create_with_params(path, password, KdfParams::default())
    }

    pub(crate) fn create_with_params(
        path: &Path,
        password: &SecretString,
        kdf_params: KdfParams,
    ) -> Result<(Self, RecoveryPhrase), VaultError> {
        // The unlock form refuses an empty password, so a vault made with one
        // could only ever be opened with the recovery phrase.
        if password.expose_secret().is_empty() {
            return Err(VaultError::EmptyPassword);
        }

        let kdf_salt = crypto::generate_salt();
        let dek = crypto::generate_dek();

        let kek = crypto::derive_kek(password.expose_secret().as_bytes(), &kdf_salt, &kdf_params)?;
        let enc_dek_by_password = crypto::encrypt(&kek, dek.as_ref());

        let (recovery_phrase, recovery_entropy) = RecoveryPhrase::generate();
        let enc_dek_by_recovery = crypto::encrypt(&recovery_entropy, dek.as_ref());

        let payload = VaultPayload {
            entries: Vec::new(),
            keys: Vec::new(),
            snippets: Vec::new(),
            groups: Vec::new(),
            tags: Vec::new(),
            proxies: Vec::new(),
            proxy_routing: ProxyRouting::default(),
            sync: None,
            team_pins: Vec::new(),
            joins: Vec::new(),
        };
        let enc_payload = Self::encrypt_payload(&dek, &payload)?;

        let vault_file = VaultFile {
            version: VAULT_VERSION,
            kdf_salt,
            kdf_params: kdf_params.clone(),
            enc_dek_by_password: enc_dek_by_password.clone(),
            enc_dek_by_recovery: enc_dek_by_recovery.clone(),
            enc_payload,
        };
        storage::write_vault(path, &vault_file)?;

        let manager = Self::new_with_dek(
            path,
            &dek,
            kdf_salt,
            kdf_params,
            enc_dek_by_password,
            enc_dek_by_recovery,
            payload,
        );
        Ok((manager, recovery_phrase))
    }

    /// Unlock an existing vault with the master password.
    ///
    /// A wrong password gives `InvalidPassword`. A payload that does not
    /// decrypt under a valid DEK gives `Corrupted`.
    pub fn unlock_with_password(path: &Path, password: &SecretString) -> Result<Self, VaultError> {
        let vf = storage::read_vault(path)?;

        let kek = crypto::derive_kek(
            password.expose_secret().as_bytes(),
            &vf.kdf_salt,
            &vf.kdf_params,
        )?;
        let dek = Self::unwrap_dek(&kek, &vf.enc_dek_by_password)
            .map_err(|_| VaultError::InvalidPassword)?;

        let payload = Self::decrypt_payload(&dek, &vf.enc_payload)?;

        Ok(Self::new_with_dek(
            path,
            &dek,
            vf.kdf_salt,
            vf.kdf_params,
            vf.enc_dek_by_password,
            vf.enc_dek_by_recovery,
            payload,
        ))
    }

    /// Unlock with a recovery phrase and set a new master password.
    ///
    /// Rotates both DEK slots: the old password and the supplied phrase stop
    /// working. Returns the manager and a new recovery phrase to show the user
    /// once.
    ///
    /// A wrong phrase gives `InvalidRecoveryPhrase`. An empty `new_password`
    /// gives `EmptyPassword` and leaves the vault untouched.
    pub fn unlock_with_recovery(
        path: &Path,
        phrase: &RecoveryPhrase,
        new_password: &SecretString,
    ) -> Result<(Self, RecoveryPhrase), VaultError> {
        Self::unlock_with_recovery_with_params(path, phrase, new_password, KdfParams::default())
    }

    pub(crate) fn unlock_with_recovery_with_params(
        path: &Path,
        phrase: &RecoveryPhrase,
        new_password: &SecretString,
        new_kdf_params: KdfParams,
    ) -> Result<(Self, RecoveryPhrase), VaultError> {
        // The unlock form refuses an empty password, so a vault rotated to one
        // could only ever be opened with the recovery phrase.
        if new_password.expose_secret().is_empty() {
            return Err(VaultError::EmptyPassword);
        }

        let vf = storage::read_vault(path)?;

        let recovery_key = phrase.to_bytes();
        let dek = Self::unwrap_dek(&recovery_key, &vf.enc_dek_by_recovery)
            .map_err(|_| VaultError::InvalidRecoveryPhrase)?;

        let mut payload = Self::decrypt_payload(&dek, &vf.enc_payload)?;

        let new_salt = crypto::generate_salt();
        let new_kek = crypto::derive_kek(
            new_password.expose_secret().as_bytes(),
            &new_salt,
            &new_kdf_params,
        )?;
        let new_enc_dek_by_password = crypto::encrypt(&new_kek, dek.as_ref());

        let (new_phrase, new_recovery_entropy) = RecoveryPhrase::generate();
        let new_enc_dek_by_recovery = crypto::encrypt(&new_recovery_entropy, dek.as_ref());

        Self::note_recovery(&mut payload, &recovery_key, &new_kek, &new_recovery_entropy);
        let enc_payload = Self::encrypt_payload(&dek, &payload)?;

        let new_vf = VaultFile {
            version: VAULT_VERSION,
            kdf_salt: new_salt,
            kdf_params: new_kdf_params.clone(),
            enc_dek_by_password: new_enc_dek_by_password.clone(),
            enc_dek_by_recovery: new_enc_dek_by_recovery.clone(),
            enc_payload,
        };
        storage::write_vault(path, &new_vf)?;

        let manager = Self::new_with_dek(
            path,
            &dek,
            new_salt,
            new_kdf_params,
            new_enc_dek_by_password,
            new_enc_dek_by_recovery,
            payload,
        );
        Ok((manager, new_phrase))
    }

    /// The public half of the password slot, for [`PasswordSlot::rewrap`].
    ///
    /// Changing the password is split in two because Argon2id is too slow for
    /// the UI thread and the manager cannot leave it: the snapshot goes to a
    /// worker, and the result comes back to [`Self::install_password_slot`].
    pub fn password_slot(&self) -> PasswordSlot {
        PasswordSlot {
            kdf_salt: self.kdf_salt,
            kdf_params: self.kdf_params.clone(),
            enc_dek_by_password: self.enc_dek_by_password.clone(),
        }
    }

    /// Switch to a password slot made by [`PasswordSlot::rewrap`] and save.
    /// `enc_dek_by_recovery` is not modified, so the recovery phrase stays valid.
    ///
    /// The slot must come from this vault's current slot, otherwise
    /// `StalePasswordSlot`. If the write fails the old slot stays in memory
    /// too, so a later save cannot slip the new password in unnoticed.
    pub fn install_password_slot(&mut self, slot: NewPasswordSlot) -> Result<(), VaultError> {
        if slot.replaces != self.enc_dek_by_password {
            return Err(VaultError::StalePasswordSlot);
        }

        let old_salt = std::mem::replace(&mut self.kdf_salt, slot.kdf_salt);
        let old_blob = std::mem::replace(&mut self.enc_dek_by_password, slot.enc_dek_by_password);

        let saved = self.save();
        if saved.is_err() {
            self.kdf_salt = old_salt;
            self.enc_dek_by_password = old_blob;
        }
        saved
    }

    /// Switch to a new recovery phrase and save. The old phrase stops
    /// opening the vault; the password is not touched.
    pub fn install_recovery_slot(&mut self, slot: NewRecoverySlot) -> Result<(), VaultError> {
        if slot.replaces != self.enc_dek_by_password {
            return Err(VaultError::StalePasswordSlot);
        }
        let old = std::mem::replace(&mut self.enc_dek_by_recovery, slot.enc_dek_by_recovery);
        let saved = self.save();
        if saved.is_err() {
            self.enc_dek_by_recovery = old;
        }
        saved
    }

    /// Zero the DEK and drop the vault state. The decrypted records are
    /// freed, not wiped.
    pub fn lock(self) {
        drop(self);
    }

    pub fn list_entries(&self) -> Result<Vec<ServerEntrySummary>, VaultError> {
        Ok(self
            .payload
            .entries
            .iter()
            .map(|e| {
                let mut summary = ServerEntrySummary::from(e);
                summary.group_name = e.group.and_then(|id| self.group_path_name(id));
                summary.tag_names = e
                    .tags
                    .iter()
                    .filter_map(|id| self.payload.tags.iter().find(|t| t.id == *id))
                    .map(|t| t.name.clone())
                    .collect();
                summary.team_name = self.sharing_label(e.id);
                summary
            })
            .collect())
    }

    /// Full "parent / child" display path for a group, walking `parent_id`
    /// upward. `None` when the id doesn't resolve (stale reference).
    fn group_path_name(&self, id: Uuid) -> Option<String> {
        let mut parts = Vec::new();
        let mut cursor = Some(id);
        while let Some(gid) = cursor {
            let group = self.payload.groups.iter().find(|g| g.id == gid)?;
            parts.push(group.name.clone());
            cursor = group.parent_id;
            if parts.len() > 16 {
                break; // a parent loop in a damaged vault must not hang the UI
            }
        }
        parts.reverse();
        Some(parts.join(" / "))
    }

    pub fn get_entry(&self, id: Uuid) -> Result<ServerEntry, VaultError> {
        let id = self.current_id(id);
        self.payload
            .entries
            .iter()
            .find(|e| e.id == id)
            .cloned()
            .ok_or(VaultError::EntryNotFound(id))
    }

    /// Run many record edits and write the vault once at the end. Every
    /// record method saves on its own, and a save re-encrypts and rewrites the
    /// whole payload, so an import of fifty hosts would otherwise do it fifty
    /// times over. The edits still see their own changes; only the file write
    /// is deferred.
    pub fn batch<T>(&mut self, edits: impl FnOnce(&mut Self) -> T) -> Result<T, VaultError> {
        // A batch inside a batch must not end the outer one early.
        let was_deferring = std::mem::replace(&mut self.is_deferring_saves, true);
        let out = edits(self);
        self.is_deferring_saves = was_deferring;
        self.save()?;
        Ok(out)
    }

    pub fn add_entry(&mut self, entry: ServerEntry) -> Result<(), VaultError> {
        self.check_jump_link(entry.id, entry.jump_host)?;
        self.payload.entries.push(entry);
        self.save()
    }

    pub fn update_entry(&mut self, entry: ServerEntry) -> Result<(), VaultError> {
        let pos = self
            .payload
            .entries
            .iter()
            .position(|e| e.id == entry.id)
            .ok_or(VaultError::EntryNotFound(entry.id))?;
        self.check_jump_link(entry.id, entry.jump_host)?;
        let id = entry.id;
        let key_id = match &entry.auth {
            AuthMethod::PrivateKey { key_id } => Some(*key_id),
            _ => None,
        };
        self.payload.entries[pos] = entry;
        // Whoever sees a shared host has to see the key it logs in with.
        if let (Some(vault_id), Some(key_id)) = (self.vault_of(id), key_id) {
            self.bring_key(id, key_id, vault_id);
        }
        self.prune_unused_groups_and_tags();
        self.save()
    }

    /// Persist an auto-detected OS string for an entry. No-op (and no write)
    /// if the value is unchanged.
    pub fn set_entry_os(&mut self, id: Uuid, os: String) -> Result<(), VaultError> {
        let id = self.current_id(id);
        let entry = self
            .payload
            .entries
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or(VaultError::EntryNotFound(id))?;
        if entry.os.as_deref() == Some(os.as_str()) {
            return Ok(());
        }
        entry.os = Some(os);
        self.save()
    }

    /// Persist the host key fingerprint the user trusted for this entry
    /// (TOFU). No-op (and no write) if the value is unchanged.
    pub fn set_entry_host_key(&mut self, id: Uuid, fingerprint: String) -> Result<(), VaultError> {
        let id = self.current_id(id);
        let entry = self
            .payload
            .entries
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or(VaultError::EntryNotFound(id))?;
        if entry.host_key_fingerprint.as_deref() == Some(fingerprint.as_str()) {
            return Ok(());
        }
        entry.host_key_fingerprint = Some(fingerprint);
        self.save()
    }

    /// Replace the stored password of an entry that logs in with one. An entry
    /// that uses a key or the agent is left alone: the caller only knows a
    /// password worked, not that the user wants to switch methods.
    pub fn set_entry_password(&mut self, id: Uuid, password: &str) -> Result<(), VaultError> {
        let id = self.current_id(id);
        let entry = self
            .payload
            .entries
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or(VaultError::EntryNotFound(id))?;
        let AuthMethod::Password(stored) = &mut entry.auth else {
            return Ok(());
        };
        if stored == password {
            return Ok(());
        }
        *stored = password.to_string();
        self.save()
    }

    /// Persist the folders the SFTP browser was last showing for this host, so a
    /// restart or reconnect reopens them instead of the home dir. No-op (and no
    /// write) if both values are unchanged.
    pub fn set_entry_last_dirs(
        &mut self,
        id: Uuid,
        local: String,
        remote: String,
    ) -> Result<(), VaultError> {
        let id = self.current_id(id);
        let entry = self
            .payload
            .entries
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or(VaultError::EntryNotFound(id))?;
        if entry.last_local_dir.as_deref() == Some(local.as_str())
            && entry.last_remote_dir.as_deref() == Some(remote.as_str())
        {
            return Ok(());
        }
        entry.last_local_dir = Some(local);
        entry.last_remote_dir = Some(remote);
        self.save()
    }

    /// Record that a session was just opened to this host. Updates the
    /// recency timestamp the sidebar uses to order "recent" hosts, then saves.
    pub fn touch_entry_opened(&mut self, id: Uuid) -> Result<(), VaultError> {
        let id = self.current_id(id);
        let entry = self
            .payload
            .entries
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or(VaultError::EntryNotFound(id))?;
        entry.last_opened_at = Some(unix_now() as i64);
        self.save()
    }

    /// Remove a host and drop every jump-host link pointing at it, so the hosts
    /// behind it fall back to a direct connect instead of failing to resolve.
    /// The remove dialog names them first, see `AppView::show_remove_dialog`.
    pub fn delete_entry(&mut self, id: Uuid) -> Result<(), VaultError> {
        let id = self.current_id(id);
        let before = self.payload.entries.len();
        self.payload.entries.retain(|e| e.id != id);
        if self.payload.entries.len() == before {
            return Err(VaultError::EntryNotFound(id));
        }
        for entry in &mut self.payload.entries {
            if entry.jump_host == Some(id) {
                tracing::info!(host = %entry.label, "jump host deleted, host now connects directly");
                entry.jump_host = None;
            }
        }
        self.prune_unused_groups_and_tags();
        self.save()
    }

    /// Names of the hosts that reach the given one through a jump-host link.
    /// Deleting it drops those links, so the user is told before it happens.
    pub fn servers_using_jump_host(&self, id: Uuid) -> Vec<String> {
        self.payload
            .entries
            .iter()
            .filter(|e| e.jump_host == Some(id))
            .map(|e| {
                if e.label.is_empty() {
                    e.host.clone()
                } else {
                    e.label.clone()
                }
            })
            .collect()
    }

    /// Reject a jump-host link that would make the host unreachable: a loop
    /// back onto itself, or a chain deeper than [`MAX_JUMP_HOSTS`]. A candidate
    /// whose own chain is already broken is left alone: that is someone else's
    /// record to fix, and the connect attempt reports it.
    fn check_jump_link(&self, id: Uuid, jump: Option<Uuid>) -> Result<(), VaultError> {
        let Some(jump_id) = jump else { return Ok(()) };
        let candidate = self.get_entry(jump_id)?;
        if jump_id == id {
            return Err(VaultError::JumpCycle(candidate.label));
        }
        let Ok(chain) = resolve_jump_chain(&candidate, &self.payload.entries) else {
            return Ok(());
        };
        if chain.iter().any(|hop| hop.id == id) {
            return Err(VaultError::JumpCycle(candidate.label));
        }
        // The chain is the candidate plus its own bastions, all of which the
        // edited host, and everything already behind it, would now sit behind.
        if chain.len() + self.depth_behind(id) > MAX_JUMP_HOSTS {
            return Err(VaultError::JumpTooDeep(MAX_JUMP_HOSTS));
        }
        Ok(())
    }

    /// How many jump-host levels hang off this host, walking the links
    /// backwards. 0 = nothing routes through it. Stops at the cap: past it the
    /// exact number changes nothing, and the walk must not follow a loop
    /// forever if the vault file was edited by hand.
    fn depth_behind(&self, id: Uuid) -> usize {
        let mut level = vec![id];
        let mut depth = 0;
        while depth <= MAX_JUMP_HOSTS {
            let next: Vec<Uuid> = self
                .payload
                .entries
                .iter()
                .filter(|e| e.jump_host.is_some_and(|jump| level.contains(&jump)))
                .map(|e| e.id)
                .collect();
            if next.is_empty() {
                return depth;
            }
            depth += 1;
            level = next;
        }
        depth
    }

    /// Set (or clear) the host this entry is tunnelled through. No-op (and no
    /// write) if unchanged. Used by the ssh-config import, which can only link
    /// the chain once every host it names exists.
    pub fn set_entry_jump_host(&mut self, id: Uuid, jump: Option<Uuid>) -> Result<(), VaultError> {
        let id = self.current_id(id);
        self.check_jump_link(id, jump)?;
        let entry = self
            .payload
            .entries
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or(VaultError::EntryNotFound(id))?;
        if entry.jump_host == jump {
            return Ok(());
        }
        entry.jump_host = jump;
        self.save()
    }

    /// The hosts to authenticate against to reach this one, in connect order:
    /// outermost bastion first, the host itself last. Cloned so callers can
    /// drop the vault borrow before spawning the connection.
    pub fn resolve_jump_chain_for(&self, entry_id: Uuid) -> Result<Vec<ServerEntry>, VaultError> {
        let entry_id = self.current_id(entry_id);
        let entry = self
            .payload
            .entries
            .iter()
            .find(|e| e.id == entry_id)
            .ok_or(VaultError::EntryNotFound(entry_id))?;
        let chain = resolve_jump_chain(entry, &self.payload.entries)?;
        Ok(chain.into_iter().cloned().collect())
    }

    /// The `ssh` command line that reaches this host from a plain terminal,
    /// with its jump hosts as `-J`.
    pub fn ssh_command_for(&self, entry_id: Uuid) -> Result<String, VaultError> {
        let entry_id = self.current_id(entry_id);
        let entry = self
            .payload
            .entries
            .iter()
            .find(|e| e.id == entry_id)
            .ok_or(VaultError::EntryNotFound(entry_id))?;
        let chain = resolve_jump_chain(entry, &self.payload.entries).unwrap_or_else(|e| {
            // A broken chain cannot be written as -J; the direct command is
            // still more useful than copying nothing.
            tracing::warn!(error = %e, host = %entry.host, "jump chain is broken, copying the direct ssh command");
            vec![entry]
        });
        Ok(ssh_command(&chain))
    }

    /// Hosts that may serve as `exclude`'s jump host: everything the save
    /// would accept, sorted by label like the other pickers.
    pub fn list_jump_candidates(&self, exclude: Option<Uuid>) -> Vec<ServerEntrySummary> {
        // The hosts already behind the edited one are the same for every
        // candidate, so the walk happens once instead of once per row.
        let behind = exclude.map_or(0, |id| self.depth_behind(id));
        let mut candidates: Vec<ServerEntrySummary> = self
            .payload
            .entries
            .iter()
            .filter(|e| Some(e.id) != exclude)
            .filter(|e| self.is_pickable_jump_host(e, exclude, behind))
            .map(ServerEntrySummary::from)
            .collect();
        candidates.sort_by_key(|c| c.label.to_lowercase());
        candidates
    }

    /// Mirrors [`Self::check_jump_link`], so the picker never offers a host
    /// the save would reject: no loop back through the edited host, and no
    /// chain that one more hop would push past [`MAX_JUMP_HOSTS`]. A candidate
    /// whose own chain is already broken stays pickable: that is a different
    /// record to fix, and the connect attempt reports it.
    fn is_pickable_jump_host(
        &self,
        candidate: &ServerEntry,
        exclude: Option<Uuid>,
        behind: usize,
    ) -> bool {
        let Ok(chain) = resolve_jump_chain(candidate, &self.payload.entries) else {
            return true;
        };
        if exclude.is_some_and(|id| chain.iter().any(|hop| hop.id == id)) {
            return false;
        }
        chain.len() + behind <= MAX_JUMP_HOSTS
    }

    // --- key CRUD ---

    pub fn add_key(
        &mut self,
        name: String,
        pem: String,
        passphrase: Option<String>,
    ) -> Result<Uuid, VaultError> {
        let (fingerprint, key_type) = derive_key_metadata(&pem, passphrase.as_deref())?;
        self.push_key(name, pem, passphrase, fingerprint, key_type)
    }

    fn push_key(
        &mut self,
        name: String,
        pem: String,
        passphrase: Option<String>,
        fingerprint: String,
        key_type: String,
    ) -> Result<Uuid, VaultError> {
        let id = Uuid::new_v4();
        self.payload.keys.push(SshKey {
            id,
            name,
            created_at: unix_now(),
            fingerprint,
            key_type,
            pem,
            passphrase,
        });
        self.save()?;
        Ok(id)
    }

    /// [`add_key`](Self::add_key), but returns the existing key's id when one
    /// with the same fingerprint is already stored. Import paths call this so
    /// re-importing doesn't pile up duplicates.
    pub fn add_key_dedup(
        &mut self,
        name: String,
        pem: String,
        passphrase: Option<String>,
    ) -> Result<Uuid, VaultError> {
        // Reading a key with a passphrase is slow, so it happens once here
        // and not again in `add_key`.
        let (fingerprint, key_type) = derive_key_metadata(&pem, passphrase.as_deref())?;
        if let Some(existing) = self
            .payload
            .keys
            .iter()
            .find(|k| k.fingerprint == fingerprint)
        {
            return Ok(existing.id);
        }
        self.push_key(name, pem, passphrase, fingerprint, key_type)
    }

    pub fn rename_key(&mut self, id: Uuid, new_name: String) -> Result<(), VaultError> {
        let key = self
            .payload
            .keys
            .iter_mut()
            .find(|k| k.id == id)
            .ok_or(VaultError::KeyNotFound(id))?;
        key.name = new_name;
        self.save()
    }

    pub fn delete_key(&mut self, id: Uuid) -> Result<(), VaultError> {
        let before = self.payload.keys.len();
        self.payload.keys.retain(|k| k.id != id);
        if self.payload.keys.len() == before {
            return Err(VaultError::KeyNotFound(id));
        }
        self.save()
    }

    pub fn list_keys(&self) -> Result<Vec<SshKeySummary>, VaultError> {
        Ok(self.payload.keys.iter().map(SshKeySummary::from).collect())
    }

    pub fn get_key(&self, id: Uuid) -> Result<&SshKey, VaultError> {
        self.payload
            .keys
            .iter()
            .find(|k| k.id == id)
            .ok_or(VaultError::KeyNotFound(id))
    }

    pub fn servers_using_key(&self, key_id: Uuid) -> Vec<KeyUser> {
        self.payload
            .entries
            .iter()
            .filter(|e| matches!(&e.auth, AuthMethod::PrivateKey { key_id: k } if *k == key_id))
            .map(|e| KeyUser {
                label: e.label.clone(),
                username: e.username.clone(),
                host: e.host.clone(),
            })
            .collect()
    }

    // --- proxy CRUD + routing ---

    pub fn add_proxy(
        &mut self,
        name: String,
        kind: ProxyKind,
        host: String,
        port: u16,
        auth: Option<ProxyAuth>,
    ) -> Result<Uuid, VaultError> {
        let id = Uuid::new_v4();
        self.payload.proxies.push(ProxyDef {
            id,
            name,
            kind,
            host,
            port,
            auth,
        });
        self.save()?;
        Ok(id)
    }

    pub fn update_proxy(
        &mut self,
        id: Uuid,
        name: String,
        kind: ProxyKind,
        host: String,
        port: u16,
        auth: Option<ProxyAuth>,
    ) -> Result<(), VaultError> {
        let p = self
            .payload
            .proxies
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or(VaultError::ProxyNotFound(id))?;
        p.name = name;
        p.kind = kind;
        p.host = host;
        p.port = port;
        p.auth = auth;
        self.save()
    }

    /// Remove a proxy and scrub every reference to it: the global rule's
    /// default and any per-host `Use(id)` override fall back so nothing points
    /// at a dangling id.
    pub fn delete_proxy(&mut self, id: Uuid) -> Result<(), VaultError> {
        let before = self.payload.proxies.len();
        self.payload.proxies.retain(|p| p.id != id);
        if self.payload.proxies.len() == before {
            return Err(VaultError::ProxyNotFound(id));
        }
        if self.payload.proxy_routing.default_proxy == Some(id) {
            self.payload.proxy_routing.default_proxy = None;
        }
        for entry in &mut self.payload.entries {
            if entry.proxy == ProxyChoice::Use(id) {
                entry.proxy = ProxyChoice::Default;
            }
        }
        self.save()
    }

    pub fn list_proxies(&self) -> Vec<ProxyDefSummary> {
        self.payload
            .proxies
            .iter()
            .map(ProxyDefSummary::from)
            .collect()
    }

    pub fn get_proxy(&self, id: Uuid) -> Result<&ProxyDef, VaultError> {
        self.payload
            .proxies
            .iter()
            .find(|p| p.id == id)
            .ok_or(VaultError::ProxyNotFound(id))
    }

    pub fn proxy_routing(&self) -> &ProxyRouting {
        &self.payload.proxy_routing
    }

    pub fn set_proxy_routing(&mut self, routing: ProxyRouting) -> Result<(), VaultError> {
        self.payload.proxy_routing = routing;
        self.save()
    }

    /// Set a single host's proxy override. No-op (and no write) if unchanged.
    pub fn set_entry_proxy_choice(
        &mut self,
        id: Uuid,
        choice: ProxyChoice,
    ) -> Result<(), VaultError> {
        let id = self.current_id(id);
        let entry = self
            .payload
            .entries
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or(VaultError::EntryNotFound(id))?;
        if entry.proxy == choice {
            return Ok(());
        }
        entry.proxy = choice;
        self.save()
    }

    /// Resolve the effective proxy for a host, applying its per-host override
    /// on top of the global rule. `None` = connect directly.
    pub fn resolve_proxy_for(&self, entry_id: Uuid) -> Option<&ProxyDef> {
        let entry_id = self.current_id(entry_id);
        let entry = self.payload.entries.iter().find(|e| e.id == entry_id)?;
        resolve_proxy(entry, &self.payload.proxy_routing, &self.payload.proxies)
    }

    // --- tag / group CRUD (visual labels and folders) ---

    pub fn list_tags(&self) -> Vec<Tag> {
        self.payload.tags.clone()
    }

    #[cfg(test)]
    pub fn list_groups(&self) -> Vec<Group> {
        self.payload.groups.clone()
    }

    /// Every group with its flattened "parent / child" display path, sorted
    /// alphabetically: what pickers show and match against.
    pub fn list_group_paths(&self) -> Vec<(Uuid, String)> {
        let mut paths: Vec<(Uuid, String)> = self
            .payload
            .groups
            .iter()
            .filter_map(|g| self.group_path_name(g.id).map(|p| (g.id, p)))
            .collect();
        paths.sort_by_key(|a| a.1.to_lowercase());
        paths
    }

    /// Case-insensitive lookup by name; creates (and saves) when missing.
    /// Import paths call this so re-importing doesn't duplicate tags.
    pub fn find_or_create_tag(&mut self, name: &str) -> Result<Uuid, VaultError> {
        let name = name.trim();
        if let Some(tag) = self
            .payload
            .tags
            .iter()
            .find(|t| t.name.eq_ignore_ascii_case(name))
        {
            return Ok(tag.id);
        }
        let id = Uuid::new_v4();
        self.payload.tags.push(Tag {
            id,
            name: name.to_string(),
        });
        self.save()?;
        Ok(id)
    }

    /// Case-insensitive lookup by (name, parent); creates (and saves) when
    /// missing. Call once per path segment to build nested folders.
    pub fn find_or_create_group(
        &mut self,
        name: &str,
        parent_id: Option<Uuid>,
    ) -> Result<Uuid, VaultError> {
        let name = name.trim();
        if let Some(group) = self
            .payload
            .groups
            .iter()
            .find(|g| g.parent_id == parent_id && g.name.eq_ignore_ascii_case(name))
        {
            return Ok(group.id);
        }
        let id = Uuid::new_v4();
        self.payload.groups.push(Group {
            id,
            name: name.to_string(),
            parent_id,
        });
        self.save()?;
        Ok(id)
    }

    /// Drop tags and groups no longer referenced by any entry, so editing or
    /// deleting hosts doesn't leave orphaned labels/folders cluttering the
    /// pickers. A group is kept while a host is in it *or* it's an ancestor of
    /// one that is, so the intermediate folders of a used nested path survive.
    /// Does not save: the caller folds it into its own save.
    fn prune_unused_groups_and_tags(&mut self) {
        use std::collections::{HashMap, HashSet};

        let used_tags: HashSet<Uuid> = self
            .payload
            .entries
            .iter()
            .flat_map(|e| e.tags.iter().copied())
            .collect();
        self.payload.tags.retain(|t| used_tags.contains(&t.id));

        // Seed with the groups entries point at, then walk parent links so
        // ancestors of a used folder are kept too.
        let mut used_groups: HashSet<Uuid> = self
            .payload
            .entries
            .iter()
            .filter_map(|e| e.group)
            .collect();
        let parents: HashMap<Uuid, Option<Uuid>> = self
            .payload
            .groups
            .iter()
            .map(|g| (g.id, g.parent_id))
            .collect();
        let seeds: Vec<Uuid> = used_groups.iter().copied().collect();
        for seed in seeds {
            let mut cursor = parents.get(&seed).copied().flatten();
            // `insert` returns false once we reach an already-seen ancestor,
            // which also guards against malformed parent cycles.
            while let Some(pid) = cursor {
                if !used_groups.insert(pid) {
                    break;
                }
                cursor = parents.get(&pid).copied().flatten();
            }
        }
        self.payload.groups.retain(|g| used_groups.contains(&g.id));
    }

    // --- snippet CRUD ---

    pub fn add_snippet(
        &mut self,
        name: String,
        description: Option<String>,
        body: SnippetBody,
    ) -> Result<Uuid, VaultError> {
        let id = Uuid::new_v4();
        let now = unix_now() as i64;
        self.payload.snippets.push(Snippet {
            id,
            name,
            description,
            created_at: now,
            updated_at: now,
            body,
            usages: Vec::new(),
        });
        self.save()?;
        Ok(id)
    }

    /// Record that a snippet was just run on the given server. Bumps the
    /// per-server recency the command palette uses to surface recently-used
    /// snippets, then saves.
    pub fn touch_snippet_used(
        &mut self,
        snippet_id: Uuid,
        server_id: Uuid,
    ) -> Result<(), VaultError> {
        let server_id = self.current_id(server_id);
        let s = self
            .payload
            .snippets
            .iter_mut()
            .find(|s| s.id == snippet_id)
            .ok_or(VaultError::SnippetNotFound(snippet_id))?;
        let now = unix_now() as i64;
        match s.usages.iter_mut().find(|u| u.server_id == server_id) {
            Some(u) => u.at = now,
            None => s.usages.push(SnippetUsage { server_id, at: now }),
        }
        self.save()
    }

    pub fn update_snippet(
        &mut self,
        id: Uuid,
        name: String,
        description: Option<String>,
        body: SnippetBody,
    ) -> Result<(), VaultError> {
        let s = self
            .payload
            .snippets
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or(VaultError::SnippetNotFound(id))?;
        s.name = name;
        s.description = description;
        s.body = body;
        s.updated_at = unix_now() as i64;
        self.save()
    }

    pub fn delete_snippet(&mut self, id: Uuid) -> Result<(), VaultError> {
        let before = self.payload.snippets.len();
        self.payload.snippets.retain(|s| s.id != id);
        if self.payload.snippets.len() == before {
            return Err(VaultError::SnippetNotFound(id));
        }
        for entry in &mut self.payload.entries {
            if entry.startup_snippet == Some(id) {
                entry.startup_snippet = None;
            }
        }
        self.save()
    }

    /// Case-insensitive lookup by name. Import paths use it to attach a host
    /// to a snippet the same file brought in.
    pub fn find_snippet_by_name(&self, name: &str) -> Option<Uuid> {
        let name = name.trim();
        self.payload
            .snippets
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(name))
            .map(|s| s.id)
    }

    pub fn list_snippets(&self) -> Result<Vec<Snippet>, VaultError> {
        Ok(self.payload.snippets.clone())
    }

    pub fn get_snippet(&self, id: Uuid) -> Result<&Snippet, VaultError> {
        self.payload
            .snippets
            .iter()
            .find(|s| s.id == id)
            .ok_or(VaultError::SnippetNotFound(id))
    }

    // --- private helpers ---

    fn new_with_dek(
        path: &Path,
        dek: &[u8; 32],
        kdf_salt: [u8; 32],
        kdf_params: KdfParams,
        enc_dek_by_password: AeadBlob,
        enc_dek_by_recovery: AeadBlob,
        payload: VaultPayload,
    ) -> Self {
        let boxed = Box::new(*dek);
        memory::lock_memory(boxed.as_ref().as_ptr(), 32);

        Self {
            path: path.to_owned(),
            dek: boxed,
            kdf_salt,
            kdf_params,
            enc_dek_by_password,
            enc_dek_by_recovery,
            payload,
            is_deferring_saves: false,
        }
    }

    /// Decrypt a DEK blob. Returns `Corrupted` on AEAD failure.
    /// Callers map this to `InvalidPassword` or `InvalidRecoveryPhrase` as appropriate.
    pub(crate) fn unwrap_dek(
        key: &[u8; 32],
        blob: &AeadBlob,
    ) -> Result<Zeroizing<[u8; 32]>, VaultError> {
        let raw = crypto::decrypt(key, blob)?;
        if raw.len() != 32 {
            return Err(VaultError::Corrupted(format!(
                "DEK blob has unexpected length: {}",
                raw.len()
            )));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&raw);
        Ok(Zeroizing::new(arr))
    }

    fn save(&self) -> Result<(), VaultError> {
        if self.is_deferring_saves {
            return Ok(());
        }
        let enc_payload = Self::encrypt_payload(self.dek.as_ref(), &self.payload)?;
        let vf = VaultFile {
            version: VAULT_VERSION,
            kdf_salt: self.kdf_salt,
            kdf_params: self.kdf_params.clone(),
            enc_dek_by_password: self.enc_dek_by_password.clone(),
            enc_dek_by_recovery: self.enc_dek_by_recovery.clone(),
            enc_payload,
        };
        storage::write_vault(&self.path, &vf)
    }

    fn encrypt_payload(dek: &[u8; 32], payload: &VaultPayload) -> Result<AeadBlob, VaultError> {
        // Every password and private key in the clear, so wipe it on the way out.
        let mut buf = Zeroizing::new(Vec::new());
        ciborium::into_writer(payload, &mut *buf)
            .map_err(|e| VaultError::Corrupted(format!("CBOR encode: {e}")))?;
        Ok(crypto::encrypt(dek, &buf))
    }

    fn decrypt_payload(dek: &[u8; 32], blob: &AeadBlob) -> Result<VaultPayload, VaultError> {
        let raw = crypto::decrypt(dek, blob)?;
        ciborium::from_reader(&raw[..])
            .map_err(|e| VaultError::Corrupted(format!("CBOR decode: {e}")))
    }
}

fn derive_key_metadata(
    pem: &str,
    passphrase: Option<&str>,
) -> Result<(String, String), VaultError> {
    let pass_str = passphrase.filter(|p| !p.is_empty());
    let key_pair = keys::decode_secret_key(pem, pass_str)
        .map_err(|e| VaultError::InvalidKey(e.to_string()))?;
    let pub_key = key_pair
        .clone_public_key()
        .map_err(|e| VaultError::InvalidKey(e.to_string()))?;
    let fingerprint = format!("SHA256:{}", pub_key.fingerprint());
    let key_type = key_type_display(&key_pair, &pub_key);
    Ok((fingerprint, key_type))
}

fn key_type_display(pair: &keys::key::KeyPair, pub_key: &keys::key::PublicKey) -> String {
    use keys::key::{KeyPair, PublicKey};
    match pair {
        KeyPair::Ed25519(_) => "Ed25519".to_string(),
        KeyPair::RSA { .. } => {
            if let PublicKey::RSA { ref key, .. } = pub_key {
                let proto = keys::protocol::RsaPublicKey::from(key);
                format!("RSA {}", proto.modulus.len() * 8)
            } else {
                "RSA".to_string()
            }
        }
        KeyPair::EC { ref key } => match key.algorithm() {
            "ecdsa-sha2-nistp256" => "ECDSA P-256".to_string(),
            "ecdsa-sha2-nistp384" => "ECDSA P-384".to_string(),
            "ecdsa-sha2-nistp521" => "ECDSA P-521".to_string(),
            alg => alg.to_string(),
        },
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::ForwardKind;
    use secrecy::SecretString;
    use tempfile::tempdir;

    #[test]
    fn the_data_folder_keeps_the_name_existing_users_have() {
        let dir = app_data_dir().unwrap();
        let expected = if cfg!(target_os = "macos") {
            "Application Support/dev.flint.flint.flint"
        } else if cfg!(windows) {
            "flint/flint/data"
        } else {
            "flint"
        };
        assert!(dir.ends_with(expected), "{dir:?}");
    }

    fn fast_kdf() -> KdfParams {
        KdfParams {
            m_cost: 256,
            t_cost: 1,
            p_cost: 1,
        }
    }

    fn pw(s: &str) -> SecretString {
        SecretString::new(s.to_string())
    }

    fn make_vault(dir: &std::path::Path) -> (VaultManager, RecoveryPhrase) {
        let path = dir.join("vault.bin");
        VaultManager::create_with_params(&path, &pw("hunter2"), fast_kdf()).unwrap()
    }

    fn test_entry() -> ServerEntry {
        ServerEntry {
            id: Uuid::new_v4(),
            label: "Prod web server".into(),
            host: "prod.example.com".into(),
            port: 22,
            username: "deploy".into(),
            auth: AuthMethod::Password("s3cr3t".into()),
            notes: Some("main production host".into()),
            os: None,
            last_opened_at: None,
            host_key_fingerprint: None,
            last_remote_dir: None,
            last_local_dir: None,
            group: None,
            tags: Vec::new(),
            proxy: ProxyChoice::Default,
            jump_host: None,
            forwards: Vec::new(),
            startup_snippet: None,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        }
    }

    #[test]
    fn servers_using_key_reports_the_real_login() {
        let dir = tempfile::tempdir().unwrap();
        let (mut vault, _) = make_vault(dir.path());
        let key_id = Uuid::new_v4();
        let mut with_key = test_entry();
        with_key.auth = AuthMethod::PrivateKey { key_id };
        vault.add_entry(with_key).unwrap();
        vault.add_entry(test_entry()).unwrap();

        let users = vault.servers_using_key(key_id);
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].label, "Prod web server");
        assert_eq!(users[0].username, "deploy");
        assert_eq!(users[0].host, "prod.example.com");
    }

    /// A named host, so jump-chain assertions read as the labels the user sees.
    fn named_entry(label: &str) -> ServerEntry {
        ServerEntry {
            label: label.into(),
            ..test_entry()
        }
    }

    /// `count` hosts, each behind the one before it. Returns their ids in the
    /// same order, outermost first.
    fn add_chain(vm: &mut VaultManager, count: usize) -> Vec<Uuid> {
        let mut ids = Vec::new();
        for index in 0..count {
            let mut entry = named_entry(&format!("hop{index}"));
            entry.jump_host = ids.last().copied();
            ids.push(entry.id);
            vm.add_entry(entry).unwrap();
        }
        ids
    }

    #[test]
    fn jump_chain_resolves_outermost_first() {
        let dir = tempdir().unwrap();
        let (mut vm, _) = make_vault(dir.path());
        let ids = add_chain(&mut vm, 3);

        let chain = vm.resolve_jump_chain_for(ids[2]).unwrap();
        assert_eq!(
            chain.iter().map(|e| e.label.as_str()).collect::<Vec<_>>(),
            vec!["hop0", "hop1", "hop2"],
        );
        // A host with no jump host resolves to just itself.
        assert_eq!(vm.resolve_jump_chain_for(ids[0]).unwrap().len(), 1);
    }

    #[test]
    fn a_host_cannot_jump_through_itself() {
        let dir = tempdir().unwrap();
        let (mut vm, _) = make_vault(dir.path());
        let entry = named_entry("solo");
        let id = entry.id;
        vm.add_entry(entry).unwrap();

        let err = vm.set_entry_jump_host(id, Some(id)).unwrap_err();
        assert!(matches!(err, VaultError::JumpCycle(_)), "got {err:?}");
        assert_eq!(vm.get_entry(id).unwrap().jump_host, None);
    }

    #[test]
    fn closing_a_loop_is_rejected() {
        let dir = tempdir().unwrap();
        let (mut vm, _) = make_vault(dir.path());
        let ids = add_chain(&mut vm, 2);

        // hop1 already goes through hop0; sending hop0 back through hop1 would
        // make both unreachable.
        let err = vm.set_entry_jump_host(ids[0], Some(ids[1])).unwrap_err();
        assert!(matches!(err, VaultError::JumpCycle(_)), "got {err:?}");
        assert_eq!(vm.get_entry(ids[0]).unwrap().jump_host, None);
    }

    #[test]
    fn the_chain_stops_at_the_depth_cap() {
        let dir = tempdir().unwrap();
        let (mut vm, _) = make_vault(dir.path());
        // MAX_JUMP_HOSTS bastions plus the host itself is the deepest legal chain.
        let ids = add_chain(&mut vm, MAX_JUMP_HOSTS + 1);
        assert_eq!(
            vm.resolve_jump_chain_for(ids[MAX_JUMP_HOSTS])
                .unwrap()
                .len(),
            MAX_JUMP_HOSTS + 1
        );

        let mut one_too_many = named_entry("too-deep");
        one_too_many.jump_host = Some(ids[MAX_JUMP_HOSTS]);
        let err = vm.add_entry(one_too_many).unwrap_err();
        assert!(matches!(err, VaultError::JumpTooDeep(_)), "got {err:?}");
    }

    /// Linking one host must not push the hosts already behind it past the
    /// cap: they would stop connecting, and the user edited a different host.
    #[test]
    fn a_link_that_would_bury_the_hosts_behind_it_is_rejected() {
        let dir = tempdir().unwrap();
        let (mut vm, _) = make_vault(dir.path());
        let ids = add_chain(&mut vm, MAX_JUMP_HOSTS + 1);
        let extra = named_entry("extra");
        let extra_id = extra.id;
        vm.add_entry(extra).unwrap();

        // The chain is already at the cap, so putting one more host in front
        // of its head buries the deepest one.
        let err = vm.set_entry_jump_host(ids[0], Some(extra_id)).unwrap_err();
        assert!(matches!(err, VaultError::JumpTooDeep(_)), "got {err:?}");
        assert_eq!(vm.get_entry(ids[0]).unwrap().jump_host, None);

        // And the picker does not offer what the save would refuse.
        let labels: Vec<String> = vm
            .list_jump_candidates(Some(ids[0]))
            .into_iter()
            .map(|c| c.label)
            .collect();
        assert!(labels.is_empty(), "got {labels:?}");
    }

    #[test]
    fn deleting_a_bastion_leaves_the_hosts_behind_it_direct() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        let (mut vm, _) =
            VaultManager::create_with_params(&path, &pw("hunter2"), fast_kdf()).unwrap();
        let ids = add_chain(&mut vm, 2);

        assert_eq!(vm.servers_using_jump_host(ids[0]), vec!["hop1".to_string()]);
        vm.delete_entry(ids[0]).unwrap();
        drop(vm);

        // The cleared link has to be on disk, not only in memory.
        let reloaded = VaultManager::unlock_with_password(&path, &pw("hunter2")).unwrap();
        assert_eq!(reloaded.get_entry(ids[1]).unwrap().jump_host, None);
        assert_eq!(reloaded.resolve_jump_chain_for(ids[1]).unwrap().len(), 1);
    }

    #[test]
    fn jump_candidates_exclude_the_host_and_anything_behind_it() {
        let dir = tempdir().unwrap();
        let (mut vm, _) = make_vault(dir.path());
        let ids = add_chain(&mut vm, 3);

        // Editing hop1: hop0 is in front of it (fine), hop2 is behind it
        // (picking it would close a loop), and it cannot pick itself.
        let labels: Vec<String> = vm
            .list_jump_candidates(Some(ids[1]))
            .into_iter()
            .map(|c| c.label)
            .collect();
        assert_eq!(labels, vec!["hop0".to_string()]);

        // Adding a new host: everything is pickable.
        assert_eq!(vm.list_jump_candidates(None).len(), 3);
    }

    #[test]
    fn jump_candidates_drop_hosts_whose_chain_is_already_at_the_cap() {
        let dir = tempdir().unwrap();
        let (mut vm, _) = make_vault(dir.path());
        add_chain(&mut vm, MAX_JUMP_HOSTS + 1);

        // The deepest host would push a new one past the cap, so the picker
        // must not offer it: the save would reject it.
        let labels: Vec<String> = vm
            .list_jump_candidates(None)
            .into_iter()
            .map(|c| c.label)
            .collect();
        assert!(
            !labels.contains(&format!("hop{MAX_JUMP_HOSTS}")),
            "got {labels:?}"
        );
        assert_eq!(labels.len(), MAX_JUMP_HOSTS);
    }

    #[test]
    fn a_batch_writes_once_and_persists_every_edit() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        let (mut vm, _) =
            VaultManager::create_with_params(&path, &pw("hunter2"), fast_kdf()).unwrap();

        let ids = vm
            .batch(|vm| {
                let first = named_entry("first");
                let second = named_entry("second");
                let (a, b) = (first.id, second.id);
                vm.add_entry(first).unwrap();
                vm.add_entry(second).unwrap();
                vm.set_entry_jump_host(b, Some(a)).unwrap();
                // Three record writes so far, and none of them reached the
                // file: that is the whole point of the batch.
                let on_disk = VaultManager::unlock_with_password(&path, &pw("hunter2")).unwrap();
                assert!(on_disk.list_entries().unwrap().is_empty());
                (a, b)
            })
            .unwrap();
        drop(vm);

        let reloaded = VaultManager::unlock_with_password(&path, &pw("hunter2")).unwrap();
        assert_eq!(reloaded.list_entries().unwrap().len(), 2);
        assert_eq!(reloaded.get_entry(ids.1).unwrap().jump_host, Some(ids.0));
    }

    #[test]
    fn port_forwards_survive_a_lock_and_unlock() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        let (mut vm, _) = VaultManager::create_with_params(&path, &pw("pw"), fast_kdf()).unwrap();

        let mut entry = test_entry();
        entry.forwards = vec![
            PortForward {
                id: Uuid::new_v4(),
                name: "db".into(),
                kind: ForwardKind::Local,
                bind_address: "127.0.0.1".into(),
                bind_port: 5432,
                target_host: "db.internal".into(),
                target_port: 5432,
                auto_start: true,
            },
            PortForward {
                id: Uuid::new_v4(),
                name: "webhook".into(),
                kind: ForwardKind::Remote,
                bind_address: "localhost".into(),
                bind_port: 9000,
                target_host: "127.0.0.1".into(),
                target_port: 3000,
                auto_start: false,
            },
            PortForward {
                id: Uuid::new_v4(),
                name: "socks".into(),
                kind: ForwardKind::Dynamic,
                bind_address: "127.0.0.1".into(),
                bind_port: 1080,
                target_host: String::new(),
                target_port: 0,
                auto_start: false,
            },
        ];
        let id = entry.id;
        vm.add_entry(entry).unwrap();
        vm.lock();

        let vm2 = VaultManager::unlock_with_password(&path, &pw("pw")).unwrap();
        let forwards = vm2.get_entry(id).unwrap().forwards;
        assert_eq!(forwards.len(), 3);
        assert_eq!(forwards[0].kind, ForwardKind::Local);
        assert_eq!(forwards[0].bind_port, 5432);
        assert_eq!(forwards[0].target_host, "db.internal");
        assert!(forwards[0].auto_start);
        assert_eq!(forwards[1].kind, ForwardKind::Remote);
        assert_eq!(forwards[1].bind_address, "localhost");
        assert!(!forwards[1].auto_start);
        assert_eq!(forwards[2].kind, ForwardKind::Dynamic);
        assert_eq!(forwards[2].bind_port, 1080);
    }

    #[test]
    fn tags_and_groups_resolve_and_persist() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        let (mut vm, _) = VaultManager::create_with_params(&path, &pw("pw"), fast_kdf()).unwrap();

        let parent = vm.find_or_create_group("infra", None).unwrap();
        let child = vm.find_or_create_group("Web", Some(parent)).unwrap();
        let tag = vm.find_or_create_tag("vpn").unwrap();
        // Lookups are case-insensitive and never duplicate.
        assert_eq!(vm.find_or_create_group("INFRA", None).unwrap(), parent);
        assert_eq!(vm.find_or_create_tag("VPN").unwrap(), tag);
        assert_eq!(vm.list_groups().len(), 2);
        assert_eq!(vm.list_tags().len(), 1);

        let mut entry = test_entry();
        entry.group = Some(child);
        entry.tags = vec![tag];
        let id = entry.id;
        vm.add_entry(entry).unwrap();
        vm.lock();

        let vm2 = VaultManager::unlock_with_password(&path, &pw("pw")).unwrap();
        let summary = vm2
            .list_entries()
            .unwrap()
            .into_iter()
            .find(|s| s.id == id)
            .unwrap();
        assert_eq!(summary.group_name.as_deref(), Some("infra / Web"));
        assert_eq!(summary.tag_names, vec!["vpn".to_string()]);
    }

    #[test]
    fn editing_and_deleting_hosts_prunes_orphaned_groups_and_tags() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        let (mut vm, _) = VaultManager::create_with_params(&path, &pw("pw"), fast_kdf()).unwrap();

        let parent = vm.find_or_create_group("infra", None).unwrap();
        let child = vm.find_or_create_group("web", Some(parent)).unwrap();
        vm.find_or_create_group("standalone", None).unwrap();
        let tag_keep = vm.find_or_create_tag("keep").unwrap();
        let tag_drop = vm.find_or_create_tag("drop").unwrap();

        let mut entry = test_entry();
        entry.group = Some(child);
        entry.tags = vec![tag_keep, tag_drop];
        let id = entry.id;
        vm.add_entry(entry).unwrap();
        // add_entry doesn't prune: the unreferenced "standalone" survives.
        assert_eq!(vm.list_groups().len(), 3);

        // Editing the host off "drop" and out of the nested folder prunes the
        // now-unused tag and "standalone", but keeps "infra" (still an
        // ancestor of the host's "web" folder).
        let mut edited = vm.get_entry(id).unwrap();
        edited.tags = vec![tag_keep];
        vm.update_entry(edited).unwrap();
        let group_names: Vec<String> = vm.list_groups().into_iter().map(|g| g.name).collect();
        assert!(group_names.contains(&"infra".to_string()));
        assert!(group_names.contains(&"web".to_string()));
        assert!(!group_names.contains(&"standalone".to_string()));
        assert_eq!(vm.list_tags().len(), 1);
        assert_eq!(vm.list_tags()[0].id, tag_keep);

        // Deleting the last host clears every group and tag.
        vm.delete_entry(id).unwrap();
        assert!(vm.list_groups().is_empty());
        assert!(vm.list_tags().is_empty());
    }

    #[test]
    fn snippet_crud_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.bin");

        let (mut vm, _) = VaultManager::create_with_params(&path, &pw("pw"), fast_kdf()).unwrap();

        let id1 = vm
            .add_snippet(
                "List files".into(),
                Some("shows cwd contents".into()),
                SnippetBody::Script {
                    content: "ls -la".into(),
                },
            )
            .unwrap();

        let id2 = vm
            .add_snippet(
                "Disk usage".into(),
                None,
                SnippetBody::Script {
                    content: "df -h".into(),
                },
            )
            .unwrap();

        let snippets = vm.list_snippets().unwrap();
        assert_eq!(snippets.len(), 2);

        let s1 = vm.get_snippet(id1).unwrap();
        assert_eq!(s1.name, "List files");
        assert_eq!(s1.description.as_deref(), Some("shows cwd contents"));
        assert!(matches!(&s1.body, SnippetBody::Script { content } if content == "ls -la"));

        let created_at = s1.created_at;

        vm.update_snippet(
            id1,
            "List files (verbose)".into(),
            None,
            SnippetBody::Script {
                content: "ls -lah".into(),
            },
        )
        .unwrap();

        let updated = vm.get_snippet(id1).unwrap();
        assert_eq!(updated.name, "List files (verbose)");
        assert_eq!(updated.description, None);
        assert!(matches!(&updated.body, SnippetBody::Script { content } if content == "ls -lah"));
        assert_eq!(updated.created_at, created_at);
        assert!(updated.updated_at >= created_at);

        vm.delete_snippet(id2).unwrap();
        assert_eq!(vm.list_snippets().unwrap().len(), 1);
        assert!(matches!(
            vm.get_snippet(id2),
            Err(VaultError::SnippetNotFound(_))
        ));

        vm.lock();

        let vm2 = VaultManager::unlock_with_password(&path, &pw("pw")).unwrap();
        let reloaded = vm2.list_snippets().unwrap();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].name, "List files (verbose)");
        assert!(
            matches!(&reloaded[0].body, SnippetBody::Script { content } if content == "ls -lah")
        );
    }

    #[test]
    fn deleting_a_startup_snippet_unlinks_the_hosts_running_it() {
        let dir = tempdir().unwrap();
        let (mut vm, _) = make_vault(dir.path());

        let snip = vm
            .add_snippet(
                "Tail syslog".into(),
                None,
                SnippetBody::Script {
                    content: "tail".into(),
                },
            )
            .unwrap();
        assert_eq!(vm.find_snippet_by_name("tail SYSLOG"), Some(snip));

        let mut entry = test_entry();
        entry.startup_snippet = Some(snip);
        let entry_id = entry.id;
        vm.add_entry(entry).unwrap();

        vm.delete_snippet(snip).unwrap();
        assert_eq!(vm.get_entry(entry_id).unwrap().startup_snippet, None);
        assert_eq!(vm.find_snippet_by_name("Tail syslog"), None);
    }

    #[test]
    fn snippet_usage_records_per_server_and_persists() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.bin");

        let (mut vm, _) = VaultManager::create_with_params(&path, &pw("pw"), fast_kdf()).unwrap();
        let snip = vm
            .add_snippet(
                "Deploy".into(),
                None,
                SnippetBody::Script {
                    content: "make".into(),
                },
            )
            .unwrap();
        let server_a = Uuid::new_v4();
        let server_b = Uuid::new_v4();

        vm.touch_snippet_used(snip, server_a).unwrap();
        vm.touch_snippet_used(snip, server_b).unwrap();
        // Re-running on server A updates its timestamp in place, not a new entry.
        vm.touch_snippet_used(snip, server_a).unwrap();

        assert_eq!(vm.get_snippet(snip).unwrap().usages.len(), 2);
        vm.lock();

        let vm2 = VaultManager::unlock_with_password(&path, &pw("pw")).unwrap();
        let usages = &vm2.get_snippet(snip).unwrap().usages;
        assert_eq!(usages.len(), 2);
        assert!(usages.iter().any(|u| u.server_id == server_a));
        assert!(usages.iter().any(|u| u.server_id == server_b));
    }

    #[test]
    fn vault_manager_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<VaultManager>();
    }

    #[test]
    fn roundtrip_create_add_unlock_read() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.bin");

        let (mut vm, _) = VaultManager::create_with_params(&path, &pw("pw1"), fast_kdf()).unwrap();
        let entry = test_entry();
        let id = entry.id;
        vm.add_entry(entry.clone()).unwrap();
        vm.lock();

        let vm2 = VaultManager::unlock_with_password(&path, &pw("pw1")).unwrap();
        let got = vm2.get_entry(id).unwrap();
        assert_eq!(got.host, entry.host);
        assert_eq!(got.username, entry.username);
        assert_eq!(vm2.list_entries().unwrap().len(), 1);
    }

    #[test]
    fn update_and_delete_entry() {
        let dir = tempdir().unwrap();
        let (mut vm, _) = make_vault(dir.path());
        let entry = test_entry();
        let id = entry.id;

        vm.add_entry(entry.clone()).unwrap();

        let mut updated = entry.clone();
        updated.host = "updated.example.com".into();
        vm.update_entry(updated).unwrap();
        assert_eq!(vm.get_entry(id).unwrap().host, "updated.example.com");

        vm.delete_entry(id).unwrap();
        assert!(matches!(
            vm.get_entry(id),
            Err(VaultError::EntryNotFound(_))
        ));
        assert_eq!(vm.list_entries().unwrap().len(), 0);
    }

    #[test]
    fn last_dirs_persist_across_reload_and_dedup() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        let (mut vm, _) = VaultManager::create_with_params(&path, &pw("pw"), fast_kdf()).unwrap();
        let entry = test_entry();
        let id = entry.id;
        vm.add_entry(entry).unwrap();
        assert_eq!(vm.get_entry(id).unwrap().last_local_dir, None);
        assert_eq!(vm.get_entry(id).unwrap().last_remote_dir, None);

        vm.set_entry_last_dirs(id, "/Users/me/work".into(), "/srv/app".into())
            .unwrap();
        vm.lock();

        let mut vm2 = VaultManager::unlock_with_password(&path, &pw("pw")).unwrap();
        assert_eq!(
            vm2.get_entry(id).unwrap().last_local_dir.as_deref(),
            Some("/Users/me/work")
        );
        assert_eq!(
            vm2.get_entry(id).unwrap().last_remote_dir.as_deref(),
            Some("/srv/app")
        );

        // Re-setting the same folders is a no-op (does not fail, keeps values).
        vm2.set_entry_last_dirs(id, "/Users/me/work".into(), "/srv/app".into())
            .unwrap();
        assert_eq!(
            vm2.get_entry(id).unwrap().last_remote_dir.as_deref(),
            Some("/srv/app")
        );
    }

    #[test]
    fn set_entry_password_replaces_only_a_stored_password() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        let (mut vm, _) = VaultManager::create_with_params(&path, &pw("pw"), fast_kdf()).unwrap();
        let mut with_password = test_entry();
        with_password.auth = AuthMethod::Password("old".to_string());
        let password_id = with_password.id;
        let mut with_agent = test_entry();
        with_agent.id = Uuid::new_v4();
        with_agent.auth = AuthMethod::Agent;
        let agent_id = with_agent.id;
        vm.add_entry(with_password).unwrap();
        vm.add_entry(with_agent).unwrap();

        vm.set_entry_password(password_id, "new").unwrap();
        vm.set_entry_password(agent_id, "new").unwrap();
        vm.lock();

        let vm2 = VaultManager::unlock_with_password(&path, &pw("pw")).unwrap();
        assert!(matches!(
            vm2.get_entry(password_id).unwrap().auth,
            AuthMethod::Password(p) if p == "new"
        ));
        assert!(matches!(
            vm2.get_entry(agent_id).unwrap().auth,
            AuthMethod::Agent
        ));
    }

    #[test]
    fn host_key_fingerprint_persists_across_reload() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        let (mut vm, _) = VaultManager::create_with_params(&path, &pw("pw"), fast_kdf()).unwrap();
        let entry = test_entry();
        let id = entry.id;
        vm.add_entry(entry).unwrap();
        assert_eq!(vm.get_entry(id).unwrap().host_key_fingerprint, None);

        vm.set_entry_host_key(id, "SHA256:abcdef".to_string())
            .unwrap();
        vm.lock();

        let vm2 = VaultManager::unlock_with_password(&path, &pw("pw")).unwrap();
        assert_eq!(
            vm2.get_entry(id).unwrap().host_key_fingerprint.as_deref(),
            Some("SHA256:abcdef")
        );
    }

    #[test]
    fn wrong_password_returns_invalid_password() {
        let dir = tempdir().unwrap();
        let (vm, _) = make_vault(dir.path());
        let path = vm.path.clone();
        vm.lock();

        let err = VaultManager::unlock_with_password(&path, &pw("wrong")).unwrap_err();
        assert!(
            matches!(err, VaultError::InvalidPassword),
            "expected InvalidPassword, got {err:?}"
        );
    }

    #[test]
    fn wrong_recovery_phrase_returns_invalid_recovery_phrase() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        VaultManager::create_with_params(&path, &pw("pw"), fast_kdf()).unwrap();

        let (wrong_phrase, _) = RecoveryPhrase::generate();
        let err = VaultManager::unlock_with_recovery_with_params(
            &path,
            &wrong_phrase,
            &pw("new"),
            fast_kdf(),
        )
        .unwrap_err();
        assert!(matches!(err, VaultError::InvalidRecoveryPhrase));
    }

    #[test]
    fn recovery_with_an_empty_password_is_rejected_and_changes_nothing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        let (vm, phrase) =
            VaultManager::create_with_params(&path, &pw("old-pw"), fast_kdf()).unwrap();
        vm.lock();
        let before = std::fs::read(&path).unwrap();

        let err =
            VaultManager::unlock_with_recovery_with_params(&path, &phrase, &pw(""), fast_kdf())
                .unwrap_err();
        assert!(matches!(err, VaultError::EmptyPassword), "got {err:?}");
        assert_eq!(std::fs::read(&path).unwrap(), before);

        // The old password and the old phrase both still work.
        VaultManager::unlock_with_password(&path, &pw("old-pw"))
            .unwrap()
            .lock();
        VaultManager::unlock_with_recovery_with_params(&path, &phrase, &pw("new-pw"), fast_kdf())
            .unwrap();
    }

    #[test]
    fn recovery_rotates_both_slots() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        let (mut vm, old_phrase) =
            VaultManager::create_with_params(&path, &pw("old-pw"), fast_kdf()).unwrap();
        vm.add_entry(test_entry()).unwrap();
        vm.lock();

        let (vm2, new_phrase) = VaultManager::unlock_with_recovery_with_params(
            &path,
            &old_phrase,
            &pw("new-pw"),
            fast_kdf(),
        )
        .unwrap();
        assert_eq!(vm2.list_entries().unwrap().len(), 1);
        vm2.lock();

        assert!(matches!(
            VaultManager::unlock_with_password(&path, &pw("old-pw")),
            Err(VaultError::InvalidPassword)
        ));

        assert!(matches!(
            VaultManager::unlock_with_recovery_with_params(
                &path,
                &old_phrase,
                &pw("any"),
                fast_kdf()
            ),
            Err(VaultError::InvalidRecoveryPhrase)
        ));

        let vm3 = VaultManager::unlock_with_password(&path, &pw("new-pw")).unwrap();
        vm3.lock();

        let (vm4, _) = VaultManager::unlock_with_recovery_with_params(
            &path,
            &new_phrase,
            &pw("newer-pw"),
            fast_kdf(),
        )
        .unwrap();
        vm4.lock();
    }

    fn change_password(vm: &mut VaultManager, current: &str, new: &str) -> Result<(), VaultError> {
        let slot = vm.password_slot().rewrap(&pw(current), &pw(new))?;
        vm.install_password_slot(slot)
    }

    #[test]
    fn change_password_old_fails_new_works_recovery_unchanged() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        let (mut vm, phrase) =
            VaultManager::create_with_params(&path, &pw("old"), fast_kdf()).unwrap();
        change_password(&mut vm, "old", "new").unwrap();
        vm.lock();

        assert!(matches!(
            VaultManager::unlock_with_password(&path, &pw("old")),
            Err(VaultError::InvalidPassword)
        ));

        let vm2 = VaultManager::unlock_with_password(&path, &pw("new")).unwrap();
        vm2.lock();

        // The recovery phrase was not rotated, so it still works.
        let (vm3, _) = VaultManager::unlock_with_recovery_with_params(
            &path,
            &phrase,
            &pw("recovered"),
            fast_kdf(),
        )
        .unwrap();
        vm3.lock();
    }

    #[test]
    fn change_password_wrong_old_returns_invalid_password() {
        let dir = tempdir().unwrap();
        let (mut vm, _) = make_vault(dir.path());
        let before = std::fs::read(dir.path().join("vault.bin")).unwrap();

        let err = change_password(&mut vm, "wrong", "new").unwrap_err();
        assert!(matches!(err, VaultError::InvalidPassword));
        assert_eq!(std::fs::read(dir.path().join("vault.bin")).unwrap(), before);
    }

    #[test]
    fn change_password_to_an_empty_one_is_rejected_and_changes_nothing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        let (mut vm, _) = make_vault(dir.path());
        let before = std::fs::read(&path).unwrap();

        let err = change_password(&mut vm, "hunter2", "").unwrap_err();
        assert!(matches!(err, VaultError::EmptyPassword), "got {err:?}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        vm.lock();

        VaultManager::unlock_with_password(&path, &pw("hunter2"))
            .unwrap()
            .lock();
    }

    #[test]
    fn change_password_keeps_the_vault_contents() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        let (mut vm, _) = make_vault(dir.path());
        let entry = test_entry();
        let entry_id = entry.id;
        vm.add_entry(entry).unwrap();

        change_password(&mut vm, "hunter2", "new").unwrap();

        // Still usable without a re-unlock, and later saves keep the new slot.
        vm.add_entry(test_entry()).unwrap();
        vm.lock();

        let reopened = VaultManager::unlock_with_password(&path, &pw("new")).unwrap();
        assert_eq!(reopened.list_entries().unwrap().len(), 2);
        assert_eq!(reopened.get_entry(entry_id).unwrap().id, entry_id);
    }

    #[test]
    fn change_password_twice_leaves_only_the_last_password_working() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        let (mut vm, _) = make_vault(dir.path());

        change_password(&mut vm, "hunter2", "second").unwrap();
        // The first password no longer passes the check in memory either.
        assert!(matches!(
            change_password(&mut vm, "hunter2", "third"),
            Err(VaultError::InvalidPassword)
        ));
        change_password(&mut vm, "second", "third").unwrap();
        vm.lock();

        for stale in ["hunter2", "second"] {
            assert!(matches!(
                VaultManager::unlock_with_password(&path, &pw(stale)),
                Err(VaultError::InvalidPassword)
            ));
        }
        VaultManager::unlock_with_password(&path, &pw("third"))
            .unwrap()
            .lock();
    }

    #[test]
    fn change_password_uses_a_fresh_salt_and_keeps_the_kdf_cost() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        let (mut vm, _) = make_vault(dir.path());
        let before = storage::read_vault(&path).unwrap();

        change_password(&mut vm, "hunter2", "new").unwrap();

        let after = storage::read_vault(&path).unwrap();
        assert_ne!(after.kdf_salt, before.kdf_salt);
        assert_ne!(after.enc_dek_by_password, before.enc_dek_by_password);
        assert_eq!(after.enc_dek_by_recovery, before.enc_dek_by_recovery);
        assert_eq!(after.kdf_params.m_cost, before.kdf_params.m_cost);
        assert_eq!(after.kdf_params.t_cost, before.kdf_params.t_cost);
        assert_eq!(after.kdf_params.p_cost, before.kdf_params.p_cost);
    }

    #[test]
    fn a_password_slot_made_from_an_older_slot_is_refused() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        let (mut vm, _) = make_vault(dir.path());

        // Two changes start from the same slot; only the first may land.
        let first = vm
            .password_slot()
            .rewrap(&pw("hunter2"), &pw("first"))
            .unwrap();
        let second = vm
            .password_slot()
            .rewrap(&pw("hunter2"), &pw("second"))
            .unwrap();
        vm.install_password_slot(first).unwrap();

        let err = vm.install_password_slot(second).unwrap_err();
        assert!(matches!(err, VaultError::StalePasswordSlot), "got {err:?}");
        vm.lock();

        VaultManager::unlock_with_password(&path, &pw("first"))
            .unwrap()
            .lock();
        assert!(matches!(
            VaultManager::unlock_with_password(&path, &pw("second")),
            Err(VaultError::InvalidPassword)
        ));
    }

    #[test]
    fn a_password_slot_survives_a_lock_and_unlock_in_between() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        let (vm, _) = make_vault(dir.path());

        let slot = vm
            .password_slot()
            .rewrap(&pw("hunter2"), &pw("new"))
            .unwrap();
        vm.lock();

        // Same file, same slot: the manager is new but the change still fits.
        let mut reopened = VaultManager::unlock_with_password(&path, &pw("hunter2")).unwrap();
        reopened.install_password_slot(slot).unwrap();
        reopened.lock();

        VaultManager::unlock_with_password(&path, &pw("new"))
            .unwrap()
            .lock();
    }

    #[test]
    fn a_failed_save_leaves_the_old_password_slot_in_memory() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        let (mut vm, _) = make_vault(dir.path());
        let slot = vm
            .password_slot()
            .rewrap(&pw("hunter2"), &pw("new"))
            .unwrap();

        // A directory in the way of the temp file makes the write fail.
        let blocker = dir.path().join("vault.bin.tmp");
        std::fs::create_dir(&blocker).unwrap();
        let err = vm.install_password_slot(slot).unwrap_err();
        assert!(matches!(err, VaultError::Io(_)), "got {err:?}");
        std::fs::remove_dir(&blocker).unwrap();

        // The next save must write the old slot, not the one that failed.
        vm.add_entry(test_entry()).unwrap();
        vm.lock();

        VaultManager::unlock_with_password(&path, &pw("hunter2"))
            .unwrap()
            .lock();
        assert!(matches!(
            VaultManager::unlock_with_password(&path, &pw("new")),
            Err(VaultError::InvalidPassword)
        ));
    }

    #[test]
    fn recovery_still_rotates_both_slots_after_a_password_change() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        let (mut vm, phrase) =
            VaultManager::create_with_params(&path, &pw("old"), fast_kdf()).unwrap();
        vm.add_entry(test_entry()).unwrap();
        change_password(&mut vm, "old", "changed").unwrap();
        vm.lock();

        let (recovered, _) = VaultManager::unlock_with_recovery_with_params(
            &path,
            &phrase,
            &pw("recovered"),
            fast_kdf(),
        )
        .unwrap();
        assert_eq!(recovered.list_entries().unwrap().len(), 1);
        recovered.lock();

        assert!(matches!(
            VaultManager::unlock_with_password(&path, &pw("changed")),
            Err(VaultError::InvalidPassword)
        ));
        VaultManager::unlock_with_password(&path, &pw("recovered"))
            .unwrap()
            .lock();
    }

    #[test]
    fn locking_and_unlocking_keeps_every_record() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        let (mut vm, _) = make_vault(dir.path());
        vm.add_entry(test_entry()).unwrap();
        let before = std::fs::read(&path).unwrap();

        vm.lock();
        // Locking is memory-only: the file is not rewritten.
        assert_eq!(std::fs::read(&path).unwrap(), before);

        for _ in 0..2 {
            let reopened = VaultManager::unlock_with_password(&path, &pw("hunter2")).unwrap();
            assert_eq!(reopened.list_entries().unwrap().len(), 1);
            reopened.lock();
        }
    }

    #[test]
    fn tampered_enc_payload_returns_corrupted() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        VaultManager::create_with_params(&path, &pw("pw"), fast_kdf()).unwrap();

        let mut vf = storage::read_vault(&path).unwrap();
        vf.enc_payload.ciphertext[0] ^= 0xff;
        let mut buf = Vec::new();
        ciborium::into_writer(&vf, &mut buf).unwrap();
        std::fs::write(&path, &buf).unwrap();

        let err = VaultManager::unlock_with_password(&path, &pw("pw")).unwrap_err();
        assert!(
            matches!(err, VaultError::Corrupted(_)),
            "expected Corrupted, got {err:?}"
        );
    }

    #[test]
    fn unsupported_version_returns_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        VaultManager::create_with_params(&path, &pw("pw"), fast_kdf()).unwrap();

        let mut vf = storage::read_vault(&path).unwrap();
        vf.version = 999;
        let mut buf = Vec::new();
        ciborium::into_writer(&vf, &mut buf).unwrap();
        std::fs::write(&path, &buf).unwrap();

        let err = VaultManager::unlock_with_password(&path, &pw("pw")).unwrap_err();
        assert!(matches!(err, VaultError::UnsupportedVersion(999)));
    }

    #[test]
    fn a_newer_format_with_another_layout_is_unsupported_not_corrupted() {
        #[derive(serde::Serialize)]
        struct FutureVaultFile {
            version: u32,
            layout: String,
        }

        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        let mut buf = Vec::new();
        ciborium::into_writer(
            &FutureVaultFile {
                version: 999,
                layout: "new".into(),
            },
            &mut buf,
        )
        .unwrap();
        std::fs::write(&path, &buf).unwrap();

        let err = VaultManager::unlock_with_password(&path, &pw("pw")).unwrap_err();
        assert!(
            matches!(err, VaultError::UnsupportedVersion(999)),
            "got {err:?}"
        );
    }

    #[test]
    fn a_vault_asking_for_an_absurd_kdf_cost_is_corrupted() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        VaultManager::create_with_params(&path, &pw("pw"), fast_kdf()).unwrap();

        let mut vf = storage::read_vault(&path).unwrap();
        vf.kdf_params.m_cost = u32::MAX;
        let mut buf = Vec::new();
        ciborium::into_writer(&vf, &mut buf).unwrap();
        std::fs::write(&path, &buf).unwrap();

        let err = VaultManager::unlock_with_password(&path, &pw("pw")).unwrap_err();
        assert!(matches!(err, VaultError::Corrupted(_)), "got {err:?}");
    }

    #[test]
    fn a_vault_cannot_be_created_with_an_empty_password() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.bin");

        let err = VaultManager::create_with_params(&path, &pw(""), fast_kdf()).unwrap_err();
        assert!(matches!(err, VaultError::EmptyPassword), "got {err:?}");
        assert!(!path.exists());
    }

    #[test]
    fn a_batch_inside_a_batch_does_not_write_early() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vault.bin");
        let (mut vm, _) = VaultManager::create_with_params(&path, &pw("pw"), fast_kdf()).unwrap();

        vm.batch(|vm| {
            vm.batch(|vm| vm.add_entry(named_entry("inner")).unwrap())
                .unwrap();
            vm.add_entry(named_entry("outer")).unwrap();
            let on_disk = VaultManager::unlock_with_password(&path, &pw("pw")).unwrap();
            assert!(on_disk.list_entries().unwrap().is_empty());
        })
        .unwrap();
        drop(vm);

        let reloaded = VaultManager::unlock_with_password(&path, &pw("pw")).unwrap();
        assert_eq!(reloaded.list_entries().unwrap().len(), 2);
    }

    #[test]
    fn stale_tmp_file_does_not_corrupt_vault() {
        let dir = tempdir().unwrap();
        let vault_path = dir.path().join("vault.bin");
        let tmp_path = dir.path().join("vault.bin.tmp");

        let (mut vm, _) =
            VaultManager::create_with_params(&vault_path, &pw("pw"), fast_kdf()).unwrap();
        vm.add_entry(test_entry()).unwrap();
        vm.lock();

        // Simulate a crash that left an incomplete .tmp file
        std::fs::write(&tmp_path, b"garbage data - simulated incomplete write").unwrap();

        let vm2 = VaultManager::unlock_with_password(&vault_path, &pw("pw")).unwrap();
        assert_eq!(vm2.list_entries().unwrap().len(), 1);
    }

    #[test]
    fn a_deleted_vault_cannot_be_unlocked() {
        let dir = tempdir().unwrap();
        let vault_path = dir.path().join("vault.bin");
        let (vm, phrase) =
            VaultManager::create_with_params(&vault_path, &pw("pw"), fast_kdf()).unwrap();
        vm.lock();

        delete_vault_files(&vault_path).unwrap();

        assert!(!vault_path.exists());
        assert!(matches!(
            VaultManager::unlock_with_password(&vault_path, &pw("pw")),
            Err(VaultError::Io(_))
        ));
        assert!(matches!(
            VaultManager::unlock_with_recovery(&vault_path, &phrase, &pw("new")),
            Err(VaultError::Io(_))
        ));
    }

    #[test]
    fn deleting_a_vault_removes_its_tmp_file_too() {
        let dir = tempdir().unwrap();
        let vault_path = dir.path().join("vault.bin");
        let tmp_path = dir.path().join("vault.bin.tmp");
        let (vm, _) = VaultManager::create_with_params(&vault_path, &pw("pw"), fast_kdf()).unwrap();
        vm.lock();
        std::fs::write(&tmp_path, b"left by a save that did not finish").unwrap();

        delete_vault_files(&vault_path).unwrap();

        assert!(!vault_path.exists());
        assert!(!tmp_path.exists());
    }

    #[test]
    fn a_vault_stays_when_its_tmp_file_cannot_be_removed() {
        let dir = tempdir().unwrap();
        let vault_path = dir.path().join("vault.bin");
        let (vm, _) = VaultManager::create_with_params(&vault_path, &pw("pw"), fast_kdf()).unwrap();
        vm.lock();
        // A directory in its place: removing it as a file fails.
        std::fs::create_dir(dir.path().join("vault.bin.tmp")).unwrap();

        assert!(delete_vault_files(&vault_path).is_err());

        assert!(VaultManager::unlock_with_password(&vault_path, &pw("pw")).is_ok());
    }

    #[test]
    fn deleting_a_vault_that_is_not_there_is_not_an_error() {
        let dir = tempdir().unwrap();
        let vault_path = dir.path().join("vault.bin");
        let tmp_path = dir.path().join("vault.bin.tmp");

        delete_vault_files(&vault_path).unwrap();

        // Only the temporary file is left, as after a crash in the first save.
        std::fs::write(&tmp_path, b"partial").unwrap();
        delete_vault_files(&vault_path).unwrap();
        assert!(!tmp_path.exists());
    }

    #[test]
    fn a_new_vault_can_be_made_where_one_was_deleted() {
        let dir = tempdir().unwrap();
        let vault_path = dir.path().join("vault.bin");
        let (mut vm, _) =
            VaultManager::create_with_params(&vault_path, &pw("old"), fast_kdf()).unwrap();
        vm.add_entry(test_entry()).unwrap();
        vm.lock();

        delete_vault_files(&vault_path).unwrap();
        let (vm, _) =
            VaultManager::create_with_params(&vault_path, &pw("new"), fast_kdf()).unwrap();

        assert!(vm.list_entries().unwrap().is_empty());
        assert!(matches!(
            VaultManager::unlock_with_password(&vault_path, &pw("old")),
            Err(VaultError::InvalidPassword)
        ));
    }

    #[test]
    fn get_missing_entry_returns_not_found() {
        let dir = tempdir().unwrap();
        let (vm, _) = make_vault(dir.path());
        assert!(matches!(
            vm.get_entry(Uuid::new_v4()),
            Err(VaultError::EntryNotFound(_))
        ));
    }

    // --- the copied ssh command ---

    fn host_entry(username: &str, host: &str, port: u16) -> ServerEntry {
        ServerEntry {
            username: username.into(),
            host: host.into(),
            port,
            ..test_entry()
        }
    }

    #[test]
    fn ssh_command_for_a_direct_host_has_no_jump_flag() {
        let db = host_entry("deploy", "db", 22);
        assert_eq!(ssh_command(&[&db]), "ssh deploy@db");
    }

    #[test]
    fn ssh_command_adds_a_non_default_port() {
        let db = host_entry("deploy", "db", 2222);
        assert_eq!(ssh_command(&[&db]), "ssh -p 2222 deploy@db");
    }

    #[test]
    fn ssh_command_lists_the_bastions_outermost_first() {
        let bastion = host_entry("ops", "bastion", 22);
        let inner = host_entry("ops", "inner", 2200);
        let db = host_entry("deploy", "db", 2222);
        assert_eq!(
            ssh_command(&[&bastion, &inner, &db]),
            "ssh -J ops@bastion,ops@inner:2200 -p 2222 deploy@db"
        );
    }

    #[test]
    fn ssh_command_brackets_an_ipv6_bastion() {
        let bastion = host_entry("ops", "2001:db8::1", 22);
        let other = host_entry("ops", "fe80::2", 2200);
        let db = host_entry("deploy", "::1", 22);
        assert_eq!(
            ssh_command(&[&bastion, &other, &db]),
            "ssh -J 'ops@[2001:db8::1],ops@[fe80::2]:2200' deploy@::1"
        );
    }

    #[test]
    fn ssh_command_quotes_what_a_shell_would_read_as_syntax() {
        let db = host_entry("dep loy", "db; rm -rf ~", 22);
        assert_eq!(ssh_command(&[&db]), r"ssh 'dep loy@db; rm -rf ~'");

        let quoted = host_entry("o'brien", "$(id)", 22);
        assert_eq!(ssh_command(&[&quoted]), r"ssh 'o'\''brien@$(id)'");

        let bastion = host_entry("ops", "`id`", 22);
        let plain = host_entry("deploy", "db", 22);
        assert_eq!(
            ssh_command(&[&bastion, &plain]),
            "ssh -J 'ops@`id`' deploy@db"
        );
    }

    #[test]
    fn ssh_command_leaves_out_an_empty_username() {
        let bastion = host_entry("", "bastion", 2200);
        let db = host_entry("", "db", 22);
        assert_eq!(ssh_command(&[&bastion, &db]), "ssh -J bastion:2200 db");
    }

    #[test]
    fn ssh_command_does_not_let_a_host_pass_as_an_option() {
        let db = host_entry("", "-oProxyCommand=id", 2222);
        assert_eq!(ssh_command(&[&db]), "ssh -p 2222 -- '-oProxyCommand=id'");
    }

    #[test]
    fn ssh_command_for_walks_the_stored_jump_chain() {
        let dir = tempfile::tempdir().unwrap();
        let (mut vm, _) = make_vault(dir.path());
        let bastion = host_entry("ops", "bastion", 2200);
        let mut db = host_entry("deploy", "db", 2222);
        db.jump_host = Some(bastion.id);
        let (bastion_id, db_id) = (bastion.id, db.id);
        vm.add_entry(bastion).unwrap();
        vm.add_entry(db).unwrap();

        assert_eq!(
            vm.ssh_command_for(db_id).unwrap(),
            "ssh -J ops@bastion:2200 -p 2222 deploy@db"
        );
        assert_eq!(
            vm.ssh_command_for(bastion_id).unwrap(),
            "ssh -p 2200 ops@bastion"
        );
        assert!(vm.ssh_command_for(Uuid::new_v4()).is_err());
    }

    #[test]
    fn ssh_command_for_a_broken_chain_falls_back_to_the_direct_command() {
        let dir = tempfile::tempdir().unwrap();
        let (mut vm, _) = make_vault(dir.path());
        let db = host_entry("deploy", "db", 22);
        let db_id = db.id;
        vm.add_entry(db).unwrap();
        vm.payload.entries[0].jump_host = Some(Uuid::new_v4());

        assert_eq!(vm.ssh_command_for(db_id).unwrap(), "ssh deploy@db");
    }

    // --- jump chains that only a broken vault can produce ---

    /// A loop that is already in the payload (a hand-edited file, an interrupted
    /// write) has to end the walk with an error instead of spinning.
    #[test]
    fn a_cycle_in_the_stored_payload_stops_the_walk() {
        let mut a = named_entry("a");
        let mut b = named_entry("b");
        a.jump_host = Some(b.id);
        b.jump_host = Some(a.id);
        let entries = vec![a, b];

        let err = resolve_jump_chain(&entries[0], &entries).unwrap_err();
        assert!(
            matches!(err, format::JumpChainError::Cycle(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn a_jump_host_that_no_longer_exists_is_reported() {
        let mut app = named_entry("app");
        app.jump_host = Some(Uuid::new_v4());
        let entries = vec![app];

        let err = resolve_jump_chain(&entries[0], &entries).unwrap_err();
        assert!(
            matches!(err, format::JumpChainError::Missing(_)),
            "got {err:?}"
        );
    }

    /// Vaults written before jump hosts existed have no such key in their CBOR
    /// map; they must still load, as a direct connection.
    #[test]
    fn an_entry_written_before_jump_hosts_still_loads() {
        let entry = test_entry();
        let mut value = ciborium::value::Value::serialized(&entry).unwrap();
        let map = value.as_map_mut().expect("entries serialize as a CBOR map");
        let before = map.len();
        map.retain(|(key, _)| key.as_text() != Some("jump_host"));
        assert_eq!(map.len(), before - 1, "jump_host is not a map key");

        let mut buf = Vec::new();
        ciborium::into_writer(&value, &mut buf).unwrap();
        let loaded: ServerEntry = ciborium::from_reader(&buf[..]).unwrap();

        assert_eq!(loaded.jump_host, None);
        assert_eq!(loaded.host, entry.host);
    }
}
