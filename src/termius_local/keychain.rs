//! Read Termius's per-install secretbox key from Credential Manager. Termius
//! uses keytar, which stores the key as a generic credential under the target
//! `Termius/localKey` (keytar joins service and account with a slash). The
//! stored value is base64 of exactly 32 bytes.

use anyhow::{bail, Context as _, Result};
use base64::Engine as _;
use zeroize::Zeroizing;

const SERVICE: &str = "Termius";
const ACCOUNT: &str = "localKey";

/// Read and decode the 32-byte key. The returned error is safe to show.
pub fn read_local_key() -> Result<Zeroizing<Vec<u8>>> {
    let raw = read_raw_key()?;
    let text = std::str::from_utf8(&raw).context("key was not UTF-8")?;
    decode_key(text.trim())
}

fn decode_key(encoded: &str) -> Result<Zeroizing<Vec<u8>>> {
    let key = Zeroizing::new(
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .context("key is not valid base64")?,
    );
    if key.len() != 32 {
        bail!("unexpected key length {} (expected 32)", key.len());
    }
    Ok(key)
}

/// The blob is the base64 string as raw bytes.
fn read_raw_key() -> Result<Zeroizing<Vec<u8>>> {
    use windows::core::PCWSTR;
    use windows::Win32::Security::Credentials::{
        CredFree, CredReadW, CREDENTIALW, CRED_TYPE_GENERIC,
    };

    let target: Vec<u16> = format!("{SERVICE}/{ACCOUNT}")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let mut credential: *mut CREDENTIALW = std::ptr::null_mut();
        CredReadW(
            PCWSTR(target.as_ptr()),
            CRED_TYPE_GENERIC,
            0,
            &mut credential,
        )
        .context("no Termius key in Credential Manager")?;

        let blob_size = (*credential).CredentialBlobSize as usize;
        let blob_ptr = (*credential).CredentialBlob;
        // An empty blob has a null pointer, which from_raw_parts must not get.
        let bytes = if blob_ptr.is_null() || blob_size == 0 {
            Zeroizing::new(Vec::new())
        } else {
            Zeroizing::new(std::slice::from_raw_parts(blob_ptr, blob_size).to_vec())
        };
        CredFree(credential as *const _);
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_32_byte_base64_key() {
        let raw = [3u8; 32];
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        assert_eq!(decode_key(&encoded).unwrap().as_slice(), raw);
    }

    #[test]
    fn rejects_a_key_of_the_wrong_length() {
        let encoded = base64::engine::general_purpose::STANDARD.encode([1u8; 16]);
        assert!(decode_key(&encoded).is_err());
    }
}
