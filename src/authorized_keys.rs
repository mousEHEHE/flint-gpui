//! Adds a public key to `~/.ssh/authorized_keys` on a host, over a session
//! that is already signed in, as `ssh-copy-id` does. Termius calls this
//! "Export to host".

/// What the script reports on its last line.
const ADDED: &str = "flint-key-added";
const ALREADY_THERE: &str = "flint-key-present";

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Added,
    AlreadyThere,
}

/// A `sh` script that adds `public_key` ("<type> <base64>") with `comment`,
/// unless the key is there already. `None` when the key does not look like
/// one, so nothing odd is ever put into the script.
pub fn install_script(public_key: &str, comment: &str) -> Option<String> {
    let (key_type, blob) = public_key.split_once(' ')?;
    let is_type = !key_type.is_empty()
        && key_type
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'@'));
    let is_blob = !blob.is_empty()
        && blob
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='));
    if !is_type || !is_blob {
        return None;
    }
    let line = format!("{key_type} {blob} {}", safe_comment(comment));
    // umask 077 gives a new ~/.ssh and file the modes sshd insists on; existing
    // ones keep theirs. A file that does not end in a newline gets one first.
    Some(format!(
        r#"exec 2>&1
umask 077
cd || exit 1
mkdir -p .ssh && touch .ssh/authorized_keys || exit 1
if grep -v '^[[:space:]]*#' .ssh/authorized_keys | grep -qF "{blob}"; then echo {ALREADY_THERE}; exit 0; fi
if [ -s .ssh/authorized_keys ] && [ -n "$(tail -c 1 .ssh/authorized_keys)" ]; then echo >> .ssh/authorized_keys; fi
echo "{line}" >> .ssh/authorized_keys || exit 1
if type restorecon >/dev/null 2>&1; then restorecon -F .ssh .ssh/authorized_keys; fi
echo {ADDED}
"#
    ))
}

/// Letters, digits and `._@-` only, so the comment cannot break the script.
fn safe_comment(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .take(60)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if cleaned.trim_matches('-').is_empty() {
        "flint".to_string()
    } else {
        cleaned
    }
}

/// The script's answer. The error is what the host printed, for the status line.
pub fn outcome(output: Option<&str>) -> Result<Outcome, String> {
    let Some(output) = output else {
        return Err("the host did not answer in time".to_string());
    };
    let lines: Vec<&str> = output.lines().map(str::trim).collect();
    if lines.contains(&ADDED) {
        return Ok(Outcome::Added);
    }
    if lines.contains(&ALREADY_THERE) {
        return Ok(Outcome::AlreadyThere);
    }
    let said = lines.iter().rev().find(|line| !line.is_empty());
    Err(said.map_or(
        "the host could not run the script (is it a Unix shell?)".to_string(),
        |line| line.to_string(),
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    const KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOu3VmXXxcCNVvVML3jZYbM5jSzmZs/oN0xWUE6pseW/";

    /// Runs the script with `sh` in a home folder of its own.
    fn run(home: &std::path::Path, script: &str) -> String {
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .env("HOME", home)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn mode(path: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_key_is_added_once_with_the_modes_sshd_wants() {
        let home = tempfile::tempdir().unwrap();
        let script = install_script(KEY, "laptop").unwrap();

        assert_eq!(
            outcome(Some(&run(home.path(), &script))),
            Ok(Outcome::Added)
        );
        assert_eq!(
            outcome(Some(&run(home.path(), &script))),
            Ok(Outcome::AlreadyThere)
        );

        let file = home.path().join(".ssh/authorized_keys");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            format!("{KEY} laptop\n")
        );
        assert_eq!(mode(&home.path().join(".ssh")), 0o700);
        assert_eq!(mode(&file), 0o600);
    }

    #[test]
    fn the_command_the_app_sends_does_the_same() {
        // What actually goes to the host: the script, base64 into sh.
        let home = tempfile::tempdir().unwrap();
        let command = crate::server_stats::command_for(&install_script(KEY, "laptop").unwrap());
        assert_eq!(
            outcome(Some(&run(home.path(), &command))),
            Ok(Outcome::Added)
        );
        let file = home.path().join(".ssh/authorized_keys");
        assert_eq!(
            std::fs::read_to_string(file).unwrap(),
            format!("{KEY} laptop\n")
        );
    }

    #[test]
    fn existing_keys_stay_and_a_missing_last_newline_is_added() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".ssh")).unwrap();
        let file = home.path().join(".ssh/authorized_keys");
        std::fs::write(&file, "ssh-rsa AAAAold old-key").unwrap();

        let script = install_script(KEY, "laptop").unwrap();
        assert_eq!(
            outcome(Some(&run(home.path(), &script))),
            Ok(Outcome::Added)
        );

        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            format!("ssh-rsa AAAAold old-key\n{KEY} laptop\n")
        );
    }

    #[test]
    fn a_commented_out_key_is_added_again() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".ssh")).unwrap();
        let file = home.path().join(".ssh/authorized_keys");
        std::fs::write(&file, format!("  # {KEY} disabled\n")).unwrap();

        let script = install_script(KEY, "laptop").unwrap();
        assert_eq!(
            outcome(Some(&run(home.path(), &script))),
            Ok(Outcome::Added)
        );
    }

    #[test]
    fn a_key_with_options_or_another_comment_counts_as_there() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".ssh")).unwrap();
        let file = home.path().join(".ssh/authorized_keys");
        std::fs::write(&file, format!("from=\"10.0.0.1\" {KEY} other\n")).unwrap();

        let script = install_script(KEY, "laptop").unwrap();
        assert_eq!(
            outcome(Some(&run(home.path(), &script))),
            Ok(Outcome::AlreadyThere)
        );
    }

    #[test]
    fn nothing_odd_gets_into_the_script() {
        assert!(install_script("ssh-ed25519", "x").is_none());
        assert!(install_script("ssh-ed25519 AAAA\"; rm -rf ~; \"", "x").is_none());
        assert!(install_script("ssh;ed25519 AAAA", "x").is_none());
        assert_eq!(safe_comment("My key \"$(reboot)\""), "My-key----reboot--");
        assert_eq!(safe_comment("  "), "flint");
        assert_eq!(safe_comment("ключ"), "flint");
    }

    #[test]
    fn a_failure_reports_what_the_host_said() {
        assert_eq!(
            outcome(Some("touch: .ssh/authorized_keys: Permission denied\n")),
            Err("touch: .ssh/authorized_keys: Permission denied".to_string())
        );
        assert!(outcome(None).is_err());
        assert!(outcome(Some("")).unwrap_err().contains("Unix shell"));
    }
}
