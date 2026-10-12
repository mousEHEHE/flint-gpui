#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct DiscoveredApp {
    pub name: String,
    pub path: PathBuf,
}

pub fn discover_apps() -> Vec<DiscoveredApp> {
    #[cfg(target_os = "macos")]
    return discover_macos();
    #[cfg(target_os = "windows")]
    return discover_windows();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    Vec::new()
}

#[cfg(target_os = "macos")]
fn discover_macos() -> Vec<DiscoveredApp> {
    let mut roots = vec![
        PathBuf::from("/Applications"),
        PathBuf::from("/System/Applications"),
    ];
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join("Applications"));
    }

    let mut apps = Vec::new();
    let mut seen = HashSet::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("app") {
                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("?")
                    .to_string();
                // deduplicate by lowercase name so /Applications and ~/Applications don't double-list
                if seen.insert(name.to_lowercase()) {
                    apps.push(DiscoveredApp { name, path });
                }
            }
        }
    }
    apps.sort_by_key(|a| a.name.to_lowercase());
    apps
}

#[cfg(target_os = "windows")]
fn discover_windows() -> Vec<DiscoveredApp> {
    // Heuristic: one level deep under standard program dirs.
    // Prefer the .exe whose stem overlaps the parent subdir name.
    // Windows packaging varies widely; this is a best-effort scan.
    let roots: Vec<PathBuf> = [
        std::env::var("ProgramFiles").ok(),
        std::env::var("ProgramFiles(x86)").ok(),
        std::env::var("LOCALAPPDATA")
            .map(|p| format!("{p}\\Programs"))
            .ok(),
    ]
    .into_iter()
    .flatten()
    .map(PathBuf::from)
    .collect();

    let mut apps = Vec::new();
    let mut seen = HashSet::new();
    for root in roots {
        let Ok(subdirs) = std::fs::read_dir(&root) else {
            continue;
        };
        for subdir in subdirs.flatten() {
            let subdir_path = subdir.path();
            if !subdir_path.is_dir() {
                continue;
            }
            let dir_stem = subdir_path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase();
            let Ok(files) = std::fs::read_dir(&subdir_path) else {
                continue;
            };
            let mut candidates: Vec<PathBuf> = files
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
                })
                .filter(|p| !lowercase_stem(p).starts_with("unins"))
                .collect();
            // The exe whose stem overlaps the folder name comes first. The
            // name breaks ties, because read_dir has no fixed order.
            candidates.sort_by_cached_key(|p| {
                let stem = lowercase_stem(p);
                let is_named_like_folder = !stem.is_empty()
                    && !dir_stem.is_empty()
                    && (dir_stem.contains(&stem) || stem.contains(&dir_stem));
                (!is_named_like_folder, stem)
            });
            if let Some(exe) = candidates.into_iter().next() {
                let name = exe
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("?")
                    .to_string();
                if seen.insert(exe.clone()) {
                    apps.push(DiscoveredApp { name, path: exe });
                }
            }
        }
    }
    apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    apps
}

#[cfg(target_os = "windows")]
fn lowercase_stem(path: &std::path::Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase()
}
