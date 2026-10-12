//! Knows whether a newer build is out, and downloads it when asked. Lives for
//! the whole run of the app, so the title bar and Settings show one state.
//!
//! The requests themselves are in `crate::update`. This file decides when
//! they are made: never on an OS without builds, and never while the
//! "Check for updates" preference is off.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use gpui::{App, AppContext as _, Context, Entity, Global};

use crate::account::api::base_url;
use crate::preferences::Preferences;
use crate::update::{self, Release, UpdateError};
use crate::version;

/// Long enough that the check does not compete with the window coming up.
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(10);
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// How often the service looks whether a check is due. Also the wait before
/// a failed check is tried again.
const DUE_LOOK_INTERVAL: Duration = Duration::from_secs(15 * 60);

#[derive(Clone)]
pub enum Download {
    Idle,
    Running { percent: u8 },
    Done(PathBuf),
    Failed(String),
}

pub struct UpdateService {
    pub is_checking: bool,
    /// How the last check ended, and when if it went well. `None` before
    /// the first one.
    last_check: Option<Result<SystemTime, String>>,
    /// The build the server offers, when it is newer than this app.
    pub release: Option<Release>,
    pub download: Download,
    is_cancelled: Arc<AtomicBool>,
    /// Counts cancels and resets, so an answer to work nobody waits for any
    /// more is ignored.
    generation: u64,
}

struct GlobalUpdateService(Entity<UpdateService>);

impl Global for GlobalUpdateService {}

enum DownloadEvent {
    Progress(u8),
    /// The build that was saved, which is the server's current build, and
    /// where it is.
    Finished(Result<(Release, PathBuf), UpdateError>),
}

impl UpdateService {
    pub fn install(cx: &mut App) {
        let service = cx.new(Self::new);
        cx.set_global(GlobalUpdateService(service));
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalUpdateService>().0.clone()
    }

    fn new(cx: &mut Context<Self>) -> Self {
        cx.spawn(async move |this, cx| {
            let mut delay = FIRST_CHECK_DELAY;
            loop {
                cx.background_executor().timer(delay).await;
                if this.update(cx, |this, cx| this.check_if_due(cx)).is_err() {
                    break;
                }
                // The timer stands still while the machine sleeps, so it
                // cannot count the six hours. The clock can.
                delay = DUE_LOOK_INTERVAL;
            }
        })
        .detach();

        Self {
            is_checking: false,
            last_check: None,
            release: None,
            download: Download::Idle,
            is_cancelled: Arc::new(AtomicBool::new(false)),
            generation: 0,
        }
    }

    fn check_if_due(&mut self, cx: &mut Context<Self>) {
        if is_check_due(&self.last_check, SystemTime::now()) {
            self.check(cx);
        }
    }

    /// Ask the server which build is current. Does nothing when checks are
    /// off or this OS has no builds.
    pub fn check(&mut self, cx: &mut Context<Self>) {
        let Some(platform) = update::platform() else {
            return;
        };
        if self.is_checking || !Preferences::is_update_check_enabled(cx) {
            return;
        }
        self.is_checking = true;
        cx.notify();

        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.try_send(update::newer_release(
                &base_url(),
                platform,
                version::CURRENT,
            ));
        });
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let Ok(answer) = rx.recv().await else { return };
            this.update(cx, |this, cx| {
                if this.generation == generation {
                    this.finish_check(answer, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn finish_check(
        &mut self,
        answer: Result<Option<Release>, UpdateError>,
        cx: &mut Context<Self>,
    ) {
        self.is_checking = false;
        match answer {
            Ok(offered) => {
                self.last_check = Some(Ok(SystemTime::now()));
                self.take_offer(offered);
            }
            // Shown on the About page only. A laptop that is offline must
            // not be told about it every time.
            Err(e) => {
                tracing::debug!(error = %e, "update check failed");
                self.last_check = Some(Err(e.to_string()));
            }
        }
        cx.notify();
    }

    fn take_offer(&mut self, offered: Option<Release>) {
        // The download on screen belongs to the release on screen.
        let is_downloading = matches!(self.download, Download::Running { .. });
        if is_downloading || self.release == offered {
            return;
        }
        if let Some(release) = &offered {
            tracing::info!(version = %release.version, "a newer version is available");
        }
        self.release = offered;
        self.download = Download::Idle;
    }

    /// Turn the "Check for updates" preference on or off. Off also drops
    /// what was already known, so nothing about updates stays on screen.
    pub fn set_enabled(&mut self, is_enabled: bool, cx: &mut Context<Self>) {
        Preferences::update(cx, |prefs| prefs.is_update_check_enabled = is_enabled);
        if is_enabled {
            self.check(cx);
            return;
        }
        self.stop_work();
        self.last_check = None;
        self.release = None;
        cx.notify();
    }

    /// Fetch the offered build into the Downloads folder.
    pub fn start_download(&mut self, cx: &mut Context<Self>) {
        let Some(platform) = update::platform() else {
            return;
        };
        let Some(release) = self.release.clone() else {
            return;
        };
        if matches!(self.download, Download::Running { .. }) {
            return;
        }
        let Some(folder) = dirs::download_dir().or_else(dirs::home_dir) else {
            self.download = Download::Failed("Could not find your Downloads folder.".into());
            cx.notify();
            return;
        };
        self.download = Download::Running { percent: 0 };
        self.is_cancelled = Arc::new(AtomicBool::new(false));
        cx.notify();
        tracing::info!(version = %release.version, folder = %folder.display(), "update download started");

        let (tx, rx) = async_channel::unbounded();
        let is_cancelled = self.is_cancelled.clone();
        std::thread::spawn(move || {
            let mut last_percent = 0;
            // One event for each percent, not one for each block read.
            let on_progress = |received, total| {
                let percent = percent_of(received, total);
                if percent != last_percent {
                    last_percent = percent;
                    let _ = tx.try_send(DownloadEvent::Progress(percent));
                }
            };
            let done = update::download_offer(
                &base_url(),
                platform,
                &release,
                version::CURRENT,
                &folder,
                &is_cancelled,
                on_progress,
            );
            let _ = tx.try_send(DownloadEvent::Finished(done));
        });

        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv().await {
                let handled = this.update(cx, |this, cx| {
                    if this.generation == generation {
                        this.take_download_event(event);
                        cx.notify();
                    }
                });
                if handled.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    pub fn cancel_download(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.download, Download::Running { .. }) {
            return;
        }
        self.stop_work();
        cx.notify();
    }

    /// The worker thread sees the flag, deletes its part file and ends.
    fn stop_work(&mut self) {
        self.is_cancelled.store(true, Ordering::Relaxed);
        self.generation += 1;
        self.is_checking = false;
        self.download = Download::Idle;
    }

    fn take_download_event(&mut self, event: DownloadEvent) {
        self.download = match event {
            DownloadEvent::Progress(percent) => Download::Running { percent },
            DownloadEvent::Finished(Ok((release, path))) => {
                tracing::info!(version = %release.version, path = %path.display(), "update downloaded");
                // Not always the build that was offered: the server may have
                // replaced it since the check.
                self.release = Some(release);
                Download::Done(path)
            }
            // Only a cancel the service did not ask for ends up here.
            DownloadEvent::Finished(Err(UpdateError::Cancelled)) => Download::Idle,
            // The server has nothing newer any more, so there is no offer.
            DownloadEvent::Finished(Err(UpdateError::Withdrawn)) => {
                tracing::info!("the offered build is no longer on the server");
                self.release = None;
                Download::Idle
            }
            DownloadEvent::Finished(Err(e)) => {
                tracing::warn!(error = %e, "update download failed");
                Download::Failed(format!("Could not download: {e}."))
            }
        };
    }

    /// Show the downloaded build in Finder or Explorer.
    pub fn reveal_download(&mut self, cx: &mut Context<Self>) {
        if self.forget_missing_download() {
            cx.notify();
            return;
        }
        let Download::Done(path) = &self.download else {
            return;
        };
        let path = path.clone();
        std::thread::spawn(move || {
            if let Err(e) = crate::file_open::reveal_in_file_manager(&path) {
                tracing::warn!(?e, path = %path.display(), "could not show the downloaded build");
            }
        });
    }

    /// The user moved or deleted the downloaded build: say so, which also
    /// brings the Download button back. Returns whether that happened.
    fn forget_missing_download(&mut self) -> bool {
        let Download::Done(path) = &self.download else {
            return false;
        };
        if path.exists() {
            return false;
        }
        self.download = Download::Failed(format!("{} is no longer there.", path.display()));
        true
    }

    /// The line under "Check for updates" on the About page.
    pub fn result_line(&self, is_enabled: bool) -> String {
        if !is_enabled {
            return "Checks are off".into();
        }
        if self.is_checking {
            return "Checking\u{2026}".into();
        }
        match (&self.last_check, &self.release) {
            (Some(Err(problem)), _) => format!("Could not check: {problem}"),
            (_, Some(release)) => format!("{} is available", release.version),
            (Some(Ok(_)), None) => "Up to date".into(),
            (None, None) => "Not checked yet".into(),
        }
    }
}

/// A check is due when the last one failed, or went well six hours ago.
fn is_check_due(last_check: &Option<Result<SystemTime, String>>, now: SystemTime) -> bool {
    let Some(Ok(checked_at)) = last_check else {
        return true;
    };
    // A clock that was set back gives no age: check then.
    !now.duration_since(*checked_at)
        .is_ok_and(|age| age < CHECK_INTERVAL)
}

fn percent_of(received: u64, total: u64) -> u8 {
    if total == 0 {
        return 0;
    }
    (received.min(total) * 100 / total) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(version: &str) -> Release {
        Release {
            version: version.into(),
            notes: String::new(),
            filename: format!("Flint-{version}.dmg"),
            size_bytes: 1000,
            sha256: "00".into(),
            download_path: "/api/v1/downloads/macos".into(),
        }
    }

    fn service() -> UpdateService {
        UpdateService {
            is_checking: false,
            last_check: None,
            release: None,
            download: Download::Idle,
            is_cancelled: Arc::new(AtomicBool::new(false)),
            generation: 0,
        }
    }

    #[test]
    fn percent_is_whole_and_never_above_a_hundred() {
        assert_eq!(percent_of(0, 1000), 0);
        assert_eq!(percent_of(499, 1000), 49);
        assert_eq!(percent_of(1000, 1000), 100);
        assert_eq!(percent_of(5000, 1000), 100);
        assert_eq!(percent_of(5, 0), 0);
        assert_eq!(percent_of(300 * 1024 * 1024, 300 * 1024 * 1024), 100);
    }

    #[test]
    fn the_result_line_says_how_the_last_check_ended() {
        let mut service = service();
        assert_eq!(service.result_line(true), "Not checked yet");
        assert_eq!(service.result_line(false), "Checks are off");

        service.is_checking = true;
        assert_eq!(service.result_line(true), "Checking\u{2026}");
        service.is_checking = false;

        service.last_check = Some(Ok(SystemTime::now()));
        assert_eq!(service.result_line(true), "Up to date");

        service.release = Some(release("0.2.0"));
        assert_eq!(service.result_line(true), "0.2.0 is available");

        service.last_check = Some(Err("could not reach the server: timed out".into()));
        assert_eq!(
            service.result_line(true),
            "Could not check: could not reach the server: timed out"
        );
        assert_eq!(service.result_line(false), "Checks are off");
    }

    #[test]
    fn a_new_offer_replaces_the_old_one_and_its_download() {
        let mut service = service();
        service.take_offer(Some(release("0.2.0")));
        service.download = Download::Done("/tmp/Flint-0.2.0.dmg".into());

        // The same offer again keeps the finished download.
        service.take_offer(Some(release("0.2.0")));
        assert!(matches!(service.download, Download::Done(_)));

        service.take_offer(Some(release("0.3.0")));
        assert_eq!(service.release, Some(release("0.3.0")));
        assert!(matches!(service.download, Download::Idle));

        service.take_offer(None);
        assert_eq!(service.release, None);
    }

    #[test]
    fn an_offer_does_not_change_under_a_running_download() {
        let mut service = service();
        service.take_offer(Some(release("0.2.0")));
        service.download = Download::Running { percent: 40 };

        service.take_offer(Some(release("0.3.0")));

        assert_eq!(service.release, Some(release("0.2.0")));
        assert!(matches!(
            service.download,
            Download::Running { percent: 40 }
        ));
    }

    #[test]
    fn download_events_move_the_state() {
        let mut service = service();
        service.release = Some(release("0.2.0"));

        service.take_download_event(DownloadEvent::Progress(7));
        assert!(matches!(service.download, Download::Running { percent: 7 }));

        service.take_download_event(DownloadEvent::Finished(Err(UpdateError::Cancelled)));
        assert!(matches!(service.download, Download::Idle));

        service.take_download_event(DownloadEvent::Finished(Err(UpdateError::WrongChecksum)));
        let Download::Failed(problem) = &service.download else {
            panic!("a wrong checksum must fail the download");
        };
        assert_eq!(
            problem,
            "Could not download: the file does not match the checksum the server announced, so it was \
             not kept."
        );
        assert_eq!(service.release, Some(release("0.2.0")));
    }

    #[test]
    fn a_finished_download_shows_the_build_that_was_saved() {
        let mut service = service();
        service.release = Some(release("0.2.0"));

        // The server had 0.3.0 by the time of the download.
        let saved = (release("0.3.0"), PathBuf::from("/tmp/Flint-0.3.0.dmg"));
        service.take_download_event(DownloadEvent::Finished(Ok(saved)));

        assert_eq!(service.release, Some(release("0.3.0")));
        assert!(
            matches!(&service.download, Download::Done(path) if path.ends_with("Flint-0.3.0.dmg"))
        );
    }

    #[test]
    fn a_withdrawn_build_takes_the_offer_away() {
        let mut service = service();
        service.release = Some(release("0.2.0"));
        service.download = Download::Running { percent: 3 };

        service.take_download_event(DownloadEvent::Finished(Err(UpdateError::Withdrawn)));

        assert_eq!(service.release, None);
        assert!(matches!(service.download, Download::Idle));
    }

    #[test]
    fn a_failed_check_is_due_again_and_a_good_one_after_six_hours() {
        let now = SystemTime::now();
        let ago = |seconds| Some(Ok(now - Duration::from_secs(seconds)));

        assert!(is_check_due(&None, now));
        assert!(is_check_due(&Some(Err("offline".into())), now));
        assert!(!is_check_due(&ago(0), now));
        assert!(!is_check_due(&ago(6 * 60 * 60 - 1), now));
        assert!(is_check_due(&ago(6 * 60 * 60), now));
        // The clock was set back after the check.
        assert!(is_check_due(&Some(Ok(now + Duration::from_secs(60))), now));
    }

    #[test]
    fn a_downloaded_build_that_is_gone_can_be_downloaded_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Flint-0.2.0.dmg");
        std::fs::write(&path, "build").unwrap();
        let mut service = service();
        service.download = Download::Done(path.clone());

        assert!(!service.forget_missing_download());
        assert!(matches!(service.download, Download::Done(_)));

        std::fs::remove_file(&path).unwrap();
        assert!(service.forget_missing_download());
        let Download::Failed(problem) = &service.download else {
            panic!("a missing file must bring the Download button back");
        };
        assert!(
            problem.ends_with("Flint-0.2.0.dmg is no longer there."),
            "{problem}"
        );
    }

    #[test]
    fn stopping_resets_the_download_and_tells_the_worker() {
        let mut service = service();
        service.download = Download::Running { percent: 40 };
        service.is_checking = true;
        let flag = service.is_cancelled.clone();

        service.stop_work();

        assert!(flag.load(Ordering::Relaxed));
        assert!(matches!(service.download, Download::Idle));
        assert!(!service.is_checking);
        assert_eq!(service.generation, 1);
    }
}
