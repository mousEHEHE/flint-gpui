use std::path::Path;
#[cfg(target_os = "windows")]
use std::path::PathBuf;

use crate::file_associations::FileAssociations;

#[derive(Debug)]
pub enum OpenError {
    /// The OS has no app for this file type, so the caller shows the app
    /// picker. Nothing produces it on platforms without an OS open command.
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    NoHandler,
    /// I/O or process spawn failure unrelated to a missing handler.
    Io(std::io::Error),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::NoHandler => write!(f, "no application registered for this file type"),
            OpenError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for OpenError {}

/// The key under which an app association is remembered for `file`.
///
/// Normally the lowercase extension (e.g. `"yml"`). Files with no extension
/// (`Dockerfile`, `Makefile`, `.gitignore`) fall back to their lowercase file
/// name so they can still be associated and opened. Returns `None` only when
/// the path has no final component at all.
pub fn association_key(file: &Path) -> Option<String> {
    if let Some(ext) = file.extension().and_then(|s| s.to_str()) {
        if !ext.is_empty() {
            return Some(ext.to_lowercase());
        }
    }
    file.file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
}

pub fn open_with_association_or_default(
    file: &Path,
    associations: &FileAssociations,
) -> Result<(), OpenError> {
    if let Some(key) = association_key(file) {
        if let Some(app) = associations.get(&key) {
            tracing::debug!(target: "flint::file_edit", key, app = %app.display(), "opening via association");
            return open_in_specific_app(app, file);
        }
    }
    tracing::debug!(target: "flint::file_edit", file = %file.display(), "opening via OS default");
    open_in_default_app(file)
}

/// Drop the `\\?\` verbatim prefix before handing a path to another program.
///
/// Cache paths carry it so our own long-path file I/O works (see
/// `sftp_cache_path`), but the shell and many editors mishandle such a path.
#[cfg(target_os = "windows")]
fn without_verbatim_prefix(path: &Path) -> PathBuf {
    let text = path.as_os_str().to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => path.to_path_buf(),
    }
}

pub fn open_in_specific_app(app: &Path, file: &Path) -> Result<(), OpenError> {
    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("open")
            .arg("-a")
            .arg(app)
            .arg(file)
            .status()
            .map_err(OpenError::Io)?;
        if status.success() {
            Ok(())
        } else {
            Err(OpenError::Io(std::io::Error::other(format!(
                "`open -a` exited with {status}"
            ))))
        }
    }

    #[cfg(target_os = "windows")]
    {
        // Not waited for: the editor runs for as long as the user keeps it
        // open, and this is called on the main thread.
        std::process::Command::new(app)
            .arg(without_verbatim_prefix(file))
            .spawn()
            .map(drop)
            .map_err(OpenError::Io)
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (app, file);
        Err(OpenError::Io(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "platform not supported",
        )))
    }
}

pub fn open_in_default_app(path: &Path) -> Result<(), OpenError> {
    // A missing file must not look like a missing app.
    std::fs::metadata(path).map_err(OpenError::Io)?;

    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("open")
            .arg(path)
            .status()
            .map_err(OpenError::Io)?;
        if status.success() {
            Ok(())
        } else {
            // `open` exits 1 when no application is registered for the file type.
            Err(OpenError::NoHandler)
        }
    }

    #[cfg(target_os = "windows")]
    {
        shell_open(&without_verbatim_prefix(path))
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = path;
        Err(OpenError::Io(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "platform not supported",
        )))
    }
}

/// Open `path` with the app Windows has for its type.
///
/// This must not go through `cmd /C start`: the path ends with a file name
/// that comes from the server, and `cmd` would run `&`, `|` and `%VAR%` in it.
#[cfg(target_os = "windows")]
fn shell_open(path: &Path) -> Result<(), OpenError> {
    use std::os::windows::ffi::OsStrExt as _;

    // Declared here because the `windows` crate is built without its shell
    // feature.
    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteW(
            window: *mut std::ffi::c_void,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show_command: i32,
        ) -> isize;
    }
    const SW_SHOWNORMAL: i32 = 1;
    const SE_ERR_NOASSOC: isize = 31;

    let file: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `file` is a NUL-terminated UTF-16 string that outlives the
    // call, and every other pointer may be null.
    let code = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            std::ptr::null(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW reports success as any value above 32.
    if code > 32 {
        return Ok(());
    }
    if code == SE_ERR_NOASSOC {
        return Err(OpenError::NoHandler);
    }
    Err(OpenError::Io(std::io::Error::other(format!(
        "ShellExecuteW failed with code {code}"
    ))))
}

/// Show `path` selected in Finder or Explorer. Opens nothing.
pub fn reveal_in_file_manager(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg("-R")
            .arg(path)
            .status()
            .map(drop)
    }

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt as _;
        // Explorer reads its own command line: the path needs quotes of its
        // own after the comma, which the usual argument quoting does not give.
        // Its exit code says nothing, it is 1 even when the folder opened.
        std::process::Command::new("explorer")
            .raw_arg(format!(
                "/select,\"{}\"",
                without_verbatim_prefix(path).display()
            ))
            .status()
            .map(drop)
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = path;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "platform not supported",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::association_key;
    use std::path::Path;

    #[test]
    fn association_key_uses_lowercased_extension() {
        assert_eq!(
            association_key(Path::new("/srv/app.YML")).as_deref(),
            Some("yml")
        );
        assert_eq!(
            association_key(Path::new("/srv/app.tar.gz")).as_deref(),
            Some("gz")
        );
    }

    // Cache paths are built with a `\\?\` prefix; the shell and most editors
    // reject it, so it has to come off at the process boundary.
    #[cfg(target_os = "windows")]
    #[test]
    fn verbatim_prefix_is_stripped_for_external_programs() {
        use super::without_verbatim_prefix;
        assert_eq!(
            without_verbatim_prefix(Path::new(r"\\?\C:\cache\Flint\app.yml")),
            Path::new(r"C:\cache\Flint\app.yml")
        );
        assert_eq!(
            without_verbatim_prefix(Path::new(r"\\?\UNC\srv\share\app.yml")),
            Path::new(r"\\srv\share\app.yml")
        );
        // A plain path is handed over untouched.
        assert_eq!(
            without_verbatim_prefix(Path::new(r"C:\Program Files\App\app.exe")),
            Path::new(r"C:\Program Files\App\app.exe")
        );
    }

    #[test]
    fn a_missing_file_is_an_io_error_and_not_a_missing_app() {
        let directory = tempfile::tempdir().unwrap();
        let result = super::open_in_default_app(&directory.path().join("gone.txt"));
        assert!(matches!(result, Err(super::OpenError::Io(_))));
    }

    #[test]
    fn association_key_falls_back_to_file_name() {
        // A file without an extension must still get a key.
        assert_eq!(
            association_key(Path::new("/srv/Dockerfile")).as_deref(),
            Some("dockerfile")
        );
        assert_eq!(
            association_key(Path::new("/srv/Makefile")).as_deref(),
            Some("makefile")
        );
        // Dotfiles have no extension; the whole name is the key.
        assert_eq!(
            association_key(Path::new("/srv/.gitignore")).as_deref(),
            Some(".gitignore")
        );
    }
}
