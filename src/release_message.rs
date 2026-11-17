//! What the release key signs. `examples/sign_release.rs` includes this file,
//! so the signer and the app cannot disagree about it.

/// The file is checked against the SHA-256, so signing this covers the file
/// too. The version is in it, so an old build cannot be offered as a new one.
pub fn release_message(platform: &str, version: &str, sha256: &str) -> String {
    format!(
        "flint-release\n{platform}\n{version}\n{}\n",
        sha256.to_ascii_lowercase()
    )
}
