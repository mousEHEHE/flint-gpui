use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use gpui::{Context, Task};
use notify::{EventKind, RecursiveMode, Watcher as _};

use crate::ui::sftp_browser::SftpBrowser;

#[derive(Clone, Debug)]
pub enum SyncState {
    Idle,
    Uploading,
    /// The server's version is being fetched over the local copy.
    Downloading,
    Synced {
        at: SystemTime,
    },
    Conflict,
    Error,
}

pub struct FileEditSession {
    pub remote: String,
    /// Remote mtime captured at download time. Compared against current remote mtime
    /// before each upload to detect server-side edits (conflicts).
    /// Updated after every successful upload or reload.
    pub original_remote_mtime: Option<u32>,
    /// Local mtime at last successful upload. Used during disconnect to decide
    /// whether to preserve (has unsaved changes) or delete (fully synced) the cache file.
    pub last_synced_local_mtime: Option<SystemTime>,
    pub sync_state: SyncState,
    /// Set when on_file_changed fires while an upload is already in progress.
    /// Cleared and re-triggers after the in-flight upload completes.
    pub pending_resync: bool,
    _watcher: notify::RecommendedWatcher,
    _bridge_task: Task<()>,
}

impl FileEditSession {
    pub fn new(
        remote: String,
        local: PathBuf,
        original_remote_mtime: Option<u32>,
        cx: &mut Context<SftpBrowser>,
    ) -> notify::Result<Self> {
        let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();
        let mut watcher = notify::recommended_watcher(tx)?;

        let parent = local.parent().unwrap_or(std::path::Path::new("."));
        watcher.watch(parent, RecursiveMode::NonRecursive)?;

        let basename = local.file_name().map(OsStr::to_owned).unwrap_or_default();

        let bridge_task = cx.spawn(async move |this, cx| {
            let mut last_change: Option<Instant> = None;

            loop {
                while let Ok(result) = rx.try_recv() {
                    let Ok(event) = result else { continue };
                    if event_matches_basename(&event, &basename) && is_real_change(&event) {
                        tracing::debug!(
                            target: "flint::file_edit",
                            kind = ?event.kind,
                            paths = ?event.paths,
                            basename = %basename.to_string_lossy(),
                            "watcher event matched",
                        );
                        last_change = Some(Instant::now());
                    }
                }

                if let Some(t) = last_change {
                    if t.elapsed() >= Duration::from_millis(500) {
                        this.update(cx, |browser, cx| {
                            browser.on_file_changed(local.clone(), cx);
                        })
                        .ok();
                        last_change = None;
                    }
                }

                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
            }
        });

        Ok(Self {
            remote,
            original_remote_mtime,
            last_synced_local_mtime: None,
            sync_state: SyncState::Idle,
            pending_resync: false,
            _watcher: watcher,
            _bridge_task: bridge_task,
        })
    }
}

fn event_matches_basename(event: &notify::Event, basename: &OsStr) -> bool {
    event.paths.iter().any(|p| p.file_name() == Some(basename))
}

fn is_real_change(event: &notify::Event) -> bool {
    use notify::event::ModifyKind;
    match event.kind {
        EventKind::Create(_) => true,
        EventKind::Modify(ModifyKind::Data(_)) => true,
        EventKind::Modify(ModifyKind::Name(_)) => true,
        EventKind::Modify(ModifyKind::Other) => true,
        EventKind::Modify(ModifyKind::Any) => true,
        EventKind::Any => true,
        EventKind::Modify(ModifyKind::Metadata(_)) => {
            tracing::debug!(
                target: "flint::file_edit",
                kind = ?event.kind,
                "watcher event ignored",
            );
            false
        }
        EventKind::Access(_) | EventKind::Remove(_) | EventKind::Other => false,
    }
}
