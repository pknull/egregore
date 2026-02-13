use argon2::Argon2;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::error::{EgreError, Result};

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;

/// Encrypted key file format.
#[derive(Serialize, Deserialize)]
pub struct EncryptedKey {
    pub salt: Vec<u8>,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
}

/// Encrypt a 32-byte secret key with a passphrase using Argon2id + ChaCha20-Poly1305.
pub fn encrypt_key(secret_bytes: &[u8; 32], passphrase: &str) -> Result<EncryptedKey> {
    let mut salt = [0u8; SALT_LEN];
    rand::rngs::OsRng.fill_bytes(&mut salt);

    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);

    let derived = derive_key(passphrase, &salt)?;
    let cipher = ChaCha20Poly1305::new_from_slice(&derived)
        .map_err(|e| EgreError::Crypto { reason: e.to_string() })?;

    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, secret_bytes.as_ref())
        .map_err(|e| EgreError::Crypto { reason: e.to_string() })?;

    Ok(EncryptedKey {
        salt: salt.to_vec(),
        nonce: nonce_bytes.to_vec(),
        ciphertext,
    })
}

/// Decrypt a secret key from encrypted form using a passphrase.
pub fn decrypt_key(encrypted: &EncryptedKey, passphrase: &str) -> Result<[u8; 32]> {
    let derived = derive_key(passphrase, &encrypted.salt)?;
    let cipher = ChaCha20Poly1305::new_from_slice(&derived)
        .map_err(|e| EgreError::Crypto { reason: e.to_string() })?;

    let nonce = Nonce::from_slice(&encrypted.nonce);
    let plaintext = cipher
        .decrypt(nonce, encrypted.ciphertext.as_ref())
        .map_err(|_| EgreError::Crypto {
            reason: "decryption failed (wrong passphrase?)".into(),
        })?;

    plaintext.try_into().map_err(|_| EgreError::Crypto {
        reason: "decrypted key is not 32 bytes".into(),
    })
}

/// Save encrypted key to a JSON file.
pub fn save_encrypted(encrypted: &EncryptedKey, path: &std::path::Path) -> Result<()> {
    let json = serde_json::to_string_pretty(encrypted)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, json)?;
    Ok(())
}

/// Load encrypted key from a JSON file.
pub fn load_encrypted(path: &std::path::Path) -> Result<EncryptedKey> {
    let data = std::fs::read_to_string(path).map_err(|_| EgreError::IdentityNotFound {
        path: path.display().to_string(),
    })?;
    let encrypted: EncryptedKey = serde_json::from_str(&data)?;
    Ok(encrypted)
}

fn derive_key(passphrase: &str, salt: &[u8]) -> Result<[u8; 32]> {
    let mut output = [0u8; 32];
    Argon2::default()
        .hash_password_into(passphrase.as_bytes(), salt, &mut output)
        .map_err(|e| EgreError::Crypto {
            reason: format!("argon2 KDF failed: {e}"),
        })?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let secret = [42u8; 32];
        let passphrase = "test-passphrase-2026";

        let encrypted = encrypt_key(&secret, passphrase).unwrap();
        let decrypted = decrypt_key(&encrypted, passphrase).unwrap();

        assert_eq!(secret, decrypted);
    }

    #[test]
    fn wrong_passphrase_fails() {
        let secret = [42u8; 32];
        let encrypted = encrypt_key(&secret, "correct").unwrap();
        let result = decrypt_key(&encrypted, "wrong");
        assert!(result.is_err());
    }

    #[test]
    fn save_and_load_encrypted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("encrypted.json");

        let secret = [7u8; 32];
        let encrypted = encrypt_key(&secret, "passphrase").unwrap();
        save_encrypted(&encrypted, &path).unwrap();

        let loaded = load_encrypted(&path).unwrap();
        let decrypted = decrypt_key(&loaded, "passphrase").unwrap();
        assert_eq!(secret, decrypted);
    }

    #[test]
    fn different_salts_produce_different_ciphertexts() {
        let secret = [1u8; 32];
        let e1 = encrypt_key(&secret, "same").unwrap();
        let e2 = encrypt_key(&secret, "same").unwrap();
        // Random salt means ciphertexts differ
        assert_ne!(e1.ciphertext, e2.ciphertext);
    }
}
