//! Log file, panic hook and the text behind "Copy diagnostics".

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

const LOG_FILE_NAME: &str = "flint.log";
const OLD_LOG_FILE_NAME: &str = "flint.log.old";
/// One full file plus the previous one stay on disk, so about twice this.
const MAX_LOG_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Both outputs stay at `info` for our own code: `debug` prints remote file
/// paths, and the file would keep them on disk.
const STDERR_FILTER: &str = "warn,flint=info";
const FILE_FILTER: &str = "warn,flint=info";

pub const REPOSITORY_URL: &str = env!("CARGO_PKG_REPOSITORY");
pub const ISSUES_URL: &str = concat!(env!("CARGO_PKG_REPOSITORY"), "/issues");

/// Where the log file lives: `logs/flint.log` next to the vault.
pub fn log_file_path() -> Option<PathBuf> {
    crate::vault::app_data_dir().map(|dir| dir.join("logs").join(LOG_FILE_NAME))
}

/// Sets up stderr and file logging and makes panics go to the log.
/// `RUST_LOG`, when set, replaces the default level of both outputs.
pub fn init() {
    let log_file = open_log_file();
    let (file_writer, file_error) = match log_file {
        Ok(writer) => (Some(writer), None),
        Err(error) => (None, Some(error)),
    };

    let file_layer = file_writer.map(|writer| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(Mutex::new(writer))
            .with_filter(env_filter_or(FILE_FILTER))
    });
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(env_filter_or(STDERR_FILTER)))
        .with(file_layer)
        .init();

    if let Some(error) = file_error {
        tracing::warn!(%error, "could not open the log file, logging to stderr only");
    }
    log_panics();
    tracing::info!(
        version = crate::version::CURRENT,
        commit = crate::version::COMMIT,
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        "flint started"
    );
}

/// Version and platform for a bug report. Holds nothing about the user:
/// no paths, host names or log lines.
pub fn diagnostics_text() -> String {
    let build = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    format!(
        "flint {} ({}, {build})\nOS: {} {}\n",
        crate::version::CURRENT,
        crate::version::COMMIT,
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
}

fn env_filter_or(default_filter: &str) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter))
}

fn open_log_file() -> io::Result<RotatingLogFile> {
    let path = log_file_path().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not determine application data directory",
        )
    })?;
    RotatingLogFile::open(path, MAX_LOG_FILE_BYTES)
}

fn log_panics() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        // A release build on Windows has no console, so the log file is the
        // only place a crash can leave a trace.
        tracing::error!(panic = %panic_info, "flint panicked");
        default_hook(panic_info);
    }));
}

/// An append-only log file that keeps one older file. When a write would
/// push the file over `max_bytes`, the file becomes `flint.log.old` and a new
/// one is started.
struct RotatingLogFile {
    path: PathBuf,
    /// `None` when the file could not be reopened after a rotation. Lines are
    /// then dropped, because a broken log file must not break the app.
    file: Option<File>,
    written_bytes: u64,
    max_bytes: u64,
}

impl RotatingLogFile {
    fn open(path: PathBuf, max_bytes: u64) -> io::Result<Self> {
        if let Some(directory) = path.parent() {
            std::fs::create_dir_all(directory)?;
        }
        let file = open_for_append(&path)?;
        let written_bytes = file.metadata()?.len();
        Ok(Self {
            path,
            file: Some(file),
            written_bytes,
            max_bytes,
        })
    }

    fn rotate(&mut self) -> io::Result<()> {
        // Close the file first: Windows may refuse to rename an open file.
        self.file = None;
        self.written_bytes = 0;
        let is_renamed =
            std::fs::rename(&self.path, self.path.with_file_name(OLD_LOG_FILE_NAME)).is_ok();
        // If the rename failed, start the same file again from zero: losing
        // old lines is better than a log that grows forever or goes silent.
        self.file = Some(if is_renamed {
            open_for_append(&self.path)?
        } else {
            log_open_options().truncate(true).open(&self.path)?
        });
        Ok(())
    }
}

impl Write for RotatingLogFile {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let is_over_limit = self.written_bytes + buffer.len() as u64 > self.max_bytes;
        if is_over_limit && self.written_bytes > 0 && self.file.is_some() {
            // The error cannot be logged from inside the log writer.
            let _ = self.rotate();
        }
        let Some(file) = self.file.as_mut() else {
            return Ok(buffer.len());
        };
        file.write_all(buffer)?;
        self.written_bytes += buffer.len() as u64;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

fn open_for_append(path: &Path) -> io::Result<File> {
    log_open_options().append(true).open(path)
}

fn log_open_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.create(true).write(true);
    // The log holds host names and user names, so keep it private like the vault.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn appends_to_an_existing_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("logs").join(LOG_FILE_NAME);

        let mut log = RotatingLogFile::open(path.clone(), 100).unwrap();
        log.write_all(b"first\n").unwrap();
        drop(log);
        let mut log = RotatingLogFile::open(path.clone(), 100).unwrap();
        log.write_all(b"second\n").unwrap();

        assert_eq!(read(&path), "first\nsecond\n");
        assert_eq!(log.written_bytes, 13);
    }

    #[test]
    fn rotates_when_a_write_would_pass_the_limit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(LOG_FILE_NAME);
        let old_path = directory.path().join(OLD_LOG_FILE_NAME);

        let mut log = RotatingLogFile::open(path.clone(), 10).unwrap();
        log.write_all(b"123456\n").unwrap();
        assert!(!old_path.exists());
        log.write_all(b"abcdef\n").unwrap();

        assert_eq!(read(&old_path), "123456\n");
        assert_eq!(read(&path), "abcdef\n");
    }

    #[test]
    fn keeps_only_one_old_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(LOG_FILE_NAME);

        let mut log = RotatingLogFile::open(path.clone(), 10).unwrap();
        for line in ["111111\n", "222222\n", "333333\n"] {
            log.write_all(line.as_bytes()).unwrap();
        }

        assert_eq!(read(&directory.path().join(OLD_LOG_FILE_NAME)), "222222\n");
        assert_eq!(read(&path), "333333\n");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
    }

    #[test]
    fn a_line_longer_than_the_limit_is_still_written() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(LOG_FILE_NAME);

        let mut log = RotatingLogFile::open(path.clone(), 4).unwrap();
        log.write_all(b"a long line\n").unwrap();

        assert_eq!(read(&path), "a long line\n");
        assert!(!directory.path().join(OLD_LOG_FILE_NAME).exists());
    }

    #[test]
    fn keeps_logging_when_the_old_file_cannot_be_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(LOG_FILE_NAME);
        // A non-empty directory in the way makes the rename fail.
        let old_path = directory.path().join(OLD_LOG_FILE_NAME);
        std::fs::create_dir(&old_path).unwrap();
        std::fs::write(old_path.join("blocker"), b"x").unwrap();

        let mut log = RotatingLogFile::open(path.clone(), 10).unwrap();
        log.write_all(b"123456\n").unwrap();
        log.write_all(b"abcdef\n").unwrap();
        log.write_all(b"xy\n").unwrap();

        assert_eq!(read(&path), "abcdef\nxy\n");
        assert_eq!(log.written_bytes, 10);
    }

    #[cfg(unix)]
    #[test]
    fn log_file_is_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(LOG_FILE_NAME);

        let _log = RotatingLogFile::open(path.clone(), 10).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0);
    }

    #[test]
    fn diagnostics_have_version_and_platform_only() {
        let text = diagnostics_text();

        assert!(text.starts_with(concat!("flint ", env!("CARGO_PKG_VERSION"))));
        assert!(text.contains(std::env::consts::OS));
        assert!(text.contains(std::env::consts::ARCH));
        assert_eq!(text.lines().count(), 2);
    }

    #[test]
    fn issues_link_points_at_the_repository() {
        assert!(REPOSITORY_URL.starts_with("https://"));
        assert_eq!(ISSUES_URL, format!("{REPOSITORY_URL}/issues"));
    }
}
