//! Unlocking the vault without the password: Touch ID on macOS, Windows Hello
//! on Windows, and a remembered unlock that expires. The keys never leave
//! this device: the macOS keychain or the Windows Credential Manager.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use gpui::{App, Entity, Task, Window};
use zeroize::Zeroizing;

use crate::preferences::{Preferences, RememberUnlock};
use crate::ui::VaultState;
use crate::vault::{VaultError, VaultManager};

#[cfg(target_os = "macos")]
pub const METHOD_NAME: &str = "Touch ID";
#[cfg(not(target_os = "macos"))]
pub const METHOD_NAME: &str = "Windows Hello";

#[derive(Debug, thiserror::Error)]
pub enum TouchIdError {
    #[error("{} was cancelled", METHOD_NAME)]
    Cancelled,
    /// The enrolled fingers or the vault key changed since the key was saved.
    #[error("the saved key no longer opens the vault")]
    Stale,
    #[error("{0}")]
    Failed(String),
}

pub use platform::{delete_key, is_available, save_key};

#[cfg(windows)]
use platform::check_availability;
#[cfg(not(windows))]
use platform::is_available as check_availability;

/// Asks the system on a plain thread; Windows can take seconds to answer.
/// `is_available` gives the same answer once this one has arrived.
pub fn availability() -> async_channel::Receiver<bool> {
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.try_send(check_availability());
    });
    rx
}

/// The window the Windows Hello dialog belongs to. Unused on macOS.
#[cfg(windows)]
pub fn parent_window(window: &Window) -> isize {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match HasWindowHandle::window_handle(window).map(|handle| handle.as_raw()) {
        Ok(RawWindowHandle::Win32(handle)) => handle.hwnd.get(),
        _ => 0,
    }
}

#[cfg(not(windows))]
pub fn parent_window(_window: &Window) -> isize {
    0
}

#[cfg(target_os = "macos")]
mod platform {
    use core_foundation::base::{CFType, TCFType};
    use objc2::rc::Retained;
    use objc2_foundation::NSString;
    use objc2_local_authentication::{LAContext, LAPolicy};
    use security_framework::access_control::{ProtectionMode, SecAccessControl};
    use security_framework::item::{ItemClass, ItemSearchOptions, SearchResult};
    use security_framework::passwords::{
        delete_generic_password_options, set_generic_password_options, AccessControlOptions,
        PasswordOptions,
    };
    use zeroize::Zeroizing;

    use super::TouchIdError;

    const SERVICE: &str = "flint vault";
    const ACCOUNT: &str = "touch id key";
    const REMEMBERED_ACCOUNT: &str = "remembered key";

    const ERR_USER_CANCELED: i32 = -128;
    const ERR_AUTH_FAILED: i32 = -25293;
    const ERR_ITEM_NOT_FOUND: i32 = -25300;

    /// False on a Mac without a sensor, and with the lid closed.
    pub fn is_available() -> bool {
        let context = unsafe { LAContext::new() };
        unsafe {
            context.canEvaluatePolicy_error(LAPolicy::DeviceOwnerAuthenticationWithBiometrics)
        }
        .is_ok()
    }

    pub fn save_key(key: &[u8; 32]) -> Result<(), TouchIdError> {
        save_item(ACCOUNT, key, AccessControlOptions::BIOMETRY_CURRENT_SET)
    }

    pub fn save_remembered(blob: &[u8]) -> Result<(), TouchIdError> {
        save_item(REMEMBERED_ACCOUNT, blob, AccessControlOptions::empty())
    }

    fn save_item(
        account: &str,
        data: &[u8],
        flags: AccessControlOptions,
    ) -> Result<(), TouchIdError> {
        // Delete first: an update would keep the old item's access settings.
        delete_item(account);
        let access = SecAccessControl::create_with_protection(
            Some(ProtectionMode::AccessibleWhenUnlockedThisDeviceOnly),
            flags.bits(),
        )
        .map_err(|e| TouchIdError::Failed(e.to_string()))?;
        let mut options = PasswordOptions::new_generic_password(SERVICE, account);
        options.use_protected_keychain();
        options.set_access_control(access);
        set_generic_password_options(data, options).map_err(|e| TouchIdError::Failed(e.to_string()))
    }

    /// Blocks while the Touch ID dialog is up, so never call it on the main thread.
    pub fn read_key(_parent: isize) -> Result<Zeroizing<Vec<u8>>, TouchIdError> {
        let context = unsafe { LAContext::new() };
        unsafe { context.setLocalizedReason(&NSString::from_str("unlock the vault")) };
        // An Objective-C object is also a CF type, which is what the search takes.
        let context = unsafe { CFType::wrap_under_get_rule(Retained::as_ptr(&context).cast()) };
        let found = ItemSearchOptions::new()
            .class(ItemClass::generic_password())
            .service(SERVICE)
            .account(ACCOUNT)
            .ignore_legacy_keychains()
            .load_data(true)
            .local_authentication_context(Some(context))
            .search()
            .map_err(|e| match e.code() {
                ERR_USER_CANCELED => TouchIdError::Cancelled,
                ERR_ITEM_NOT_FOUND => TouchIdError::Stale,
                ERR_AUTH_FAILED => TouchIdError::Failed("Touch ID did not match".to_string()),
                _ => TouchIdError::Failed(e.to_string()),
            })?;
        match found.into_iter().next() {
            Some(SearchResult::Data(data)) => Ok(Zeroizing::new(data)),
            _ => Err(TouchIdError::Stale),
        }
    }

    pub fn read_remembered() -> Option<Zeroizing<Vec<u8>>> {
        let found = ItemSearchOptions::new()
            .class(ItemClass::generic_password())
            .service(SERVICE)
            .account(REMEMBERED_ACCOUNT)
            .ignore_legacy_keychains()
            .load_data(true)
            .search();
        match found {
            Ok(found) => match found.into_iter().next() {
                Some(SearchResult::Data(data)) => Some(Zeroizing::new(data)),
                _ => None,
            },
            Err(e) if e.code() == ERR_ITEM_NOT_FOUND => None,
            Err(e) => {
                tracing::warn!(?e, "failed to read the remembered key");
                None
            }
        }
    }

    pub fn delete_key() {
        delete_item(ACCOUNT);
    }

    pub fn delete_remembered() {
        delete_item(REMEMBERED_ACCOUNT);
    }

    fn delete_item(account: &str) {
        let mut options = PasswordOptions::new_generic_password(SERVICE, account);
        options.use_protected_keychain();
        match delete_generic_password_options(options) {
            Err(e) if e.code() != ERR_ITEM_NOT_FOUND => {
                tracing::warn!(
                    ?e,
                    account,
                    "failed to delete a vault key from the keychain"
                );
            }
            _ => {}
        }
    }
}

#[cfg(windows)]
mod platform {
    use windows::core::{factory, HSTRING, PCWSTR, PWSTR};
    use windows::Foundation::IAsyncOperation;
    use windows::Security::Credentials::UI::{UserConsentVerificationResult, UserConsentVerifier};
    use windows::Win32::Foundation::{ERROR_NOT_FOUND, HWND};
    use windows::Win32::Security::Credentials::{
        CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE,
        CRED_TYPE_GENERIC,
    };
    use windows::Win32::System::WinRT::IUserConsentVerifierInterop;
    use zeroize::Zeroizing;

    use super::TouchIdError;

    const KEY_TARGET: &str = "flint vault/windows hello key";
    const REMEMBERED_TARGET: &str = "flint vault/remembered key";

    // shortcut: Windows Hello stays off. The key below sits in Credential Manager,
    // where any program of the user reads it without Hello. Turn it on once the key
    // is wrapped by a Hello key (KeyCredentialManager), so Hello is needed to get it.
    pub fn is_available() -> bool {
        false
    }

    pub fn check_availability() -> bool {
        false
    }

    pub fn save_key(key: &[u8; 32]) -> Result<(), TouchIdError> {
        write_credential(KEY_TARGET, key)
    }

    pub fn save_remembered(blob: &[u8]) -> Result<(), TouchIdError> {
        write_credential(REMEMBERED_TARGET, blob)
    }

    /// Blocks while the Windows Hello dialog is up, so never call it on the main thread.
    pub fn read_key(parent: isize) -> Result<Zeroizing<Vec<u8>>, TouchIdError> {
        let key = read_credential(KEY_TARGET)?.ok_or(TouchIdError::Stale)?;
        verify_user(parent)?;
        Ok(key)
    }

    pub fn read_remembered() -> Option<Zeroizing<Vec<u8>>> {
        read_credential(REMEMBERED_TARGET)
            .inspect_err(|e| tracing::warn!(?e, "failed to read the remembered key"))
            .ok()
            .flatten()
    }

    pub fn delete_key() {
        delete_credential(KEY_TARGET);
    }

    pub fn delete_remembered() {
        delete_credential(REMEMBERED_TARGET);
    }

    fn verify_user(parent: isize) -> Result<(), TouchIdError> {
        let failed = |e: windows::core::Error| TouchIdError::Failed(e.to_string());
        let interop =
            factory::<UserConsentVerifier, IUserConsentVerifierInterop>().map_err(failed)?;
        let message = HSTRING::from("Unlock the flint vault");
        // Tied to our window, or the dialog can open behind it.
        let operation: IAsyncOperation<UserConsentVerificationResult> =
            unsafe { interop.RequestVerificationForWindowAsync(HWND(parent), &message) }
                .map_err(failed)?;
        match operation.get().map_err(failed)? {
            UserConsentVerificationResult::Verified => Ok(()),
            UserConsentVerificationResult::Canceled => Err(TouchIdError::Cancelled),
            other => Err(TouchIdError::Failed(format!(
                "Windows Hello did not verify you ({other:?})"
            ))),
        }
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn write_credential(target: &str, blob: &[u8]) -> Result<(), TouchIdError> {
        let mut target = wide(target);
        let credential = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(target.as_mut_ptr()),
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_ptr().cast_mut(),
            // Stays on this PC: it never roams with the user's profile.
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            ..Default::default()
        };
        unsafe { CredWriteW(&credential, 0) }.map_err(|e| TouchIdError::Failed(e.to_string()))
    }

    fn read_credential(target: &str) -> Result<Option<Zeroizing<Vec<u8>>>, TouchIdError> {
        let target = wide(target);
        let mut credential: *mut CREDENTIALW = std::ptr::null_mut();
        let read = unsafe {
            CredReadW(
                PCWSTR(target.as_ptr()),
                CRED_TYPE_GENERIC,
                0,
                &mut credential,
            )
        };
        match read {
            Err(e) if e.code() == ERROR_NOT_FOUND.to_hresult() => return Ok(None),
            Err(e) => return Err(TouchIdError::Failed(e.to_string())),
            Ok(()) => {}
        }
        unsafe {
            let size = (*credential).CredentialBlobSize as usize;
            let blob = (*credential).CredentialBlob;
            // An empty blob has a null pointer, which from_raw_parts must not get.
            let bytes = if blob.is_null() || size == 0 {
                Zeroizing::new(Vec::new())
            } else {
                let bytes = Zeroizing::new(std::slice::from_raw_parts(blob, size).to_vec());
                // CredFree does not wipe: the key would stay in freed memory.
                std::ptr::write_bytes(blob, 0, size);
                bytes
            };
            CredFree(credential as *const _);
            Ok(Some(bytes))
        }
    }

    fn delete_credential(target: &str) {
        let target = wide(target);
        match unsafe { CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, 0) } {
            Err(e) if e.code() != ERROR_NOT_FOUND.to_hresult() => {
                tracing::warn!(?e, "failed to delete a vault key from Credential Manager");
            }
            _ => {}
        }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use zeroize::Zeroizing;

    use super::TouchIdError;

    pub fn is_available() -> bool {
        false
    }

    pub fn save_key(_key: &[u8; 32]) -> Result<(), TouchIdError> {
        Err(TouchIdError::Failed(
            "not supported on this system".to_string(),
        ))
    }

    pub fn save_remembered(_blob: &[u8]) -> Result<(), TouchIdError> {
        Err(TouchIdError::Failed(
            "not supported on this system".to_string(),
        ))
    }

    pub fn read_key(_parent: isize) -> Result<Zeroizing<Vec<u8>>, TouchIdError> {
        Err(TouchIdError::Failed(
            "not supported on this system".to_string(),
        ))
    }

    pub fn read_remembered() -> Option<Zeroizing<Vec<u8>>> {
        None
    }

    pub fn delete_key() {}

    pub fn delete_remembered() {}
}

/// Save the open vault's key and turn the setting on once it is saved.
pub fn enable(vault_state: &Entity<VaultState>, cx: &mut App) -> Task<Result<(), TouchIdError>> {
    let Some(key) = vault_state.read(cx).manager().map(|vm| vm.device_key()) else {
        return Task::ready(Err(TouchIdError::Failed("the vault is locked".to_string())));
    };
    let saving = cx
        .background_executor()
        .spawn(async move { save_key(&key) });
    cx.spawn(async move |cx| {
        saving.await?;
        cx.update(|cx| Preferences::update(cx, |prefs| prefs.is_touch_id_enabled = true));
        tracing::info!("{METHOD_NAME} unlock turned on");
        Ok(())
    })
}

/// Save the open vault's key again: after an unlock with the password, which
/// is the way back once a new finger has dropped the saved copy, and after a
/// sign-in that changed the key. A failed save turns the setting off, so it
/// never promises a key that is not there.
pub fn save_key_again(vault_state: &Entity<VaultState>, cx: &mut App) {
    if !Preferences::is_touch_id_enabled(cx) || !is_available() {
        return;
    }
    let Some(key) = vault_state.read(cx).manager().map(|vm| vm.device_key()) else {
        return;
    };
    let saving = cx
        .background_executor()
        .spawn(async move { save_key(&key) });
    cx.spawn(async move |cx| {
        let Err(e) = saving.await else {
            return;
        };
        tracing::warn!(?e, "failed to save the {METHOD_NAME} key, turning it off");
        cx.update(|cx| Preferences::update(cx, |prefs| prefs.is_touch_id_enabled = false));
    })
    .detach();
}

/// Read the key with Touch ID or Windows Hello and open the vault with it.
/// Blocks like `read_key`.
pub fn open_vault(parent: isize) -> Result<VaultManager, TouchIdError> {
    let key = platform::read_key(parent)?;
    let key =
        Zeroizing::new(<[u8; 32]>::try_from(key.as_slice()).map_err(|_| TouchIdError::Stale)?);
    let path =
        crate::vault::default_vault_path().map_err(|e| TouchIdError::Failed(e.to_string()))?;
    VaultManager::unlock_with_device_key(&path, &key).map_err(|e| match e {
        VaultError::InvalidPassword => TouchIdError::Stale,
        e => TouchIdError::Failed(e.to_string()),
    })
}

const REMEMBERED_LEN: usize = 40;

/// Bumped by every forget, so a save that started before it gives up.
static REMEMBER_GENERATION: AtomicU64 = AtomicU64::new(0);
/// Keeps a save and a forget from crossing each other.
static REMEMBER_LOCK: Mutex<()> = Mutex::new(());

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// The key followed by the expiry. The expiry lives inside the secret, so
/// nothing that can only write files can extend it.
fn encode_remembered(key: &[u8; 32], expires_at: u64) -> Zeroizing<[u8; REMEMBERED_LEN]> {
    let mut blob = Zeroizing::new([0u8; REMEMBERED_LEN]);
    blob[..32].copy_from_slice(key);
    blob[32..].copy_from_slice(&expires_at.to_le_bytes());
    blob
}

/// The key while `now` is before the expiry. An expiry further away than the
/// longest choice means the clock was set back, so that is refused too.
fn decode_remembered(blob: &[u8], now: u64) -> Option<Zeroizing<[u8; 32]>> {
    if blob.len() != REMEMBERED_LEN {
        return None;
    }
    let expires_at = u64::from_le_bytes(blob[32..].try_into().ok()?);
    let longest = RememberUnlock::Week.duration()?.as_secs();
    if now >= expires_at || expires_at - now > longest {
        return None;
    }
    let mut key = Zeroizing::new([0u8; 32]);
    key.copy_from_slice(&blob[..32]);
    Some(key)
}

/// Keep the open vault's key for the time the user chose. Only after an
/// unlock that asked for the password or the finger: never after a remembered
/// one, or the expiry would move forward forever.
pub fn remember_unlock(vault_state: &Entity<VaultState>, cx: &mut App) {
    let Some(duration) = Preferences::remember_unlock(cx).duration() else {
        return;
    };
    let Some(key) = vault_state.read(cx).manager().map(|vm| vm.device_key()) else {
        return;
    };
    let generation = REMEMBER_GENERATION.load(Ordering::SeqCst);
    let blob = encode_remembered(&key, unix_now() + duration.as_secs());
    cx.background_executor()
        .spawn(async move {
            let _guard = REMEMBER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            if REMEMBER_GENERATION.load(Ordering::SeqCst) != generation {
                return;
            }
            if let Err(e) = platform::save_remembered(blob.as_slice()) {
                tracing::warn!(?e, "failed to remember the unlock");
            }
        })
        .detach();
}

/// Lock, vault deletion, a new password or phrase, and the setting set to
/// Never all end a remembered unlock.
pub fn forget_remembered() {
    let _guard = REMEMBER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    REMEMBER_GENERATION.fetch_add(1, Ordering::SeqCst);
    platform::delete_remembered();
}

/// Open the vault with a remembered key that has not expired. Blocks on the
/// OS store, so never call it on the main thread.
pub fn open_remembered_vault() -> Option<VaultManager> {
    let blob = platform::read_remembered()?;
    let Some(key) = decode_remembered(&blob, unix_now()) else {
        tracing::info!("the remembered unlock expired");
        forget_remembered();
        return None;
    };
    let path = crate::vault::default_vault_path().ok()?;
    match VaultManager::unlock_with_device_key(&path, &key) {
        Ok(manager) => Some(manager),
        Err(e) => {
            tracing::warn!(?e, "the remembered key did not open the vault");
            if matches!(e, VaultError::InvalidPassword) {
                forget_remembered();
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 24 * 60 * 60;

    #[test]
    fn a_remembered_key_reads_back_before_it_expires() {
        let blob = encode_remembered(&[7; 32], 1_000 + DAY);
        assert_eq!(*decode_remembered(blob.as_slice(), 1_000).unwrap(), [7; 32]);
    }

    #[test]
    fn a_remembered_key_is_refused_from_its_expiry_on() {
        let blob = encode_remembered(&[7; 32], 1_000 + DAY);
        assert!(decode_remembered(blob.as_slice(), 1_000 + DAY).is_none());
        assert!(decode_remembered(blob.as_slice(), 2_000 + DAY).is_none());
    }

    #[test]
    fn an_expiry_beyond_the_longest_choice_is_refused() {
        let blob = encode_remembered(&[7; 32], 1_000 + 7 * DAY + 1);
        assert!(decode_remembered(blob.as_slice(), 1_000).is_none());
        let blob = encode_remembered(&[7; 32], 1_000 + 7 * DAY);
        assert!(decode_remembered(blob.as_slice(), 1_000).is_some());
    }

    #[test]
    fn a_blob_of_the_wrong_length_is_refused() {
        let blob = encode_remembered(&[7; 32], 1_000 + DAY);
        assert!(decode_remembered(&blob[..32], 1_000).is_none());
        assert!(decode_remembered(&[], 1_000).is_none());
    }
}
