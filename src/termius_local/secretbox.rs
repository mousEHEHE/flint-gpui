//! NaCl secretbox (XSalsa20-Poly1305) open, and the Termius field format on
//! top of it. Termius stores an encrypted field as base64 of
//! `[0x04][1 reserved byte][24-byte nonce][16-byte MAC][ciphertext]`, opened
//! with the per-install key. This is libsodium `crypto_secretbox_open_easy`,
//! whose MAC comes before the ciphertext.

use std::cell::Cell;

use base64::Engine as _;
use crypto_secretbox::aead::{Aead, KeyInit};
use crypto_secretbox::{Key, Nonce, XSalsa20Poly1305};
use zeroize::Zeroizing;

/// Open a secretbox given the 32-byte key, 24-byte nonce and the
/// `MAC || ciphertext` body (libsodium layout). Returns `None` on a bad key
/// length or a failed authentication.
pub fn open(key: &[u8], nonce: &[u8], mac_then_ciphertext: &[u8]) -> Option<Vec<u8>> {
    if key.len() != 32 || nonce.len() != 24 || mac_then_ciphertext.len() < 16 {
        return None;
    }
    // RustCrypto's AEAD expects the tag appended (`ciphertext || MAC`), while
    // NaCl puts the MAC first. Reorder before opening.
    let (mac, ciphertext) = mac_then_ciphertext.split_at(16);
    let mut reordered = Vec::with_capacity(mac_then_ciphertext.len());
    reordered.extend_from_slice(ciphertext);
    reordered.extend_from_slice(mac);

    let cipher = XSalsa20Poly1305::new(Key::from_slice(key));
    cipher
        .decrypt(Nonce::from_slice(nonce), reordered.as_slice())
        .ok()
}

/// Decrypts Termius fields with the per-install key and counts how many
/// encrypted fields opened and how many did not. A key that opens nothing
/// does not belong to this database.
pub struct Decryptor {
    key: Zeroizing<Vec<u8>>,
    opened: Cell<usize>,
    failed: Cell<usize>,
}

impl Decryptor {
    pub fn new(key: Zeroizing<Vec<u8>>) -> Self {
        Self {
            key,
            opened: Cell::new(0),
            failed: Cell::new(0),
        }
    }

    pub fn opened_count(&self) -> usize {
        self.opened.get()
    }

    pub fn failed_count(&self) -> usize {
        self.failed.get()
    }

    /// Plaintext fields pass through the same reader, so a value that is not
    /// an encrypted `BA…` string is returned unchanged. An encrypted field that
    /// does not open becomes empty: ciphertext must never be imported as a
    /// label, address or password.
    pub fn decrypt_field(&self, value: &str) -> String {
        let Some(raw) = encrypted_payload(value) else {
            return value.to_string();
        };
        let plaintext =
            open(&self.key, &raw[2..26], &raw[26..]).and_then(|p| String::from_utf8(p).ok());
        match plaintext {
            Some(text) => {
                self.opened.set(self.opened.get() + 1);
                text
            }
            None => {
                self.failed.set(self.failed.get() + 1);
                String::new()
            }
        }
    }
}

/// The raw bytes of an encrypted field:
/// `[0x04][reserved][nonce(24)][mac(16)][ciphertext]`.
fn encrypted_payload(value: &str) -> Option<Vec<u8>> {
    if value.len() < 40 || !value.starts_with("BA") {
        return None;
    }
    let raw = base64::engine::general_purpose::STANDARD
        .decode(pad_base64(value).as_ref())
        .ok()?;
    if raw.len() < 42 || raw[0] != 4 {
        return None;
    }
    Some(raw)
}

/// Termius drops base64 padding; restore it so the decoder accepts the string.
fn pad_base64(s: &str) -> std::borrow::Cow<'_, str> {
    match s.len() % 4 {
        0 => std::borrow::Cow::Borrowed(s),
        remainder => {
            let mut padded = String::with_capacity(s.len() + (4 - remainder));
            padded.push_str(s);
            for _ in 0..(4 - remainder) {
                padded.push('=');
            }
            std::borrow::Cow::Owned(padded)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crypto_secretbox::aead::{Aead, KeyInit};
    use crypto_secretbox::{Key, Nonce, XSalsa20Poly1305};

    /// Seal with the crate, convert to NaCl layout, and check `open` recovers
    /// the plaintext. This pins the MAC-first reordering against a vector the
    /// crate itself produced.
    #[test]
    fn open_round_trips_nacl_layout() {
        let key = [7u8; 32];
        let nonce = [9u8; 24];
        let plaintext = b"hunter2 the password";

        let cipher = XSalsa20Poly1305::new(Key::from_slice(&key));
        // crate output is `ciphertext || MAC`.
        let sealed = cipher
            .encrypt(Nonce::from_slice(&nonce), plaintext.as_slice())
            .unwrap();
        let (ciphertext, mac) = sealed.split_at(sealed.len() - 16);
        let mut nacl = Vec::new();
        nacl.extend_from_slice(mac);
        nacl.extend_from_slice(ciphertext);

        let opened = open(&key, &nonce, &nacl).unwrap();
        assert_eq!(opened, plaintext);
    }

    #[test]
    fn open_rejects_a_tampered_mac() {
        let key = [1u8; 32];
        let nonce = [2u8; 24];
        let mut body = vec![0u8; 32]; // 16 MAC + 16 ciphertext, all zero
        body[0] = 0xAB;
        assert!(open(&key, &nonce, &body).is_none());
    }

    /// Build a real Termius-style `BA…` field with the crate and decrypt it.
    #[test]
    fn decrypt_field_opens_a_ba_string() {
        let key = [5u8; 32];
        let nonce = [3u8; 24];
        let plaintext = b"deploy@example.com";

        let cipher = XSalsa20Poly1305::new(Key::from_slice(&key));
        let sealed = cipher
            .encrypt(Nonce::from_slice(&nonce), plaintext.as_slice())
            .unwrap();
        let (ciphertext, mac) = sealed.split_at(sealed.len() - 16);

        let mut raw = vec![0x04u8, 0x00];
        raw.extend_from_slice(&nonce);
        raw.extend_from_slice(mac);
        raw.extend_from_slice(ciphertext);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&raw);
        assert!(encoded.starts_with("BA"));

        let decryptor = Decryptor::new(Zeroizing::new(key.to_vec()));
        assert_eq!(decryptor.decrypt_field(&encoded), "deploy@example.com");
        assert_eq!(decryptor.opened_count(), 1);

        let wrong = Decryptor::new(Zeroizing::new(vec![6u8; 32]));
        assert_eq!(wrong.decrypt_field(&encoded), "");
        assert_eq!(wrong.failed_count(), 1);
        assert_eq!(wrong.opened_count(), 0);
    }

    #[test]
    fn decrypt_field_passes_plaintext_through() {
        let decryptor = Decryptor::new(Zeroizing::new(vec![0u8; 32]));
        assert_eq!(decryptor.decrypt_field("just a label"), "just a label");
        assert_eq!(decryptor.failed_count(), 0);
    }
}
