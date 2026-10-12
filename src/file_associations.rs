use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;

#[derive(Default, Serialize, Deserialize)]
pub struct FileAssociations {
    /// Keyed by `file_open::association_key`: the lowercase extension without
    /// the dot, or the lowercase file name when there is no extension.
    associations: HashMap<String, PathBuf>,
}

pub type SharedFileAssociations = Rc<RefCell<FileAssociations>>;

impl FileAssociations {
    pub fn load_or_default() -> Self {
        let Some(path) = Self::path() else {
            tracing::warn!(target: "flint::file_associations", "no config directory, using empty associations");
            return Self::default();
        };
        tracing::debug!(target: "flint::file_associations", path = %path.display(), "loading file associations");
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(target: "flint::file_associations", path = %path.display(), "file not found, using empty associations");
                return Self::default();
            }
            Err(e) => {
                tracing::warn!(target: "flint::file_associations", ?e, "failed to read file_associations.json");
                return Self::default();
            }
        };
        match serde_json::from_slice::<Self>(&data) {
            Ok(fa) => {
                tracing::debug!(target: "flint::file_associations", count = fa.associations.len(), "loaded file associations");
                fa
            }
            Err(e) => {
                tracing::warn!(target: "flint::file_associations", ?e, "invalid file_associations.json, using defaults");
                Self::default()
            }
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::path().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "could not find the config directory",
            )
        })?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;

        #[cfg(unix)]
        atomic_write_unix(&path, &json)?;
        #[cfg(windows)]
        atomic_write_windows(&path, &json)?;
        #[cfg(not(any(unix, windows)))]
        compile_error!("unsupported platform");

        Ok(())
    }

    pub fn get(&self, extension: &str) -> Option<&Path> {
        self.associations
            .get(&extension.to_lowercase())
            .map(|p| p.as_path())
    }

    pub fn set(&mut self, extension: &str, app: PathBuf) {
        self.associations.insert(extension.to_lowercase(), app);
    }

    /// `None` when the OS gives no config directory, as in some sandboxes.
    fn path() -> Option<PathBuf> {
        Some(
            dirs::config_dir()?
                .join("Flint")
                .join("file_associations.json"),
        )
    }
}

#[cfg(unix)]
fn atomic_write_unix(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;

    let tmp = tmp_path(path);

    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o644)
            .open(&tmp)?;
        file.write_all(data)?;
        file.flush()?;
        file.sync_all()?;
    }

    std::fs::rename(&tmp, path)?;

    if let Some(parent) = path.parent() {
        let dir = std::fs::File::open(parent)?;
        let _ = dir.sync_all();
    }

    Ok(())
}

#[cfg(windows)]
fn atomic_write_windows(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::ReplaceFileW;

    let tmp = tmp_path(path);

    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
    }

    if !path.exists() {
        return std::fs::rename(&tmp, path);
    }

    let target = HSTRING::from(path.as_os_str());
    let replacement = HSTRING::from(tmp.as_os_str());
    unsafe {
        ReplaceFileW(&target, &replacement, None, Default::default(), None, None)
            .map_err(std::io::Error::other)?;
    }

    Ok(())
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}
