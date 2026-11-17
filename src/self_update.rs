//! Puts a downloaded build in place of the running app, and starts it again
//! once this one has quit. The zip was checked against the signed release
//! before it gets here.
//!
//! The new build is unpacked next to the app, so the swap is two renames on
//! one disk. Blocking: call from a worker thread.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context as _, Result};

const WORK_FOLDER_PREFIX: &str = ".flint-update-";
#[cfg(target_os = "macos")]
const TEAM_ID: &str = "S7H6BD59M7";

/// Unpack `zip` and swap it in for the installed app. Returns the path to
/// start after quitting.
pub fn install(zip: &Path) -> Result<PathBuf> {
    let current = installed_app()?;
    let folder = current.parent().context("the app is not in a folder")?;
    let work = folder.join(format!(
        "{WORK_FOLDER_PREFIX}{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir(&work)
        .with_context(|| format!("flint may not write to {}", folder.display()))?;
    let installed = unpack(zip, &work).and_then(|new| {
        check_unpacked(&new)?;
        swap_in(&new, &current, &backup_path(&current, &work))
    });
    // After a swap the folder holds the old app, which still runs until the
    // restart; the next start removes it.
    if installed.is_err() {
        if let Err(e) = std::fs::remove_dir_all(&work) {
            tracing::debug!(?e, work = %work.display(), "could not remove the update folder");
        }
    }
    installed.map(|()| current)
}

/// Start `app` as soon as this process has quit.
pub fn relaunch_after_quit(app: &Path) -> Result<()> {
    let pid = std::process::id().to_string();
    let mut command = relaunch_command(&pid, app);
    command.spawn().context("could not start the new version")?;
    Ok(())
}

/// What an update that was cut short left: its folder, and on Windows the
/// old exe, which cannot be deleted while it runs.
pub fn remove_leftovers() {
    let Ok(current) = installed_app() else {
        return;
    };
    let Some(entries) = current
        .parent()
        .and_then(|folder| std::fs::read_dir(folder).ok())
    else {
        return;
    };
    for entry in entries.flatten() {
        let is_ours = entry
            .file_name()
            .to_string_lossy()
            .starts_with(WORK_FOLDER_PREFIX);
        if is_ours {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
    #[cfg(windows)]
    let _ = std::fs::remove_file(backup_path(&current, Path::new("")));
}

/// Move `current` aside to `backup`, then `new` to where `current` was. When
/// the second move fails, the first is undone, so there is always an app.
fn swap_in(new: &Path, current: &Path, backup: &Path) -> Result<()> {
    if backup.exists() {
        remove_any(backup).context("could not remove the backup of an earlier update")?;
    }
    std::fs::rename(current, backup).context("could not move the running app aside")?;
    if let Err(e) = std::fs::rename(new, current) {
        if let Err(undo) = std::fs::rename(backup, current) {
            tracing::error!(?undo, backup = %backup.display(), "could not put the old app back");
        }
        return Err(e).context("could not move the new version in place");
    }
    tracing::info!(app = %current.display(), "new version installed");
    Ok(())
}

fn remove_any(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

fn run(command: &mut Command) -> Result<std::process::Output> {
    let output = command
        .output()
        .with_context(|| format!("could not run {command:?}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{command:?} failed: {}", stderr.trim());
    }
    Ok(output)
}

#[cfg(target_os = "macos")]
fn installed_app() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let bundle = exe
        .ancestors()
        .nth(3)
        .filter(|bundle| bundle.extension().is_some_and(|ext| ext == "app"))
        .context("flint is not running from an app bundle")?;
    // macOS runs an app from Downloads out of a read-only copy.
    if bundle.to_string_lossy().contains("/AppTranslocation/") {
        bail!("move flint to the Applications folder, then it can update itself");
    }
    Ok(bundle.to_path_buf())
}

#[cfg(windows)]
fn installed_app() -> Result<PathBuf> {
    Ok(std::env::current_exe()?)
}

#[cfg(not(any(target_os = "macos", windows)))]
fn installed_app() -> Result<PathBuf> {
    bail!("flint cannot update itself on this system")
}

#[cfg(target_os = "macos")]
fn backup_path(_current: &Path, work: &Path) -> PathBuf {
    work.join("old.app")
}

/// A running exe can be renamed but not deleted, so it waits next to the new
/// one until the next start.
#[cfg(not(target_os = "macos"))]
fn backup_path(current: &Path, _work: &Path) -> PathBuf {
    let mut name = current.as_os_str().to_owned();
    name.push(".old");
    name.into()
}

#[cfg(target_os = "macos")]
fn unpack(zip: &Path, work: &Path) -> Result<PathBuf> {
    // ditto keeps the code signature intact, unzip does not.
    run(Command::new("/usr/bin/ditto")
        .arg("-x")
        .arg("-k")
        .arg(zip)
        .arg(work))?;
    let apps: Vec<PathBuf> = std::fs::read_dir(work)?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "app"))
        .collect();
    match &apps[..] {
        [app] => Ok(app.clone()),
        _ => bail!("the download does not hold exactly one app"),
    }
}

#[cfg(windows)]
fn unpack(zip: &Path, work: &Path) -> Result<PathBuf> {
    let system_root = std::env::var_os("SystemRoot").context("no SystemRoot")?;
    let tar = Path::new(&system_root).join("System32").join("tar.exe");
    run(Command::new(tar).arg("-xf").arg(zip).arg("-C").arg(work))?;
    let exe = work.join("flint.exe");
    if !exe.is_file() {
        bail!("the download has no flint.exe");
    }
    // The license files go next to the app as they are. Only the exe needs the swap.
    let app = installed_app()?;
    let folder = app.parent().context("the app is not in a folder")?;
    for entry in std::fs::read_dir(work)?.flatten() {
        if entry.path() != exe {
            let _ = std::fs::rename(entry.path(), folder.join(entry.file_name()));
        }
    }
    Ok(exe)
}

#[cfg(not(any(target_os = "macos", windows)))]
fn unpack(_zip: &Path, _work: &Path) -> Result<PathBuf> {
    bail!("flint cannot update itself on this system")
}

/// The signature check already proved where the zip came from. This one
/// catches a build that is broken or was signed by someone else before it
/// replaces a working app.
#[cfg(target_os = "macos")]
fn check_unpacked(app: &Path) -> Result<()> {
    run(Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(app))?;
    let details = run(Command::new("/usr/bin/codesign").arg("-dv").arg(app))?;
    // codesign -d writes its report to stderr.
    let report = String::from_utf8_lossy(&details.stderr);
    if !report
        .lines()
        .any(|line| line == format!("TeamIdentifier={TEAM_ID}"))
    {
        bail!("the new version is not signed by the flint developer");
    }
    // Without this, the first start of the new version would ask whether to open
    // an app from the internet.
    let _ = Command::new("/usr/bin/xattr")
        .args(["-dr", "com.apple.quarantine"])
        .arg(app)
        .status();
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn check_unpacked(_new: &Path) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn relaunch_command(pid: &str, app: &Path) -> Command {
    let mut command = Command::new("/bin/sh");
    // The path goes in as an argument, so no quoting can break it.
    command
        .args([
            "-c",
            r#"while kill -0 "$1" 2>/dev/null; do sleep 0.2; done; exec /usr/bin/open "$2""#,
        ])
        .args(["flint-relaunch", pid])
        .arg(app);
    command
}

#[cfg(windows)]
fn relaunch_command(pid: &str, app: &Path) -> Command {
    use std::os::windows::process::CommandExt as _;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // In single quotes PowerShell takes everything as it is, except '' for '.
    let app = app.to_string_lossy().replace('\'', "''");
    let script = format!(
        "Wait-Process -Id {pid} -ErrorAction SilentlyContinue; Start-Process -FilePath '{app}'"
    );
    let mut command = Command::new("powershell");
    command
        .args(["-NoProfile", "-NonInteractive", "-Command", script.as_str()])
        .creation_flags(CREATE_NO_WINDOW);
    command
}

#[cfg(not(any(target_os = "macos", windows)))]
fn relaunch_command(_pid: &str, app: &Path) -> Command {
    Command::new(app)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_swap_puts_the_new_app_in_place_and_keeps_the_old_one_aside() {
        let dir = tempfile::tempdir().unwrap();
        let (new, current, backup) = (
            dir.path().join("new.app"),
            dir.path().join("Flint.app"),
            dir.path().join("old.app"),
        );
        std::fs::create_dir(&new).unwrap();
        std::fs::write(new.join("version"), "2").unwrap();
        std::fs::create_dir(&current).unwrap();
        std::fs::write(current.join("version"), "1").unwrap();
        // A backup left by an earlier update is replaced.
        std::fs::write(&backup, "stale").unwrap();

        swap_in(&new, &current, &backup).unwrap();

        assert_eq!(
            std::fs::read_to_string(current.join("version")).unwrap(),
            "2"
        );
        assert_eq!(
            std::fs::read_to_string(backup.join("version")).unwrap(),
            "1"
        );
        assert!(!new.exists());
    }

    #[test]
    fn a_failed_swap_leaves_the_old_app_where_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("flint.exe");
        std::fs::write(&current, "1").unwrap();
        let backup = dir.path().join("flint.exe.old");

        let missing = dir.path().join("not-there");
        assert!(swap_in(&missing, &current, &backup).is_err());

        assert_eq!(std::fs::read_to_string(&current).unwrap(), "1");
        assert!(!backup.exists());
    }
}
