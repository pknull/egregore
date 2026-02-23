//! Private Box — multi-recipient message encryption.
//!
//! Encrypts a message so only named recipients can read it. DH(sender, recipient)
//! encrypts a random symmetric key per recipient; the body is encrypted once with
//! that symmetric key.
//!
//! Wire format:
//!   [nonce(12) | recipient_count(1) | per_recipient_key(48 each) | encrypted_body]
//!
//! Recipients try-decrypt each key slot; success identifies their entry.
//! Non-recipients get `Ok(None)`. Recipient count is visible; identities are not.
//! Max 255 recipients per message.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use rand::RngCore;
use x25519_dalek::PublicKey as X25519Public;

use crate::error::{EgreError, Result};
use crate::identity::Identity;

const SYMMETRIC_KEY_SIZE: usize = 32;
const NONCE_SIZE: usize = 12;
const RECIPIENT_ENTRY_SIZE: usize = 32 + 16; // encrypted_key(32) + mac(16)

/// Encrypt a message for multiple recipients using private box.
///
/// Format:
/// [nonce(12) | recipient_count(1) | encrypted_symmetric_key_per_recipient(48 each) | encrypted_body]
///
/// Each recipient's entry: ChaCha20Poly1305(DH_shared_secret, symmetric_key)
/// Body: ChaCha20Poly1305(symmetric_key, plaintext)
pub fn box_message(
    sender: &Identity,
    recipients: &[X25519Public],
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    if recipients.is_empty() {
        return Err(EgreError::Crypto {
            reason: "no recipients".into(),
        });
    }
    if recipients.len() > 255 {
        return Err(EgreError::Crypto {
            reason: "too many recipients (max 255)".into(),
        });
    }

    // Generate random symmetric key and nonce
    let mut sym_key = [0u8; SYMMETRIC_KEY_SIZE];
    rand::rngs::OsRng.fill_bytes(&mut sym_key);

    let mut nonce_bytes = [0u8; NONCE_SIZE];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from(nonce_bytes);

    // Encrypt body with symmetric key
    let body_cipher = ChaCha20Poly1305::new_from_slice(&sym_key)
        .map_err(|e| EgreError::Crypto { reason: e.to_string() })?;
    let encrypted_body = body_cipher
        .encrypt(&nonce, plaintext)
        .map_err(|e| EgreError::Crypto { reason: e.to_string() })?;

    // For each recipient, encrypt the symmetric key with DH(sender, recipient)
    let sender_secret = sender.to_x25519_static_secret();
    let mut recipient_entries = Vec::with_capacity(recipients.len() * RECIPIENT_ENTRY_SIZE);

    for recipient_pk in recipients {
        let shared = sender_secret.diffie_hellman(recipient_pk);
        let shared_key = sha256(shared.as_bytes());

        let entry_cipher = ChaCha20Poly1305::new_from_slice(&shared_key)
            .map_err(|e| EgreError::Crypto { reason: e.to_string() })?;
        let encrypted_sym_key = entry_cipher
            .encrypt(&nonce, sym_key.as_ref())
            .map_err(|e| EgreError::Crypto { reason: e.to_string() })?;

        recipient_entries.extend_from_slice(&encrypted_sym_key);
    }

    // Assemble: [nonce | count | entries | body]
    let mut output =
        Vec::with_capacity(NONCE_SIZE + 1 + recipient_entries.len() + encrypted_body.len());
    output.extend_from_slice(&nonce_bytes);
    output.push(recipients.len() as u8);
    output.extend_from_slice(&recipient_entries);
    output.extend_from_slice(&encrypted_body);

    Ok(output)
}

/// Try to unbox a private message. Returns Ok(Some(plaintext)) if we're a recipient,
/// Ok(None) if not, or Err on corruption.
pub fn unbox_message(
    recipient: &Identity,
    sender_x25519_pk: &X25519Public,
    ciphertext: &[u8],
) -> Result<Option<Vec<u8>>> {
    if ciphertext.len() < NONCE_SIZE + 1 {
        return Err(EgreError::Crypto {
            reason: "ciphertext too short".into(),
        });
    }

    let nonce_bytes: [u8; NONCE_SIZE] = ciphertext[..NONCE_SIZE].try_into().unwrap();
    let nonce = Nonce::from(nonce_bytes);
    let recipient_count = ciphertext[NONCE_SIZE] as usize;

    let entries_start = NONCE_SIZE + 1;
    let entries_size = recipient_count * RECIPIENT_ENTRY_SIZE;
    let body_start = entries_start + entries_size;

    if ciphertext.len() < body_start {
        return Err(EgreError::Crypto {
            reason: "ciphertext truncated".into(),
        });
    }

    // Compute our shared secret with the sender
    let our_secret = recipient.to_x25519_static_secret();
    let shared = our_secret.diffie_hellman(sender_x25519_pk);
    let shared_key = sha256(shared.as_bytes());

    let entry_cipher = ChaCha20Poly1305::new_from_slice(&shared_key)
        .map_err(|e| EgreError::Crypto { reason: e.to_string() })?;

    // Try each recipient slot
    for i in 0..recipient_count {
        let entry_start = entries_start + i * RECIPIENT_ENTRY_SIZE;
        let entry = &ciphertext[entry_start..entry_start + RECIPIENT_ENTRY_SIZE];

        if let Ok(sym_key_bytes) = entry_cipher.decrypt(&nonce, entry) {
            // Found our slot — decrypt body
            let sym_key: [u8; 32] = sym_key_bytes.try_into().map_err(|_| EgreError::Crypto {
                reason: "decrypted key wrong size".into(),
            })?;

            let body_cipher = ChaCha20Poly1305::new_from_slice(&sym_key)
                .map_err(|e| EgreError::Crypto { reason: e.to_string() })?;
            let plaintext = body_cipher
                .decrypt(&nonce, &ciphertext[body_start..])
                .map_err(|_| EgreError::Crypto {
                    reason: "body decryption failed".into(),
                })?;

            return Ok(Some(plaintext));
        }
    }

    // Not a recipient
    Ok(None)
}

fn sha256(data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_unbox_single_recipient() {
        let sender = Identity::generate();
        let recipient = Identity::generate();

        let recipient_x25519 = recipient.to_x25519_public_key();
        let sender_x25519 = sender.to_x25519_public_key();

        let plaintext = b"secret knowledge for you only";
        let boxed = box_message(&sender, &[recipient_x25519], plaintext).unwrap();

        let result = unbox_message(&recipient, &sender_x25519, &boxed).unwrap();
        assert_eq!(result.as_deref(), Some(plaintext.as_ref()));
    }

    #[test]
    fn box_unbox_multiple_recipients() {
        let sender = Identity::generate();
        let r1 = Identity::generate();
        let r2 = Identity::generate();
        let r3 = Identity::generate();

        let recipients = vec![
            r1.to_x25519_public_key(),
            r2.to_x25519_public_key(),
            r3.to_x25519_public_key(),
        ];
        let sender_pk = sender.to_x25519_public_key();

        let plaintext = b"shared secret";
        let boxed = box_message(&sender, &recipients, plaintext).unwrap();

        // All recipients can decrypt
        for r in [&r1, &r2, &r3] {
            let result = unbox_message(r, &sender_pk, &boxed).unwrap();
            assert_eq!(result.as_deref(), Some(plaintext.as_ref()));
        }
    }

    #[test]
    fn non_recipient_gets_none() {
        let sender = Identity::generate();
        let recipient = Identity::generate();
        let outsider = Identity::generate();

        let boxed = box_message(
            &sender,
            &[recipient.to_x25519_public_key()],
            b"private",
        )
        .unwrap();

        let result = unbox_message(&outsider, &sender.to_x25519_public_key(), &boxed).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn empty_recipients_rejected() {
        let sender = Identity::generate();
        let result = box_message(&sender, &[], b"data");
        assert!(result.is_err());
    }

    #[test]
    fn corrupted_ciphertext_fails() {
        let sender = Identity::generate();
        let recipient = Identity::generate();

        let mut boxed = box_message(
            &sender,
            &[recipient.to_x25519_public_key()],
            b"secret",
        )
        .unwrap();

        // Corrupt a byte
        let last = boxed.len() - 1;
        boxed[last] ^= 0xFF;

        let result = unbox_message(
            &recipient,
            &sender.to_x25519_public_key(),
            &boxed,
        );
        // Should fail or return None
        assert!(result.is_err() || result.unwrap().is_none());
    }
}
