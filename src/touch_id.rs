//! Unlocking the vault with Touch ID. The vault key is kept in this Mac's
//! keychain, and only the fingers enrolled when it was saved can read it back.

use gpui::{App, Entity};
use zeroize::Zeroizing;

use crate::preferences::Preferences;
use crate::ui::VaultState;

#[derive(Debug, thiserror::Error)]
pub enum TouchIdError {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    #[error("Touch ID was cancelled")]
    Cancelled,
    /// The enrolled fingers or the vault key changed since the key was saved.
    #[error("the saved key no longer opens the vault")]
    Stale,
    #[error("{0}")]
    Failed(String),
}

pub use platform::{delete_key, is_available, read_key, save_key};

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
        // Delete first: an update would keep the old item's access settings.
        delete_key();
        let access = SecAccessControl::create_with_protection(
            Some(ProtectionMode::AccessibleWhenUnlockedThisDeviceOnly),
            AccessControlOptions::BIOMETRY_CURRENT_SET.bits(),
        )
        .map_err(|e| TouchIdError::Failed(e.to_string()))?;
        let mut options = PasswordOptions::new_generic_password(SERVICE, ACCOUNT);
        options.use_protected_keychain();
        options.set_access_control(access);
        set_generic_password_options(key, options).map_err(|e| TouchIdError::Failed(e.to_string()))
    }

    /// Blocks while the Touch ID dialog is up, so never call it on the main thread.
    pub fn read_key() -> Result<Zeroizing<Vec<u8>>, TouchIdError> {
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

    pub fn delete_key() {
        let mut options = PasswordOptions::new_generic_password(SERVICE, ACCOUNT);
        options.use_protected_keychain();
        match delete_generic_password_options(options) {
            Err(e) if e.code() != ERR_ITEM_NOT_FOUND => {
                tracing::warn!(?e, "failed to delete the Touch ID key");
            }
            _ => {}
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use zeroize::Zeroizing;

    use super::TouchIdError;

    pub fn is_available() -> bool {
        false
    }

    pub fn save_key(_key: &[u8; 32]) -> Result<(), TouchIdError> {
        Err(TouchIdError::Failed("Touch ID needs a Mac".to_string()))
    }

    pub fn read_key() -> Result<Zeroizing<Vec<u8>>, TouchIdError> {
        Err(TouchIdError::Failed("Touch ID needs a Mac".to_string()))
    }

    pub fn delete_key() {}
}

/// Save the open vault's key again: after an unlock with the password, which
/// is the way back once a new finger has dropped the saved copy, and after a
/// sign-in that changed the key. A failed save turns Touch ID off, so the
/// setting never promises a key that is not there.
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
        tracing::warn!(?e, "failed to save the Touch ID key, turning Touch ID off");
        cx.update(|cx| Preferences::update(cx, |prefs| prefs.is_touch_id_enabled = false));
    })
    .detach();
}

/// Read the key with Touch ID and open the vault with it. Blocks like `read_key`.
pub fn open_vault() -> Result<crate::vault::VaultManager, TouchIdError> {
    let key = read_key()?;
    let key =
        Zeroizing::new(<[u8; 32]>::try_from(key.as_slice()).map_err(|_| TouchIdError::Stale)?);
    let path =
        crate::vault::default_vault_path().map_err(|e| TouchIdError::Failed(e.to_string()))?;
    crate::vault::VaultManager::unlock_with_device_key(&path, &key).map_err(|e| match e {
        crate::vault::VaultError::InvalidPassword => TouchIdError::Stale,
        e => TouchIdError::Failed(e.to_string()),
    })
}
