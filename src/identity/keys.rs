//! Node identity — Ed25519 keypair with X25519 conversion for DH.
//!
//! Each node has one long-term Ed25519 identity used for signing feed messages
//! and authenticating gossip connections (SHS). The same key converts to X25519
//! for Diffie-Hellman in SHS and Private Box (via birational map, matching
//! libsodium's crypto_sign_ed25519_sk_to_curve25519).
//!
//! Public IDs use SSB wire format: `@<base64-pubkey>.ed25519` (53 chars).
//! Key storage: raw 32-byte file (`secret.key`) or Argon2id-encrypted JSON
//! (`secret.key.enc`). See `encryption.rs` for the encrypted variant.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use ed25519_dalek::{SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::error::{EgreError, Result};

/// Full identity with private key (local only).
pub struct Identity {
    pub signing_key: SigningKey,
}

impl Clone for Identity {
    fn clone(&self) -> Self {
        Self {
            signing_key: SigningKey::from_bytes(&self.signing_key.to_bytes()),
        }
    }
}

/// Public identity for wire format: `@<base64-pubkey>.ed25519`
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PublicId(pub String);

impl Identity {
    /// Generate a new random identity.
    pub fn generate() -> Self {
        let signing_key = SigningKey::generate(&mut OsRng);
        Self { signing_key }
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }

    pub fn public_id(&self) -> PublicId {
        PublicId::from_verifying_key(&self.verifying_key())
    }

    /// Save private key bytes to file (unencrypted).
    pub fn save_unencrypted(&self, path: &Path) -> Result<()> {
        let dir = path.parent().ok_or_else(|| EgreError::Config {
            reason: "invalid key path".into(),
        })?;
        std::fs::create_dir_all(dir)?;
        std::fs::write(path, self.signing_key.to_bytes())?;
        Ok(())
    }

    /// Load private key from unencrypted file.
    pub fn load_unencrypted(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).map_err(|_| EgreError::IdentityNotFound {
            path: path.display().to_string(),
        })?;
        let key_bytes: [u8; 32] =
            bytes.try_into().map_err(|_| EgreError::InvalidKeypair {
                reason: "expected 32 bytes".into(),
            })?;
        let signing_key = SigningKey::from_bytes(&key_bytes);
        Ok(Self { signing_key })
    }

    /// Load or generate identity at the given directory.
    pub fn load_or_generate(identity_dir: &Path) -> Result<Self> {
        let key_path = identity_dir.join("secret.key");
        if key_path.exists() {
            Self::load_unencrypted(&key_path)
        } else {
            let identity = Self::generate();
            identity.save_unencrypted(&key_path)?;
            // Also save public key for convenience
            let pub_path = identity_dir.join("public.key");
            std::fs::write(pub_path, identity.public_id().0.as_bytes())?;
            Ok(identity)
        }
    }

    /// Get the raw secret key bytes (for Curve25519 conversion, etc.).
    pub fn secret_bytes(&self) -> [u8; 32] {
        self.signing_key.to_bytes()
    }

    /// Convert Ed25519 signing key to X25519 static secret for DH.
    /// Uses SHA-512 of the secret key (first 32 bytes), clamped — matches libsodium conversion.
    pub fn to_x25519_static_secret(&self) -> x25519_dalek::StaticSecret {
        use sha2::{Digest, Sha512};
        let mut hasher = Sha512::new();
        hasher.update(self.secret_bytes());
        let hash = hasher.finalize();
        let mut scalar_bytes = [0u8; 32];
        scalar_bytes.copy_from_slice(&hash[..32]);
        // Clamp
        scalar_bytes[0] &= 248;
        scalar_bytes[31] &= 127;
        scalar_bytes[31] |= 64;
        x25519_dalek::StaticSecret::from(scalar_bytes)
    }

    /// Get the X25519 public key derived from this identity.
    pub fn to_x25519_public_key(&self) -> x25519_dalek::PublicKey {
        x25519_dalek::PublicKey::from(&self.to_x25519_static_secret())
    }
}

impl PublicId {
    /// Validate that a string is a well-formed public ID (`@<base64-32-bytes>.ed25519`).
    /// Checks prefix, suffix, length (53 chars), and valid base64. Does not verify the
    /// key is cryptographically valid (i.e., a point on the curve).
    pub fn is_valid_format(s: &str) -> bool {
        // @<44 base64 chars>.ed25519 = 1 + 44 + 8 = 53
        if s.len() != 53 || !s.starts_with('@') || !s.ends_with(".ed25519") {
            return false;
        }
        let b64_part = &s[1..45];
        B64.decode(b64_part).is_ok_and(|bytes| bytes.len() == 32)
    }

    /// Create from a verifying (public) key.
    pub fn from_verifying_key(vk: &VerifyingKey) -> Self {
        let encoded = B64.encode(vk.as_bytes());
        Self(format!("@{}.ed25519", encoded))
    }

    /// Parse the base64 public key bytes from the wire format.
    pub fn to_verifying_key(&self) -> Result<VerifyingKey> {
        let inner = self
            .0
            .strip_prefix('@')
            .and_then(|s| s.strip_suffix(".ed25519"))
            .ok_or_else(|| EgreError::InvalidKeypair {
                reason: format!("invalid public ID format: {}", self.0),
            })?;
        let bytes = B64.decode(inner).map_err(|e| EgreError::InvalidKeypair {
            reason: format!("base64 decode failed: {e}"),
        })?;
        let key_bytes: [u8; 32] =
            bytes.try_into().map_err(|_| EgreError::InvalidKeypair {
                reason: "expected 32 bytes after decode".into(),
            })?;
        VerifyingKey::from_bytes(&key_bytes).map_err(|e| EgreError::InvalidKeypair {
            reason: format!("invalid ed25519 public key: {e}"),
        })
    }

    /// Convert public Ed25519 key to X25519 public key for DH.
    /// Uses the birational map from Ed25519 to Curve25519 (montgomery form).
    pub fn to_x25519_public_key(&self) -> Result<x25519_dalek::PublicKey> {
        let vk = self.to_verifying_key()?;
        let ed_point = curve25519_dalek::edwards::CompressedEdwardsY(vk.to_bytes());
        let ed_point = ed_point.decompress().ok_or_else(|| EgreError::InvalidKeypair {
            reason: "failed to decompress Ed25519 point".into(),
        })?;
        let montgomery = ed_point.to_montgomery();
        Ok(x25519_dalek::PublicKey::from(montgomery.to_bytes()))
    }
}

impl std::fmt::Display for PublicId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_and_roundtrip_public_id() {
        let identity = Identity::generate();
        let pub_id = identity.public_id();

        assert!(pub_id.0.starts_with('@'));
        assert!(pub_id.0.ends_with(".ed25519"));

        let vk = pub_id.to_verifying_key().unwrap();
        assert_eq!(vk, identity.verifying_key());
    }

    #[test]
    fn save_and_load_unencrypted() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("secret.key");

        let identity = Identity::generate();
        identity.save_unencrypted(&key_path).unwrap();

        let loaded = Identity::load_unencrypted(&key_path).unwrap();
        assert_eq!(
            identity.verifying_key(),
            loaded.verifying_key()
        );
    }

    #[test]
    fn load_or_generate_creates_new() {
        let dir = tempfile::tempdir().unwrap();
        let identity_dir = dir.path().join("identity");

        let id1 = Identity::load_or_generate(&identity_dir).unwrap();
        let id2 = Identity::load_or_generate(&identity_dir).unwrap();

        // Same key loaded both times
        assert_eq!(id1.verifying_key(), id2.verifying_key());
    }

    #[test]
    fn x25519_conversion_deterministic() {
        let identity = Identity::generate();
        let x1 = identity.to_x25519_public_key();
        let x2 = identity.to_x25519_public_key();
        assert_eq!(x1.as_bytes(), x2.as_bytes());
    }

    #[test]
    fn x25519_dh_agreement() {
        let alice = Identity::generate();
        let bob = Identity::generate();

        let alice_secret = alice.to_x25519_static_secret();
        let bob_secret = bob.to_x25519_static_secret();

        let alice_pub = alice.to_x25519_public_key();
        let bob_pub = bob.to_x25519_public_key();

        let shared_a = alice_secret.diffie_hellman(&bob_pub);
        let shared_b = bob_secret.diffie_hellman(&alice_pub);

        assert_eq!(shared_a.as_bytes(), shared_b.as_bytes());
    }

    #[test]
    fn invalid_public_id_format() {
        let bad = PublicId("not-a-valid-id".to_string());
        assert!(bad.to_verifying_key().is_err());
    }
}
