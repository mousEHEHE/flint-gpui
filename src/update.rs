//! The update check and the download of a newer build.
//!
//! Every request the app makes about updates is in this file, and there are
//! two: [`latest_release`] asks which build is current, and [`download`]
//! fetches it when the user presses Download. [`newer_release`] and
//! [`download_offer`] are built from these two. Both say which platform is
//! asking and nothing else: no account, no session, no version, no device.
//! Blocking: call them from a worker thread.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use serde::Deserialize;
use sha2::{Digest as _, Sha256};

/// A build above this size is not what the server would ever publish.
pub const MAX_DOWNLOAD_BYTES: u64 = 300 * 1024 * 1024;
const MAX_FILE_NAME_CHARS: usize = 100;
/// The server's own limit for notes.
const MAX_NOTES_CHARS: usize = 2000;
const CHECK_TIMEOUT: Duration = Duration::from_secs(20);
const PART_SUFFIX: &str = ".flint-part";
/// A running download writes to its part file all the time. One that was not
/// written to for this long belongs to a download that died.
const STALE_PART_AGE: Duration = Duration::from_secs(60 * 60);

/// The server's name for the OS this app was built for. `None` where there
/// are no builds to offer, and then nothing is ever asked.
pub fn platform() -> Option<&'static str> {
    // macOS sets this variable for an app that runs in the sandbox.
    let is_sandboxed = std::env::var_os("APP_SANDBOX_CONTAINER_ID").is_some();
    platform_for(std::env::consts::OS, is_sandboxed)
}

fn platform_for(os: &str, is_sandboxed: bool) -> Option<&'static str> {
    // The Mac App Store build is the sandboxed one. It may not write to
    // Downloads, and the store is the only place it may update from.
    if is_sandboxed {
        return None;
    }
    match os {
        "macos" => Some("macos"),
        "windows" => Some("windows"),
        _ => None,
    }
}

/// The build the server offers for a platform.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Release {
    pub version: String,
    /// Plain text from the server. Never shown as markup.
    #[serde(default)]
    pub notes: String,
    pub filename: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub download_path: String,
}

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("could not reach the server: {0}")]
    Network(String),
    #[error("the server answered with status {0}")]
    Status(u16),
    #[error("the server sent an answer flint does not understand")]
    BadAnswer,
    #[error("the file is larger than flint accepts")]
    TooLarge,
    #[error("the file is not the size the server announced, so it was not kept")]
    WrongSize,
    #[error("the file does not match the checksum the server announced, so it was not kept")]
    WrongChecksum,
    #[error("this build is no longer on the server")]
    Withdrawn,
    #[error("cancelled")]
    Cancelled,
    #[error("could not save the file: {0}")]
    Io(#[from] std::io::Error),
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .timeout_write(Duration::from_secs(30))
        // The sync client names its version here. This one must not: the
        // request is the same from every copy of the app.
        .user_agent("Flint")
        // A redirect could lead to another host. The server never sends one,
        // so one that shows up is refused as a wrong status.
        .redirects(0)
        .build()
}

fn request_error(error: ureq::Error) -> UpdateError {
    match error {
        ureq::Error::Status(status, _) => UpdateError::Status(status),
        ureq::Error::Transport(transport) => UpdateError::Network(transport.to_string()),
    }
}

/// Ask the server for the current build of `platform`. `None` when it has
/// none. `base_url` is the API's address, as `account::api::base_url` gives it.
pub fn latest_release(base_url: &str, platform: &str) -> Result<Option<Release>, UpdateError> {
    let url = format!("{base_url}/releases/latest?platform={platform}");
    let response = match agent().get(&url).timeout(CHECK_TIMEOUT).call() {
        Ok(response) => response,
        Err(ureq::Error::Status(404, _)) => return Ok(None),
        Err(error) => return Err(request_error(error)),
    };
    if response.status() != 200 {
        return Err(UpdateError::Status(response.status()));
    }
    let text = response.into_string().map_err(|_| UpdateError::BadAnswer)?;
    let release = serde_json::from_str(&text).map_err(|_| UpdateError::BadAnswer)?;
    fit_for_display(release)
        .map(Some)
        .ok_or(UpdateError::BadAnswer)
}

/// The dialog shows the notes and the file name as the server sent them, so
/// they are cut down here: notes of a sane length without invisible
/// characters, and the file name the build is saved under.
fn fit_for_display(mut release: Release) -> Option<Release> {
    release.filename = safe_file_name(&release.filename)?;
    release.notes = plain_text(&release.notes);
    Some(release)
}

fn plain_text(text: &str) -> String {
    text.trim()
        .chars()
        .filter(|&c| matches!(c, '\n' | '\t') || !(c.is_control() || is_invisible(c)))
        .take(MAX_NOTES_CHARS)
        .collect()
}

/// Zero-width characters and the ones that turn the direction of text.
fn is_invisible(c: char) -> bool {
    matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{206F}' | '\u{FEFF}')
}

/// The server's current build, when it is a later version than `current`.
/// This is the build the app tells the user about.
pub fn newer_release(
    base_url: &str,
    platform: &str,
    current: &str,
) -> Result<Option<Release>, UpdateError> {
    let offered = latest_release(base_url, platform)?;
    Ok(offered.filter(|release| crate::version::is_newer(&release.version, current)))
}

/// [`download`] of the build the user was offered. The offer can be hours
/// old: when the server has replaced the build since, the file it sends does
/// not match the offer. Then the server is asked again and its current build
/// is fetched, if it is still newer than `current`.
///
/// Returns the build that was saved and where. `on_progress` gets the bytes
/// received so far and the size of the build being fetched.
pub fn download_offer(
    base_url: &str,
    platform: &str,
    offered: &Release,
    current: &str,
    folder: &Path,
    is_cancelled: &AtomicBool,
    mut on_progress: impl FnMut(u64, u64),
) -> Result<(Release, PathBuf), UpdateError> {
    let mismatch = match download(base_url, offered, folder, is_cancelled, |received| {
        on_progress(received, offered.size_bytes)
    }) {
        Ok(path) => return Ok((offered.clone(), path)),
        Err(e @ (UpdateError::WrongSize | UpdateError::WrongChecksum)) => e,
        // The server has no build any more.
        Err(UpdateError::Status(404)) => return Err(UpdateError::Withdrawn),
        Err(e) => return Err(e),
    };
    if is_cancelled.load(Ordering::Relaxed) {
        return Err(UpdateError::Cancelled);
    }
    let Some(replacement) = newer_release(base_url, platform, current)? else {
        return Err(UpdateError::Withdrawn);
    };
    if replacement == *offered {
        // Not replaced: the file really is broken.
        return Err(mismatch);
    }
    tracing::info!(version = %replacement.version, "the offered build was replaced on the server");
    let path = download(base_url, &replacement, folder, is_cancelled, |received| {
        on_progress(received, replacement.size_bytes)
    })?;
    Ok((replacement, path))
}

/// Fetch the build into `folder` and check it against what the server
/// announced. Returns where the file is. Nothing is opened or run.
///
/// `on_progress` gets the number of bytes received so far.
pub fn download(
    base_url: &str,
    release: &Release,
    folder: &Path,
    is_cancelled: &AtomicBool,
    on_progress: impl FnMut(u64),
) -> Result<PathBuf, UpdateError> {
    let name = safe_file_name(&release.filename).ok_or(UpdateError::BadAnswer)?;
    let url = download_url(base_url, &release.download_path).ok_or(UpdateError::BadAnswer)?;
    if release.size_bytes > MAX_DOWNLOAD_BYTES {
        return Err(UpdateError::TooLarge);
    }
    // Downloaded before, for example from the dialog an hour ago.
    let target = folder.join(&name);
    if is_file_as_announced(&target, release) {
        return Ok(target);
    }

    std::fs::create_dir_all(folder)?;
    remove_stale_parts(folder, STALE_PART_AGE);
    let part = part_path(folder, &name);
    let placed = fetch_to(&url, &part, release, is_cancelled, on_progress).and_then(|()| {
        let path = free_path(folder, &name);
        std::fs::rename(&part, &path)?;
        mark_as_downloaded(&path);
        Ok(path)
    });
    if placed.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    placed
}

/// Tell the OS the file came from the internet, as a browser does for its
/// downloads. Opening it then goes through Gatekeeper or SmartScreen, which
/// is the only check left if the server itself was taken over. Best effort:
/// a file system without these marks still gets the file.
#[cfg(target_os = "macos")]
fn mark_as_downloaded(path: &Path) {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    // flags; time of download in hex; the app that fetched it; an id.
    let mark = format!("0081;{seconds:x};Flint;{}", uuid::Uuid::new_v4());
    let marked = std::process::Command::new("/usr/bin/xattr")
        .args(["-w", "com.apple.quarantine", &mark])
        .arg(path)
        .status();
    if !marked.is_ok_and(|status| status.success()) {
        tracing::warn!(path = %path.display(), "could not mark the download as quarantined");
    }
}

#[cfg(windows)]
fn mark_as_downloaded(path: &Path) {
    // The "mark of the web": a second stream of the file, zone 3 = internet.
    let mut stream = path.as_os_str().to_owned();
    stream.push(":Zone.Identifier");
    if let Err(e) = std::fs::write(&stream, "[ZoneTransfer]\r\nZoneId=3\r\n") {
        tracing::warn!(?e, path = %path.display(), "could not mark the download as from the internet");
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn mark_as_downloaded(_path: &Path) {}

/// A name of its own for every attempt. A cancelled download can still be
/// running when the next one starts, and must not share its file.
fn part_path(folder: &Path, name: &str) -> PathBuf {
    folder.join(format!(
        "{name}.{}{PART_SUFFIX}",
        uuid::Uuid::new_v4().simple()
    ))
}

/// flint was quit or killed in the middle of a download: its part file is
/// still there and nothing else would ever remove it.
fn remove_stale_parts(folder: &Path, stale_age: Duration) {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        let is_part = entry.file_name().to_string_lossy().ends_with(PART_SUFFIX);
        if !is_part || !is_older_than(&entry, stale_age) {
            continue;
        }
        if let Err(e) = std::fs::remove_file(entry.path()) {
            tracing::debug!(?e, path = %entry.path().display(), "could not remove an old part file");
        }
    }
}

fn is_older_than(entry: &std::fs::DirEntry, age: Duration) -> bool {
    let Ok(modified) = entry.metadata().and_then(|file| file.modified()) else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|since| since > age)
}

fn fetch_to(
    url: &str,
    part: &Path,
    release: &Release,
    is_cancelled: &AtomicBool,
    on_progress: impl FnMut(u64),
) -> Result<(), UpdateError> {
    if is_cancelled.load(Ordering::Relaxed) {
        return Err(UpdateError::Cancelled);
    }
    let response = agent().get(url).call().map_err(request_error)?;
    if response.status() != 200 {
        return Err(UpdateError::Status(response.status()));
    }
    let announced = response
        .header("Content-Length")
        .and_then(|length| length.parse::<u64>().ok());
    if announced.is_some_and(|length| length != release.size_bytes) {
        return Err(UpdateError::WrongSize);
    }
    // One byte more than expected is enough to see that the file is too long.
    let body = response.into_reader().take(release.size_bytes + 1);
    let mut file = File::create_new(part)?;
    let (size, checksum) = copy_and_hash(body, &mut file, is_cancelled, on_progress)?;
    file.sync_all()?;
    check_file(size, &checksum, release)
}

fn check_file(size: u64, checksum: &str, release: &Release) -> Result<(), UpdateError> {
    if size != release.size_bytes {
        return Err(UpdateError::WrongSize);
    }
    if !checksum.eq_ignore_ascii_case(&release.sha256) {
        return Err(UpdateError::WrongChecksum);
    }
    Ok(())
}

/// Copy everything, and return how many bytes there were and their SHA-256
/// as hex.
fn copy_and_hash(
    mut from: impl Read,
    mut to: impl Write,
    is_cancelled: &AtomicBool,
    mut on_progress: impl FnMut(u64),
) -> Result<(u64, String), UpdateError> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        if is_cancelled.load(Ordering::Relaxed) {
            return Err(UpdateError::Cancelled);
        }
        let count = from
            .read(&mut buffer)
            .map_err(|e| UpdateError::Network(e.to_string()))?;
        if count == 0 {
            break;
        }
        to.write_all(&buffer[..count])?;
        hasher.update(&buffer[..count]);
        size += count as u64;
        on_progress(size);
    }
    let checksum = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok((size, checksum))
}

/// Whether the file at `path` is exactly the build the server announced.
fn is_file_as_announced(path: &Path, release: &Release) -> bool {
    let has_the_size = path
        .metadata()
        .is_ok_and(|file| file.is_file() && file.len() == release.size_bytes);
    if !has_the_size {
        return false;
    }
    let Ok(file) = File::open(path) else {
        return false;
    };
    copy_and_hash(file, std::io::sink(), &AtomicBool::new(false), |_| {})
        .is_ok_and(|(size, checksum)| check_file(size, &checksum, release).is_ok())
}

/// The file name the server sent, made safe to create in a folder: the last
/// path component only, in letters, digits, `.`, `_` and `-`. `None` when
/// nothing usable is left.
fn safe_file_name(name: &str) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    // No hidden file, no "..", and no trailing dot, which Windows drops.
    let cleaned = cleaned.trim_matches('.');
    let is_usable = !cleaned.is_empty()
        && cleaned.len() <= MAX_FILE_NAME_CHARS
        && !is_windows_device_name(cleaned);
    is_usable.then(|| cleaned.to_string())
}

/// "NUL", "con.dmg", "COM1.exe": Windows opens a device for these names, with
/// any extension, and not a file.
fn is_windows_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    let is_numbered_port = stem.len() == 4
        && (stem.starts_with("COM") || stem.starts_with("LPT"))
        && stem.ends_with(|c: char| c.is_ascii_digit());
    is_numbered_port || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
}

/// `name` in `folder`, or "name (1).ext", "name (2).ext" and so on when the
/// names before it are taken.
fn free_path(folder: &Path, name: &str) -> PathBuf {
    let plain = folder.join(name);
    if !plain.exists() {
        return plain;
    }
    let (stem, extension) = match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name, ""),
    };
    let mut number = 1;
    loop {
        let path = folder.join(format!("{stem} ({number}){extension}"));
        if !path.exists() {
            return path;
        }
        number += 1;
    }
}

/// The address of the build: the host of `base_url` and the path the server
/// named. The host always comes from the app, so the answer of the server
/// cannot point the download somewhere else.
fn download_url(base_url: &str, download_path: &str) -> Option<String> {
    let is_plain_path = download_path.starts_with('/')
        && !download_path.contains("//")
        && !download_path.contains("..")
        && download_path
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-'));
    if !is_plain_path {
        return None;
    }
    Some(format!("{}{download_path}", site_of(base_url)?))
}

/// "https://host:8443" out of "https://host:8443/api/v1".
fn site_of(base_url: &str) -> Option<&str> {
    let host_start = base_url.find("://")? + 3;
    let host_end = base_url[host_start..]
        .find('/')
        .map_or(base_url.len(), |slash| host_start + slash);
    (host_end > host_start).then(|| &base_url[..host_end])
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, BufReader};
    use std::net::TcpListener;

    use super::*;

    const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    fn hello_release() -> Release {
        Release {
            version: "0.2.0".into(),
            notes: String::new(),
            filename: "Flint-0.2.0.dmg".into(),
            size_bytes: 5,
            sha256: HELLO_SHA256.into(),
            download_path: "/api/v1/downloads/macos".into(),
        }
    }

    fn not_cancelled() -> AtomicBool {
        AtomicBool::new(false)
    }

    /// A server that answers one request with `answer` and hands back the
    /// request it got. Returns the API address to give the client.
    fn serve_once(answer: Vec<u8>) -> (String, std::thread::JoinHandle<String>) {
        serve_once_after(answer, || {})
    }

    /// The same, and `before_answer` runs when the request has arrived.
    fn serve_once_after(
        answer: Vec<u8>,
        before_answer: impl FnOnce() + Send + 'static,
    ) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}/api/v1", listener.local_addr().unwrap());
        let served = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line.trim().is_empty() {
                    break;
                }
                request.push_str(&line);
            }
            before_answer();
            stream.write_all(&answer).unwrap();
            request
        });
        (base_url, served)
    }

    /// The same, for several requests in a row: one answer for each.
    fn serve_each(answers: Vec<Vec<u8>>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}/api/v1", listener.local_addr().unwrap());
        let served = std::thread::spawn(move || {
            let mut request_lines = Vec::new();
            for answer in answers {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first_line = String::new();
                reader.read_line(&mut first_line).unwrap();
                let mut line = first_line.clone();
                while !line.trim().is_empty() {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                }
                stream.write_all(&answer).unwrap();
                request_lines.push(first_line.trim().to_string());
            }
            request_lines
        });
        (base_url, served)
    }

    fn release_answer(release: &Release) -> Vec<u8> {
        let body = serde_json::json!({
            "platform": "macos",
            "version": release.version,
            "notes": release.notes,
            "filename": release.filename,
            "size_bytes": release.size_bytes,
            "sha256": release.sha256,
            "published_at": "2026-10-11T10:00:00Z",
            "download_path": release.download_path,
        });
        file_answer(body.to_string().as_bytes())
    }

    fn http(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut answer =
            format!("HTTP/1.1 {status}\r\nConnection: close\r\n{headers}\r\n").into_bytes();
        answer.extend_from_slice(body);
        answer
    }

    fn file_answer(body: &[u8]) -> Vec<u8> {
        http(
            "200 OK",
            &format!("Content-Length: {}\r\n", body.len()),
            body,
        )
    }

    #[test]
    fn the_sandboxed_store_build_has_no_platform_to_ask_for() {
        assert_eq!(platform_for("macos", false), Some("macos"));
        assert_eq!(platform_for("windows", false), Some("windows"));
        assert_eq!(platform_for("linux", false), None);
        assert_eq!(platform_for("macos", true), None);
    }

    #[test]
    fn a_windows_device_name_is_refused() {
        for name in [
            "NUL",
            "nul",
            "CON.dmg",
            "aux.tar.gz",
            "PRN.exe",
            "COM1",
            "com9.dmg",
            "LPT1.exe",
        ] {
            assert_eq!(safe_file_name(name), None, "{name:?}");
        }
        for name in [
            "CONSOLE.dmg",
            "nullable.exe",
            "COM.exe",
            "COM10.exe",
            "Flint-CON.dmg",
        ] {
            assert!(safe_file_name(name).is_some(), "{name:?}");
        }
    }

    #[test]
    fn a_file_name_keeps_its_base_name_only() {
        assert_eq!(
            safe_file_name("Flint-0.2.0.dmg").as_deref(),
            Some("Flint-0.2.0.dmg")
        );
        assert_eq!(
            safe_file_name("../../etc/passwd").as_deref(),
            Some("passwd")
        );
        assert_eq!(
            safe_file_name("/Applications/Flint.app").as_deref(),
            Some("Flint.app")
        );
        assert_eq!(
            safe_file_name(r"C:\Windows\flint_setup.exe").as_deref(),
            Some("flint_setup.exe")
        );
        assert_eq!(
            safe_file_name(r"..\..\evil.exe").as_deref(),
            Some("evil.exe")
        );
    }

    #[test]
    fn a_file_name_is_cleaned_to_a_small_set_of_characters() {
        assert_eq!(
            safe_file_name("Flint 0.2.0 (arm).dmg").as_deref(),
            Some("Flint-0.2.0--arm-.dmg")
        );
        assert_eq!(
            safe_file_name("fli\u{202e}gmd.exe").as_deref(),
            Some("fli-gmd.exe")
        );
        assert_eq!(safe_file_name("a:b|c?.zip").as_deref(), Some("a-b-c-.zip"));
        assert_eq!(safe_file_name("флинт.dmg").as_deref(), Some("-----.dmg"));
        assert_eq!(safe_file_name(".hidden").as_deref(), Some("hidden"));
        assert_eq!(safe_file_name("setup.exe.").as_deref(), Some("setup.exe"));
    }

    #[test]
    fn a_file_name_with_nothing_usable_is_refused() {
        for name in ["", ".", "..", "/", "dir/", "...", r"a\.."] {
            assert_eq!(safe_file_name(name), None, "{name:?}");
        }
        assert_eq!(safe_file_name(&"a".repeat(101)), None);
        assert!(safe_file_name(&"a".repeat(100)).is_some());
    }

    #[test]
    fn a_safe_file_name_stays_inside_the_folder() {
        let folder = Path::new("/home/me/Downloads");
        for name in [
            "../../x",
            "/etc/passwd",
            r"..\..\x",
            "a/../../b",
            "..",
            "~/x",
        ] {
            let Some(safe) = safe_file_name(name) else {
                continue;
            };
            assert_eq!(folder.join(&safe).parent(), Some(folder), "{name:?}");
        }
    }

    #[test]
    fn a_free_name_is_the_plain_one_or_the_next_number() {
        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| dir.path().join(name);

        assert_eq!(
            free_path(dir.path(), "Flint-0.2.0.dmg"),
            path("Flint-0.2.0.dmg")
        );
        std::fs::write(path("Flint-0.2.0.dmg"), "old").unwrap();
        assert_eq!(
            free_path(dir.path(), "Flint-0.2.0.dmg"),
            path("Flint-0.2.0 (1).dmg")
        );
        std::fs::write(path("Flint-0.2.0 (1).dmg"), "old").unwrap();
        assert_eq!(
            free_path(dir.path(), "Flint-0.2.0.dmg"),
            path("Flint-0.2.0 (2).dmg")
        );

        std::fs::write(path("setup"), "old").unwrap();
        assert_eq!(free_path(dir.path(), "setup"), path("setup (1)"));
    }

    #[test]
    fn the_download_address_keeps_the_host_the_app_uses() {
        let base = "https://flint.mouse.pw:8443/api/v1";
        assert_eq!(
            download_url(base, "/api/v1/downloads/macos").as_deref(),
            Some("https://flint.mouse.pw:8443/api/v1/downloads/macos")
        );
        assert_eq!(
            download_url("http://127.0.0.1:8766/api/v1", "/api/v1/downloads/windows").as_deref(),
            Some("http://127.0.0.1:8766/api/v1/downloads/windows")
        );
        assert_eq!(
            site_of("https://flint.mouse.pw"),
            Some("https://flint.mouse.pw")
        );
    }

    #[test]
    fn a_download_path_that_could_change_the_host_is_refused() {
        let base = "https://flint.mouse.pw:8443/api/v1";
        let bad = [
            "",
            "api/v1/downloads/macos",
            "https://evil.example/flint.dmg",
            "//evil.example/flint.dmg",
            "@evil.example/flint.dmg",
            "/@evil.example",
            "/a/../../b",
            "/downloads/macos?x=1",
            "/downloads/mac os",
            "/downloads\\macos",
            ".evil.example/x",
            ":8080/x",
        ];
        for path in bad {
            assert_eq!(download_url(base, path), None, "{path:?}");
        }
        assert_eq!(download_url("flint.mouse.pw", "/x"), None);
        assert_eq!(download_url("https://", "/x"), None);
    }

    #[test]
    fn copying_counts_the_bytes_and_hashes_them() {
        let mut copy = Vec::new();
        let mut seen = Vec::new();

        let (size, checksum) =
            copy_and_hash(&b"hello"[..], &mut copy, &not_cancelled(), |so_far| {
                seen.push(so_far)
            })
            .unwrap();

        assert_eq!((size, checksum.as_str()), (5, HELLO_SHA256));
        assert_eq!(copy, b"hello");
        assert_eq!(seen.last(), Some(&5));
    }

    #[test]
    fn a_cancelled_copy_stops() {
        let stopped = copy_and_hash(
            &b"hello"[..],
            std::io::sink(),
            &AtomicBool::new(true),
            |_| {},
        );
        assert!(matches!(stopped, Err(UpdateError::Cancelled)));
    }

    #[test]
    fn a_file_is_checked_by_size_and_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("build.dmg");
        let release = hello_release();

        assert!(!is_file_as_announced(&path, &release));
        std::fs::write(&path, "hello").unwrap();
        assert!(is_file_as_announced(&path, &release));
        // The checksum is compared without regard to case.
        let upper = Release {
            sha256: HELLO_SHA256.to_uppercase(),
            ..release.clone()
        };
        assert!(is_file_as_announced(&path, &upper));

        std::fs::write(&path, "hellp").unwrap();
        assert!(!is_file_as_announced(&path, &release));
        std::fs::write(&path, "hello!").unwrap();
        assert!(!is_file_as_announced(&path, &release));
        assert!(!is_file_as_announced(dir.path(), &release));
    }

    #[test]
    fn the_check_sends_the_platform_and_nothing_about_the_user() {
        let body = br#"{"platform":"macos","version":"0.2.0","notes":"Faster.\nSmaller.",
            "filename":"Flint-0.2.0.dmg","size_bytes":5,"sha256":"abc",
            "published_at":"2026-10-11T10:00:00Z","download_path":"/api/v1/downloads/macos"}"#;
        let (base_url, served) = serve_once(file_answer(body));

        let release = latest_release(&base_url, "macos").unwrap().unwrap();
        let request = served.join().unwrap();

        assert_eq!(release.version, "0.2.0");
        assert_eq!(release.notes, "Faster.\nSmaller.");
        assert_eq!(release.size_bytes, 5);
        let mut lines = request.lines();
        assert_eq!(
            lines.next(),
            Some("GET /api/v1/releases/latest?platform=macos HTTP/1.1")
        );
        let mut headers: Vec<String> = lines
            .map(|line| line.split(':').next().unwrap().to_ascii_lowercase())
            .collect();
        headers.sort();
        assert_eq!(
            headers,
            ["accept", "accept-encoding", "host", "user-agent"],
            "{request}"
        );
        assert!(request.contains("User-Agent: Flint\r\n"), "{request}");
        assert!(!request.contains(crate::version::CURRENT), "{request}");
    }

    #[test]
    fn notes_and_file_name_from_the_server_are_cut_down_to_what_can_be_shown() {
        let sent = Release {
            notes: format!(
                "  Fast\u{202e}er.\r\n\u{200b}Small\u{7}er.\n{}",
                "x".repeat(5000)
            ),
            filename: "dir/Fli\u{202e}nt 0.2.0.dmg".into(),
            ..hello_release()
        };
        let (base_url, served) = serve_once(release_answer(&sent));

        let release = latest_release(&base_url, "macos").unwrap().unwrap();
        served.join().unwrap();

        assert!(
            release.notes.starts_with("Faster.\nSmaller.\nxxx"),
            "{:?}",
            &release.notes[..40]
        );
        assert_eq!(release.notes.chars().count(), MAX_NOTES_CHARS);
        // The name on screen is the name the file is saved under.
        assert_eq!(release.filename, "Fli-nt-0.2.0.dmg");
        assert_eq!(
            safe_file_name(&release.filename).as_deref(),
            Some("Fli-nt-0.2.0.dmg")
        );
    }

    #[test]
    fn a_build_with_no_usable_file_name_is_a_broken_answer() {
        let sent = Release {
            filename: "NUL".into(),
            ..hello_release()
        };
        let (base_url, served) = serve_once(release_answer(&sent));

        assert!(matches!(
            latest_release(&base_url, "macos"),
            Err(UpdateError::BadAnswer)
        ));
        served.join().unwrap();
    }

    #[test]
    fn a_platform_without_a_build_is_not_an_error() {
        let body = br#"{"error":{"code":"no_build","message":"there is no build"}}"#;
        let (base_url, served) = serve_once(http("404 Not Found", "", body));

        assert_eq!(latest_release(&base_url, "macos").unwrap(), None);
        served.join().unwrap();
    }

    #[test]
    fn a_broken_answer_to_the_check_is_an_error() {
        let (base_url, served) = serve_once(file_answer(b"<html>captive portal</html>"));
        assert!(matches!(
            latest_release(&base_url, "macos"),
            Err(UpdateError::BadAnswer)
        ));
        served.join().unwrap();

        let (base_url, served) = serve_once(http("500 Internal Server Error", "", b""));
        assert!(matches!(
            latest_release(&base_url, "macos"),
            Err(UpdateError::Status(500))
        ));
        served.join().unwrap();
    }

    #[test]
    fn a_download_lands_in_the_folder_and_leaves_no_part_file() {
        let dir = tempfile::tempdir().unwrap();
        let (base_url, served) = serve_once(file_answer(b"hello"));
        let mut received = 0;

        let path = download(
            &base_url,
            &hello_release(),
            dir.path(),
            &not_cancelled(),
            |so_far| received = so_far,
        )
        .unwrap();
        let request = served.join().unwrap();

        assert_eq!(path, dir.path().join("Flint-0.2.0.dmg"));
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        assert_eq!(received, 5);
        assert_eq!(files_in(&dir), ["Flint-0.2.0.dmg"]);
        assert!(
            request.starts_with("GET /api/v1/downloads/macos HTTP/1.1"),
            "{request}"
        );
        assert!(
            !request.to_ascii_lowercase().contains("authorization"),
            "{request}"
        );
    }

    #[test]
    fn a_build_that_is_already_there_is_used_again_without_a_request() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Flint-0.2.0.dmg");
        std::fs::write(&path, "hello").unwrap();

        // Nothing listens on this address: a request would fail.
        let found = download(
            "http://127.0.0.1:1/api/v1",
            &hello_release(),
            dir.path(),
            &not_cancelled(),
            |_| {},
        );

        assert_eq!(found.unwrap(), path);
    }

    #[test]
    fn another_file_of_the_same_name_is_kept_and_the_build_gets_a_free_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Flint-0.2.0.dmg"), "an older file").unwrap();
        let (base_url, served) = serve_once(file_answer(b"hello"));

        let path = download(
            &base_url,
            &hello_release(),
            dir.path(),
            &not_cancelled(),
            |_| {},
        )
        .unwrap();
        served.join().unwrap();

        assert_eq!(path, dir.path().join("Flint-0.2.0 (1).dmg"));
        assert_eq!(
            std::fs::read(dir.path().join("Flint-0.2.0.dmg")).unwrap(),
            b"an older file"
        );
    }

    fn assert_nothing_left(dir: &tempfile::TempDir) {
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn a_file_with_the_wrong_checksum_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let (base_url, served) = serve_once(file_answer(b"hellp"));

        let failed = download(
            &base_url,
            &hello_release(),
            dir.path(),
            &not_cancelled(),
            |_| {},
        );
        served.join().unwrap();

        assert!(
            matches!(failed, Err(UpdateError::WrongChecksum)),
            "{failed:?}"
        );
        assert_nothing_left(&dir);
    }

    #[test]
    fn a_content_length_that_disagrees_is_refused_before_anything_is_saved() {
        let dir = tempfile::tempdir().unwrap();
        let (base_url, served) = serve_once(file_answer(b"hello, and more"));

        let failed = download(
            &base_url,
            &hello_release(),
            dir.path(),
            &not_cancelled(),
            |_| {},
        );
        served.join().unwrap();

        assert!(matches!(failed, Err(UpdateError::WrongSize)), "{failed:?}");
        assert_nothing_left(&dir);
    }

    #[test]
    fn a_body_longer_than_announced_is_refused_even_without_a_length() {
        let dir = tempfile::tempdir().unwrap();
        // No Content-Length: the body runs until the server closes.
        let (base_url, served) = serve_once(http("200 OK", "", b"hello, and more"));

        let failed = download(
            &base_url,
            &hello_release(),
            dir.path(),
            &not_cancelled(),
            |_| {},
        );
        served.join().unwrap();

        assert!(matches!(failed, Err(UpdateError::WrongSize)), "{failed:?}");
        assert_nothing_left(&dir);
    }

    #[test]
    fn a_redirect_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let redirect = http(
            "302 Found",
            "Location: http://127.0.0.1:1/elsewhere\r\nContent-Length: 0\r\n",
            b"",
        );
        let (base_url, served) = serve_once(redirect);

        let failed = download(
            &base_url,
            &hello_release(),
            dir.path(),
            &not_cancelled(),
            |_| {},
        );
        served.join().unwrap();

        assert!(
            matches!(failed, Err(UpdateError::Status(302))),
            "{failed:?}"
        );
        assert_nothing_left(&dir);
    }

    #[test]
    fn a_build_that_is_too_large_or_badly_named_is_refused_without_a_request() {
        let dir = tempfile::tempdir().unwrap();
        let nowhere = "http://127.0.0.1:1/api/v1";

        let huge = Release {
            size_bytes: MAX_DOWNLOAD_BYTES + 1,
            ..hello_release()
        };
        let failed = download(nowhere, &huge, dir.path(), &not_cancelled(), |_| {});
        assert!(matches!(failed, Err(UpdateError::TooLarge)), "{failed:?}");

        let unnamed = Release {
            filename: "../".into(),
            ..hello_release()
        };
        let failed = download(nowhere, &unnamed, dir.path(), &not_cancelled(), |_| {});
        assert!(matches!(failed, Err(UpdateError::BadAnswer)), "{failed:?}");

        let elsewhere = Release {
            download_path: "//evil.example/x".into(),
            ..hello_release()
        };
        let failed = download(nowhere, &elsewhere, dir.path(), &not_cancelled(), |_| {});
        assert!(matches!(failed, Err(UpdateError::BadAnswer)), "{failed:?}");
    }

    #[test]
    fn a_download_cancelled_on_the_way_leaves_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let (base_url, served) = serve_once(file_answer(b"hello"));
        let is_cancelled = not_cancelled();

        // Cancelled when the first bytes have arrived.
        let failed = download(
            &base_url,
            &hello_release(),
            dir.path(),
            &is_cancelled,
            |_| is_cancelled.store(true, Ordering::Relaxed),
        );
        served.join().unwrap();

        assert!(matches!(failed, Err(UpdateError::Cancelled)), "{failed:?}");
        assert_nothing_left(&dir);
    }

    #[test]
    fn a_download_cancelled_before_it_starts_makes_no_request() {
        let dir = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base_url = format!("http://{}/api/v1", listener.local_addr().unwrap());

        let failed = download(
            &base_url,
            &hello_release(),
            dir.path(),
            &AtomicBool::new(true),
            |_| {},
        );

        assert!(matches!(failed, Err(UpdateError::Cancelled)), "{failed:?}");
        assert!(listener.accept().is_err(), "a request was made");
        assert_nothing_left(&dir);
    }

    #[test]
    fn a_cancel_after_a_wrong_file_stops_before_the_server_is_asked_again() {
        let dir = tempfile::tempdir().unwrap();
        let is_cancelled = std::sync::Arc::new(not_cancelled());
        let flag = is_cancelled.clone();
        // The file has another size, and the cancel comes while it is on its way.
        let (base_url, served) = serve_once_after(file_answer(b"hello, and more"), move || {
            flag.store(true, Ordering::Relaxed)
        });

        let done = download_offer(
            &base_url,
            "macos",
            &hello_release(),
            "0.1.0",
            dir.path(),
            &is_cancelled,
            |_, _| {},
        );
        served.join().unwrap();

        // Asking again would be a network error: nothing listens any more.
        assert!(matches!(done, Err(UpdateError::Cancelled)), "{done:?}");
        assert_nothing_left(&dir);
    }

    #[test]
    fn every_attempt_writes_to_a_part_file_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let first = part_path(dir.path(), "Flint-0.2.0.dmg");
        let second = part_path(dir.path(), "Flint-0.2.0.dmg");
        assert_ne!(first, second);
        assert_eq!(first.parent(), Some(dir.path()));
        assert!(first.to_string_lossy().ends_with(PART_SUFFIX));

        // The part file of another download that is still running, and a
        // file of the user's that only looks like a part file.
        std::fs::write(&first, "another download").unwrap();
        std::fs::write(dir.path().join("Flint-0.2.0.dmg.part"), "not ours").unwrap();
        let (base_url, served) = serve_once(file_answer(b"hellp"));

        let failed = download(
            &base_url,
            &hello_release(),
            dir.path(),
            &not_cancelled(),
            |_| {},
        );
        served.join().unwrap();

        assert!(
            matches!(failed, Err(UpdateError::WrongChecksum)),
            "{failed:?}"
        );
        assert_eq!(std::fs::read(&first).unwrap(), b"another download");
        assert_eq!(
            std::fs::read(dir.path().join("Flint-0.2.0.dmg.part")).unwrap(),
            b"not ours"
        );
        assert_eq!(files_in(&dir).len(), 2);
    }

    #[test]
    fn a_part_file_left_by_a_download_that_died_is_removed_by_the_next_one() {
        let dir = tempfile::tempdir().unwrap();
        let dead = part_path(dir.path(), "Flint-0.1.9.dmg");
        let running = part_path(dir.path(), "Flint-0.2.0.dmg");
        let other = dir.path().join("report.part");
        for path in [&dead, &running, &other] {
            std::fs::write(path, "half").unwrap();
        }
        let long_ago = SystemTime::now() - 2 * STALE_PART_AGE;
        for path in [&dead, &other] {
            File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(long_ago)
                .unwrap();
        }
        let (base_url, served) = serve_once(file_answer(b"hello"));

        download(
            &base_url,
            &hello_release(),
            dir.path(),
            &not_cancelled(),
            |_| {},
        )
        .unwrap();
        served.join().unwrap();

        assert!(!dead.exists());
        assert!(running.exists());
        assert!(other.exists());
    }

    #[test]
    fn only_a_later_version_is_an_offer() {
        let offer = |version: &str, current: &str| {
            let release = Release {
                version: version.into(),
                ..hello_release()
            };
            let (base_url, served) = serve_once(release_answer(&release));
            let offered = newer_release(&base_url, "macos", current).unwrap();
            served.join().unwrap();
            offered.map(|release| release.version)
        };

        assert_eq!(offer("0.2.0", "0.1.0").as_deref(), Some("0.2.0"));
        assert_eq!(offer("0.10.0", "0.9.9").as_deref(), Some("0.10.0"));
        assert_eq!(offer("0.1.0", "0.1.0"), None);
        assert_eq!(offer("0.0.9", "0.1.0"), None);
        // A version that does not parse is never an offer.
        for version in ["", "2.0", "2.0.0-beta", "v2.0.0", "latest"] {
            assert_eq!(offer(version, "0.1.0"), None, "{version:?}");
        }
    }

    type Saved = Result<(Release, PathBuf), UpdateError>;

    /// Fetch the offer "hello" from a server that gives `answers`, one for
    /// each request. Returns the first line of every request too.
    fn fetch_offer(
        answers: Vec<Vec<u8>>,
        current: &str,
    ) -> (Saved, Vec<String>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let (base_url, served) = serve_each(answers);
        let offered = hello_release();
        let done = download_offer(
            &base_url,
            "macos",
            &offered,
            current,
            dir.path(),
            &not_cancelled(),
            |_, _| {},
        );
        (done, served.join().unwrap(), dir)
    }

    const WORLD_SHA256: &str = "486ea46224d1bb4fb680f34f7c9ad96a8f24ec88be73ea8e5a6c65260e9cb8a7";

    fn world_release() -> Release {
        Release {
            version: "0.3.0".into(),
            filename: "Flint-0.3.0.dmg".into(),
            sha256: WORLD_SHA256.into(),
            ..hello_release()
        }
    }

    #[test]
    fn an_offer_replaced_on_the_server_is_asked_for_again_and_the_new_build_is_saved() {
        let answers = vec![
            file_answer(b"world"),
            release_answer(&world_release()),
            file_answer(b"world"),
        ];

        let (done, requests, dir) = fetch_offer(answers, "0.1.0");

        let (saved, path) = done.unwrap();
        assert_eq!(saved, world_release());
        assert_eq!(path, dir.path().join("Flint-0.3.0.dmg"));
        assert_eq!(std::fs::read(&path).unwrap(), b"world");
        assert_eq!(
            requests,
            [
                "GET /api/v1/downloads/macos HTTP/1.1",
                "GET /api/v1/releases/latest?platform=macos HTTP/1.1",
                "GET /api/v1/downloads/macos HTTP/1.1",
            ]
        );
    }

    #[test]
    fn a_broken_file_of_an_offer_that_still_stands_is_not_fetched_twice() {
        let answers = vec![file_answer(b"hellp"), release_answer(&hello_release())];

        let (done, _, dir) = fetch_offer(answers, "0.1.0");

        assert!(matches!(done, Err(UpdateError::WrongChecksum)), "{done:?}");
        assert_nothing_left(&dir);
    }

    #[test]
    fn an_offer_replaced_by_nothing_newer_is_withdrawn() {
        let no_build = http(
            "404 Not Found",
            "",
            br#"{"error":{"code":"no_build","message":"x"}}"#,
        );
        let (done, _, dir) = fetch_offer(vec![file_answer(b"world"), no_build], "0.1.0");
        assert!(matches!(done, Err(UpdateError::Withdrawn)), "{done:?}");
        assert_nothing_left(&dir);

        // The replacement is the version this app already is.
        let answers = vec![file_answer(b"world"), release_answer(&world_release())];
        let (done, _, dir) = fetch_offer(answers, "0.3.0");
        assert!(matches!(done, Err(UpdateError::Withdrawn)), "{done:?}");
        assert_nothing_left(&dir);

        // The build was deleted: the download itself says so.
        let (done, requests, dir) = fetch_offer(vec![http("404 Not Found", "", b"{}")], "0.1.0");
        assert!(matches!(done, Err(UpdateError::Withdrawn)), "{done:?}");
        assert_eq!(requests.len(), 1);
        assert_nothing_left(&dir);
    }

    #[test]
    fn an_offer_that_is_still_right_is_saved_with_one_request() {
        let (done, requests, dir) = fetch_offer(vec![file_answer(b"hello")], "0.1.0");

        let (saved, path) = done.unwrap();
        assert_eq!(saved, hello_release());
        assert_eq!(path, dir.path().join("Flint-0.2.0.dmg"));
        assert_eq!(requests.len(), 1);
    }

    fn files_in(dir: &tempfile::TempDir) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_saved_build_carries_the_quarantine_mark() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Flint-0.2.0.dmg");
        std::fs::write(&path, b"build").unwrap();

        mark_as_downloaded(&path);

        let mark = std::process::Command::new("/usr/bin/xattr")
            .args(["-p", "com.apple.quarantine"])
            .arg(&path)
            .output()
            .unwrap();
        let mark = String::from_utf8_lossy(&mark.stdout);
        assert!(
            mark.starts_with("0081;") && mark.contains(";Flint;"),
            "{mark}"
        );
    }
}
