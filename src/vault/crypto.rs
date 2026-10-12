use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    XChaCha20Poly1305, XNonce,
};
use rand::{rngs::OsRng, RngCore};
use zeroize::Zeroizing;

use crate::vault::{
    error::VaultError,
    format::{AeadBlob, KdfParams},
};

pub(crate) fn generate_salt() -> [u8; 32] {
    let mut salt = [0u8; 32];
    OsRng.fill_bytes(&mut salt);
    salt
}

pub(crate) fn generate_dek() -> Zeroizing<[u8; 32]> {
    let mut dek = [0u8; 32];
    OsRng.fill_bytes(&mut dek);
    Zeroizing::new(dek)
}

/// Derives a 32-byte key from `password` with Argon2id.
///
/// `Corrupted` here means the KDF params themselves are invalid, not that the
/// password is wrong.
pub(crate) fn derive_kek(
    password: &[u8],
    salt: &[u8; 32],
    params: &KdfParams,
) -> Result<Zeroizing<[u8; 32]>, VaultError> {
    let argon2_params = Params::new(params.m_cost, params.t_cost, params.p_cost, Some(32))
        .map_err(|e| VaultError::Corrupted(format!("invalid KDF params: {e}")))?;

    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, argon2_params);

    let mut kek = Zeroizing::new([0u8; 32]);
    argon2
        .hash_password_into(password, salt, kek.as_mut())
        .map_err(|e| VaultError::Corrupted(format!("KDF failed: {e}")))?;

    Ok(kek)
}

/// Derives a 32-byte key for one purpose from `key` with HKDF-SHA256.
/// `purpose` keeps keys made for different jobs apart.
pub(crate) fn derive_subkey(key: &[u8; 32], purpose: &str) -> Zeroizing<[u8; 32]> {
    let mut subkey = Zeroizing::new([0u8; 32]);
    hkdf::Hkdf::<sha2::Sha256>::new(None, key)
        .expand(purpose.as_bytes(), subkey.as_mut())
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    subkey
}

/// Encrypts `plaintext` with XChaCha20-Poly1305 under `key`.
/// A fresh random 24-byte nonce is generated for every call.
pub(crate) fn encrypt(key: &[u8; 32], plaintext: &[u8]) -> AeadBlob {
    seal(key, &[], plaintext)
}

/// Like [`encrypt`], and ties the result to `context`: [`open`] fails unless
/// it is given the same bytes. `context` itself is not stored in the blob.
pub(crate) fn seal(key: &[u8; 32], context: &[u8], plaintext: &[u8]) -> AeadBlob {
    let cipher = XChaCha20Poly1305::new(key.into());

    let mut nonce_bytes = [0u8; 24];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = XNonce::from(nonce_bytes);

    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad: context,
            },
        )
        .expect("XChaCha20Poly1305 encryption is infallible for valid keys");

    AeadBlob {
        nonce: nonce_bytes,
        ciphertext,
    }
}

/// Decrypts `blob` with XChaCha20-Poly1305 under `key`.
///
/// Any authentication failure is `Corrupted`. Callers that unwrap a DEK map it
/// to `InvalidPassword` or `InvalidRecoveryPhrase`.
pub(crate) fn decrypt(key: &[u8; 32], blob: &AeadBlob) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    open(key, &[], blob)
}

/// Decrypts a blob made by [`seal`] with the same `context`.
pub(crate) fn open(
    key: &[u8; 32],
    context: &[u8],
    blob: &AeadBlob,
) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let nonce = XNonce::from(blob.nonce);

    let plaintext = cipher
        .decrypt(
            &nonce,
            Payload {
                msg: blob.ciphertext.as_ref(),
                aad: context,
            },
        )
        .map_err(|_| VaultError::Corrupted("AEAD authentication failed".into()))?;

    Ok(Zeroizing::new(plaintext))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cheap KDF params so the tests run fast.
    fn test_kdf_params() -> KdfParams {
        KdfParams {
            m_cost: 256,
            t_cost: 1,
            p_cost: 1,
        }
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = [0u8; 32];
        let plaintext = b"vault secret data";
        let blob = encrypt(&key, plaintext);
        let result = decrypt(&key, &blob).unwrap();
        assert_eq!(result.as_slice(), plaintext);
    }

    #[test]
    fn a_sealed_blob_opens_only_with_its_context() {
        let key = [7u8; 32];
        let blob = seal(&key, b"item-1", b"secret");
        assert_eq!(open(&key, b"item-1", &blob).unwrap().as_slice(), b"secret");
        assert!(open(&key, b"item-2", &blob).is_err());
    }

    #[test]
    fn subkeys_differ_by_purpose() {
        let key = [7u8; 32];
        assert_ne!(*derive_subkey(&key, "a"), *derive_subkey(&key, "b"));
        assert_eq!(*derive_subkey(&key, "a"), *derive_subkey(&key, "a"));
    }

    #[test]
    fn encrypt_produces_fresh_nonces() {
        let key = [0u8; 32];
        let blob1 = encrypt(&key, b"same");
        let blob2 = encrypt(&key, b"same");
        assert_ne!(blob1.nonce, blob2.nonce);
        // Ciphertexts also differ because nonces differ
        assert_ne!(blob1.ciphertext, blob2.ciphertext);
    }

    #[test]
    fn wrong_key_returns_corrupted() {
        let key = [1u8; 32];
        let wrong_key = [2u8; 32];
        let blob = encrypt(&key, b"secret");
        let err = decrypt(&wrong_key, &blob).unwrap_err();
        assert!(matches!(err, VaultError::Corrupted(_)));
    }

    #[test]
    fn tampered_ciphertext_returns_corrupted() {
        let key = [0u8; 32];
        let mut blob = encrypt(&key, b"data");
        blob.ciphertext[0] ^= 0xff;
        let err = decrypt(&key, &blob).unwrap_err();
        assert!(matches!(err, VaultError::Corrupted(_)));
    }

    #[test]
    fn tampered_nonce_returns_corrupted() {
        let key = [0u8; 32];
        let mut blob = encrypt(&key, b"data");
        blob.nonce[0] ^= 0xff;
        let err = decrypt(&key, &blob).unwrap_err();
        assert!(matches!(err, VaultError::Corrupted(_)));
    }

    #[test]
    fn kdf_is_deterministic() {
        let password = b"test-password";
        let salt = [42u8; 32];
        let params = test_kdf_params();
        let kek1 = derive_kek(password, &salt, &params).unwrap();
        let kek2 = derive_kek(password, &salt, &params).unwrap();
        assert_eq!(*kek1, *kek2);
    }

    #[test]
    fn different_passwords_yield_different_keks() {
        let salt = [0u8; 32];
        let params = test_kdf_params();
        let kek1 = derive_kek(b"password-one", &salt, &params).unwrap();
        let kek2 = derive_kek(b"password-two", &salt, &params).unwrap();
        assert_ne!(*kek1, *kek2);
    }

    #[test]
    fn different_salts_yield_different_keks() {
        let params = test_kdf_params();
        let kek1 = derive_kek(b"password", &[0u8; 32], &params).unwrap();
        let kek2 = derive_kek(b"password", &[1u8; 32], &params).unwrap();
        assert_ne!(*kek1, *kek2);
    }

    #[test]
    fn generate_dek_is_random() {
        let d1 = generate_dek();
        let d2 = generate_dek();
        assert_ne!(*d1, *d2);
    }

    #[test]
    fn generate_salt_is_random() {
        let s1 = generate_salt();
        let s2 = generate_salt();
        assert_ne!(s1, s2);
    }
}
