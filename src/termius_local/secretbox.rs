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
/// `MAC || ciphertext` body (libsodium layout, which `crypto_secretbox` uses
/// too). Returns `None` on a bad key length or a failed authentication.
pub fn open(key: &[u8], nonce: &[u8], mac_then_ciphertext: &[u8]) -> Option<Vec<u8>> {
    if key.len() != 32 || nonce.len() != 24 || mac_then_ciphertext.len() < 16 {
        return None;
    }
    XSalsa20Poly1305::new(Key::from_slice(key))
        .decrypt(Nonce::from_slice(nonce), mac_then_ciphertext)
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

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    /// The secretbox test vector from NaCl and libsodium, in their
    /// `MAC || ciphertext` layout. Termius writes fields with libsodium.
    #[test]
    fn open_reads_the_libsodium_test_vector() {
        let key = hex("1b27556473e985d462cd51197a9a46c76009549eac6474f206c4ee0844f68389");
        let nonce = hex("69696ee955b62b73cd62bda875fc73d68219e0036b7a0b37");
        let message = hex(concat!(
            "be075fc53c81f2d5cf141316ebeb0c7b5228c52a4c62cbd44b66849b64244ffc",
            "e5ecbaaf33bd751a1ac728d45e6c61296cdc3c01233561f41db66cce314adb31",
            "0e3be8250c46f06dceea3a7fa1348057e2f6556ad6b1318a024a838f21af1fde",
            "048977eb48f59ffd4924ca1c60902e52f0a089bc76897040e082f93776384864",
            "5e0705",
        ));
        let boxed = hex(concat!(
            "f3ffc7703f9400e52a7dfb4b3d3305d98e993b9f48681273c29650ba32fc76ce",
            "48332ea7164d96a4476fb8c531a1186ac0dfc17c98dce87b4da7f011ec48c972",
            "71d2c20f9b928fe2270d6fb863d51738b48eeee314a7cc8ab932164548e526ae",
            "90224368517acfeabd6bb3732bc0e9da99832b61ca01b6de56244a9e88d5f9b3",
            "7973f622a43d14a6599b1f654cb45a74e355a5",
        ));
        assert_eq!(open(&key, &nonce, &boxed), Some(message));
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
        let mut raw = vec![0x04u8, 0x01];
        raw.extend_from_slice(&nonce);
        raw.extend_from_slice(&sealed);
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
