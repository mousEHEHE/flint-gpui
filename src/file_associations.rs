use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
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
        // Written beside the file and moved over it, so a crash in the
        // middle of the write cannot leave half a file.
        let unfinished = path.with_extension("json.new");
        std::fs::write(&unfinished, json)?;
        std::fs::rename(&unfinished, &path)
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
