use std::{
    io::Write,
    path::{Path, PathBuf},
};

use crate::vault::{
    error::VaultError,
    format::{VaultFile, VAULT_VERSION},
};

const MAX_VAULT_SIZE: u64 = 64 * 1024 * 1024; // 64 MiB sanity limit

// Upper limits only: a damaged or planted file must not make Argon2 ask for
// terabytes of memory before the password is even checked.
const MAX_KDF_MEMORY_KIB: u32 = 1_048_576;
const MAX_KDF_PASSES: u32 = 10;
const MAX_KDF_LANES: u32 = 4;

/// Read first, so a file from a newer format is reported as such even when
/// the rest of its layout no longer matches [`VaultFile`].
#[derive(serde::Deserialize)]
struct VaultFileVersion {
    version: u32,
}

pub fn read_vault(path: &Path) -> Result<VaultFile, VaultError> {
    let meta = std::fs::metadata(path)?;
    if meta.len() > MAX_VAULT_SIZE {
        return Err(VaultError::Corrupted(format!(
            "vault file is too large ({} bytes)",
            meta.len()
        )));
    }

    let data = std::fs::read(path)?;
    let header: VaultFileVersion = ciborium::from_reader(&data[..])
        .map_err(|e| VaultError::Corrupted(format!("CBOR decode: {e}")))?;
    if header.version != VAULT_VERSION {
        return Err(VaultError::UnsupportedVersion(header.version));
    }

    let vault: VaultFile = ciborium::from_reader(&data[..])
        .map_err(|e| VaultError::Corrupted(format!("CBOR decode: {e}")))?;

    let kdf = &vault.kdf_params;
    if kdf.m_cost > MAX_KDF_MEMORY_KIB || kdf.t_cost > MAX_KDF_PASSES || kdf.p_cost > MAX_KDF_LANES
    {
        return Err(VaultError::Corrupted("KDF cost is out of range".into()));
    }

    Ok(vault)
}

pub fn write_vault(path: &Path, vault: &VaultFile) -> Result<(), VaultError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut buf = Vec::new();
    ciborium::into_writer(vault, &mut buf)
        .map_err(|e| VaultError::Corrupted(format!("CBOR encode: {e}")))?;

    #[cfg(unix)]
    atomic_write_unix(path, &buf)?;
    #[cfg(windows)]
    atomic_write_windows(path, &buf)?;
    #[cfg(not(any(unix, windows)))]
    compile_error!("unsupported platform");

    Ok(())
}

pub(super) fn tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

#[cfg(unix)]
fn atomic_write_unix(path: &Path, data: &[u8]) -> Result<(), VaultError> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;

    let tmp = tmp_path(path);

    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(data)?;
        file.flush()?;
        file.sync_all()?;
    }

    std::fs::rename(&tmp, path)?;

    // fsync the parent directory so the rename survives a crash
    if let Some(parent) = path.parent() {
        let dir = std::fs::File::open(parent)?;
        // best-effort; some file systems (e.g. FAT) don't support dir fsync
        let _ = dir.sync_all();
    }

    Ok(())
}

#[cfg(windows)]
fn atomic_write_windows(path: &Path, data: &[u8]) -> Result<(), VaultError> {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::ReplaceFileW;

    let tmp = tmp_path(path);

    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
    }

    if !path.exists() {
        // ReplaceFileW fails when the target does not exist yet
        return std::fs::rename(&tmp, path).map_err(VaultError::Io);
    }

    // ReplaceFileW preserves ACLs and attributes of the replaced file
    let target = HSTRING::from(path.as_os_str());
    let replacement = HSTRING::from(tmp.as_os_str());
    unsafe {
        ReplaceFileW(&target, &replacement, None, Default::default(), None, None)
            .map_err(|e| VaultError::Io(std::io::Error::other(e)))?;
    }

    Ok(())
}
