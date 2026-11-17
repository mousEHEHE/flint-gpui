//! Signs a build for the update server. flint installs only builds signed
//! with this key, so keep the key file safe and keep a copy of it: without it
//! no installed copy of flint can update itself.
//!
//!   cargo run --example sign_release -- keygen
//!   cargo run --example sign_release -- sign <platform> <version> <file>
//!
//! The key is ~/.config/flint-release/update-signing.key, or $FLINT_RELEASE_KEY.

use std::io::Read as _;
use std::path::PathBuf;

use anyhow::{bail, Context as _, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use ed25519_dalek::{Signer as _, SigningKey};
use sha2::{Digest as _, Sha256};

#[path = "../src/release_message.rs"]
mod release_message;

fn key_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("FLINT_RELEASE_KEY") {
        return Ok(path.into());
    }
    let home = dirs::home_dir().context("no home folder")?;
    Ok(home.join(".config/flint-release/update-signing.key"))
}

fn keygen() -> Result<()> {
    let path = key_path()?;
    let mut seed = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut seed)?;
    std::fs::create_dir_all(path.parent().context("no folder for the key")?)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(&path)
        .with_context(|| format!("{} exists already or cannot be made", path.display()))?;
    std::io::Write::write_all(&mut file, STANDARD.encode(seed).as_bytes())?;
    let public = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
    println!("key saved to {}", path.display());
    println!("public key for src/update.rs: {public:?}");
    Ok(())
}

fn sign(platform: &str, version: &str, file: &str) -> Result<()> {
    let path = key_path()?;
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("no key at {}", path.display()))?;
    let seed: [u8; 32] = STANDARD
        .decode(text.trim())?
        .try_into()
        .map_err(|_| anyhow::anyhow!("the key file does not hold a 32-byte key"))?;
    let key = SigningKey::from_bytes(&seed);

    let bytes = std::fs::read(file).with_context(|| format!("cannot read {file}"))?;
    let sha256: String = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let message = release_message::release_message(platform, version, &sha256);
    let signature = key.sign(message.as_bytes());
    key.verifying_key()
        .verify_strict(message.as_bytes(), &signature)?;
    println!("{}", STANDARD.encode(signature.to_bytes()));
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["keygen"] => keygen(),
        ["sign", platform @ ("macos" | "windows"), version, file] => sign(platform, version, file),
        _ => bail!("usage: sign_release keygen | sign <macos|windows> <version> <file>"),
    }
}
