//! The hosts open in tabs, so the next start opens them again. Kept beside
//! `preferences.json`: it belongs to this device and is never synced.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct OpenTabs {
    /// In tab order.
    #[serde(default)]
    pub hosts: Vec<Uuid>,
    /// The tab in front. `hosts.len()` means the hosts list was in front.
    #[serde(default)]
    pub active: usize,
}

impl OpenTabs {
    /// A missing or broken file means no tabs: it must never stop the start.
    pub fn load() -> Self {
        match Self::path() {
            Some(path) => Self::load_from(&path),
            None => Self::default(),
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::path().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "could not find the config directory",
            )
        })?;
        self.save_to(&path)
    }

    /// The id of the host in front, if a host tab was in front.
    pub fn active_host(&self) -> Option<Uuid> {
        self.hosts.get(self.active).copied()
    }

    fn load_from(path: &Path) -> Self {
        let data = match std::fs::read(path) {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                tracing::warn!(?e, path = %path.display(), "failed to read open_tabs.json");
                return Self::default();
            }
        };
        serde_json::from_slice(&data).unwrap_or_else(|e| {
            tracing::warn!(?e, "invalid open_tabs.json, opening no tabs");
            Self::default()
        })
    }

    fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        // Written beside the file and moved over it, so a crash in the
        // middle of the write cannot leave half a file.
        let unfinished = path.with_extension("json.new");
        std::fs::write(&unfinished, json)?;
        std::fs::rename(&unfinished, path)
    }

    fn path() -> Option<PathBuf> {
        Some(dirs::config_dir()?.join("Flint").join("open_tabs.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_tabs_load_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("open_tabs.json");
        let tabs = OpenTabs {
            hosts: vec![Uuid::new_v4(), Uuid::new_v4()],
            active: 1,
        };
        tabs.save_to(&path).unwrap();
        assert_eq!(OpenTabs::load_from(&path), tabs);
        assert_eq!(tabs.active_host(), Some(tabs.hosts[1]));
    }

    #[test]
    fn a_missing_or_broken_file_opens_no_tabs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("open_tabs.json");
        assert_eq!(OpenTabs::load_from(&path), OpenTabs::default());
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(OpenTabs::load_from(&path), OpenTabs::default());
    }

    #[test]
    fn the_hosts_list_in_front_has_no_active_host() {
        let tabs = OpenTabs {
            hosts: vec![Uuid::new_v4()],
            active: 1,
        };
        assert_eq!(tabs.active_host(), None);
    }
}
