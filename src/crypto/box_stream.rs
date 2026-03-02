//! Box Stream — encrypted framing over an authenticated connection.
//!
//! After SHS completes, all data flows through Box Stream frames. Each frame
//! is independently encrypted with XChaCha20-Poly1305 using per-direction keys.
//!
//! Frame format:
//!   [encrypted_header(34 bytes) | encrypted_body(len bytes)]
//!
//! The header contains body length + body MAC, encrypted separately. This lets
//! the reader know exactly how many bytes to expect before attempting body
//! decryption — no length oracle. Nonces increment per operation (header and
//! body consume separate slots).
//!
//! A goodbye frame (all-zero header plaintext) signals clean stream termination.
//! Body size capped at 4096 bytes; larger payloads are split by the application
//! layer (see gossip/replication.rs).
//!
//! # Cipher Choice
//!
//! We use XChaCha20-Poly1305 (24-byte nonce) rather than ChaCha20-Poly1305
//! (12-byte nonce) to match the original SSB Box Stream design which uses
//! XSalsa20-Poly1305. This allows direct use of the full 24-byte nonce
//! derived from ephemeral keys without truncation.
//!
//! References:
//! - SSB Protocol Guide: https://ssbc.github.io/scuttlebutt-protocol-guide/
//! - XChaCha20 IETF Draft: https://datatracker.ietf.org/doc/html/draft-irtf-cfrg-xchacha-03
//! - Libsodium XChaCha20: https://libsodium.gitbook.io/doc/secret-key_cryptography/aead/chacha20-poly1305/xchacha20-poly1305_construction

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

use crate::error::{EgreError, Result};

const TAG_SIZE: usize = 16;
const HEADER_PLAIN_SIZE: usize = 2 + TAG_SIZE; // body_len(2) + body_mac(16)
const HEADER_ENCRYPTED_SIZE: usize = HEADER_PLAIN_SIZE + TAG_SIZE; // + header_mac
const MAX_BODY_SIZE: usize = 4096;

/// Encrypts frames for sending over a Box Stream.
pub struct BoxStreamWriter {
    cipher: XChaCha20Poly1305,
    nonce: [u8; 24],
}

/// Decrypts frames received from a Box Stream.
pub struct BoxStreamReader {
    cipher: XChaCha20Poly1305,
    nonce: [u8; 24],
}

impl BoxStreamWriter {
    pub fn new(key: [u8; 32], nonce: [u8; 24]) -> Self {
        let cipher = XChaCha20Poly1305::new_from_slice(&key).expect("valid key length");
        Self { cipher, nonce }
    }

    /// Encrypt a body into a box stream frame.
    /// Returns: [encrypted_header(34) | encrypted_body(len + 16)]
    ///
    /// Per SSB Box Stream spec, nonces are assigned: header gets N, body gets N+1.
    /// We must encrypt body first to compute its MAC (needed for header), but
    /// we pre-allocate both nonces to maintain correct ordering.
    pub fn encrypt_frame(&mut self, body: &[u8]) -> Result<Vec<u8>> {
        if body.is_empty() {
            return Err(EgreError::Crypto {
                reason: "empty body".into(),
            });
        }
        if body.len() > MAX_BODY_SIZE {
            return Err(EgreError::Crypto {
                reason: format!("body too large: {} > {MAX_BODY_SIZE}", body.len()),
            });
        }

        // Pre-allocate nonces in protocol order: header N, body N+1
        let header_nonce = self.next_nonce();
        let body_nonce = self.next_nonce();

        // Encrypt body first to get the body MAC (needed for header)
        let body_ct = self
            .cipher
            .encrypt(XNonce::from_slice(&body_nonce), body)
            .map_err(|e| EgreError::Crypto {
                reason: e.to_string(),
            })?;

        // body_ct = body_ciphertext(len) + body_mac(16)
        let body_mac = &body_ct[body_ct.len() - TAG_SIZE..];
        let body_len = (body.len() as u16).to_be_bytes();

        // Header plaintext: [body_len(2) | body_mac(16)]
        let mut header_plain = Vec::with_capacity(HEADER_PLAIN_SIZE);
        header_plain.extend_from_slice(&body_len);
        header_plain.extend_from_slice(body_mac);

        // Encrypt header with its pre-allocated nonce
        let header_ct = self
            .cipher
            .encrypt(XNonce::from_slice(&header_nonce), header_plain.as_slice())
            .map_err(|e| EgreError::Crypto {
                reason: e.to_string(),
            })?;

        // Frame: [encrypted_header | body_ciphertext_without_mac]
        let mut frame = Vec::with_capacity(header_ct.len() + body_ct.len() - TAG_SIZE);
        frame.extend_from_slice(&header_ct);
        frame.extend_from_slice(&body_ct[..body_ct.len() - TAG_SIZE]);
        Ok(frame)
    }

    /// Create a goodbye frame (zero-length, signals stream end).
    pub fn goodbye(&mut self) -> Result<Vec<u8>> {
        let nonce = self.next_nonce();
        let zeros = [0u8; HEADER_PLAIN_SIZE];
        let ct = self
            .cipher
            .encrypt(XNonce::from_slice(&nonce), zeros.as_ref())
            .map_err(|e| EgreError::Crypto {
                reason: e.to_string(),
            })?;
        Ok(ct)
    }

    fn next_nonce(&mut self) -> [u8; 24] {
        let current = self.nonce;
        increment_nonce(&mut self.nonce);
        current
    }
}

impl BoxStreamReader {
    pub fn new(key: [u8; 32], nonce: [u8; 24]) -> Self {
        let cipher = XChaCha20Poly1305::new_from_slice(&key).expect("valid key length");
        Self { cipher, nonce }
    }

    /// Decrypt a frame header to get the body length.
    /// Returns None if this is a goodbye frame.
    /// Returns Some(body_len, body_mac) on success.
    pub fn decrypt_header(
        &mut self,
        header_ct: &[u8; HEADER_ENCRYPTED_SIZE],
    ) -> Result<Option<(u16, [u8; TAG_SIZE])>> {
        let nonce = self.next_nonce();
        let header_plain = self
            .cipher
            .decrypt(XNonce::from_slice(&nonce), header_ct.as_ref())
            .map_err(|_| EgreError::Crypto {
                reason: "header decryption failed".into(),
            })?;

        // Check for goodbye
        if header_plain.iter().all(|&b| b == 0) {
            return Ok(None);
        }

        let body_len = u16::from_be_bytes([header_plain[0], header_plain[1]]);
        let mut body_mac = [0u8; TAG_SIZE];
        body_mac.copy_from_slice(&header_plain[2..]);

        Ok(Some((body_len, body_mac)))
    }

    /// Decrypt the body using the MAC from the header.
    pub fn decrypt_body(&mut self, body_ct: &[u8], body_mac: &[u8; TAG_SIZE]) -> Result<Vec<u8>> {
        let nonce = self.next_nonce();

        // Reconstruct full ciphertext with MAC appended
        let mut full_ct = Vec::with_capacity(body_ct.len() + TAG_SIZE);
        full_ct.extend_from_slice(body_ct);
        full_ct.extend_from_slice(body_mac);

        self.cipher
            .decrypt(XNonce::from_slice(&nonce), full_ct.as_slice())
            .map_err(|_| EgreError::Crypto {
                reason: "body decryption failed".into(),
            })
    }

    fn next_nonce(&mut self) -> [u8; 24] {
        let current = self.nonce;
        increment_nonce(&mut self.nonce);
        current
    }
}

/// Increment a 24-byte nonce (big-endian).
fn increment_nonce(nonce: &mut [u8; 24]) {
    for byte in nonce.iter_mut().rev() {
        let (val, overflow) = byte.overflowing_add(1);
        *byte = val;
        if !overflow {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_keys() -> ([u8; 32], [u8; 24], [u8; 32], [u8; 24]) {
        // Client encrypt = Server decrypt, and vice versa
        let c2s_key = [1u8; 32];
        let c2s_nonce = [2u8; 24];
        let s2c_key = [3u8; 32];
        let s2c_nonce = [4u8; 24];
        (c2s_key, c2s_nonce, s2c_key, s2c_nonce)
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let (c2s_key, c2s_nonce, _, _) = test_keys();
        let mut writer = BoxStreamWriter::new(c2s_key, c2s_nonce);
        let mut reader = BoxStreamReader::new(c2s_key, c2s_nonce);

        let plaintext = b"hello egregore network";
        let frame = writer.encrypt_frame(plaintext).unwrap();

        // Split frame into header and body
        let header: [u8; HEADER_ENCRYPTED_SIZE] =
            frame[..HEADER_ENCRYPTED_SIZE].try_into().unwrap();
        let body_ct = &frame[HEADER_ENCRYPTED_SIZE..];

        let (body_len, body_mac) = reader.decrypt_header(&header).unwrap().unwrap();
        assert_eq!(body_len as usize, plaintext.len());

        let decrypted = reader.decrypt_body(body_ct, &body_mac).unwrap();
        assert_eq!(&decrypted, plaintext);
    }

    #[test]
    fn multiple_frames() {
        let (key, nonce, _, _) = test_keys();
        let mut writer = BoxStreamWriter::new(key, nonce);
        let mut reader = BoxStreamReader::new(key, nonce);

        for i in 0..10 {
            let msg = format!("message number {i}");
            let frame = writer.encrypt_frame(msg.as_bytes()).unwrap();

            let header: [u8; HEADER_ENCRYPTED_SIZE] =
                frame[..HEADER_ENCRYPTED_SIZE].try_into().unwrap();
            let body_ct = &frame[HEADER_ENCRYPTED_SIZE..];

            let (_, body_mac) = reader.decrypt_header(&header).unwrap().unwrap();
            let decrypted = reader.decrypt_body(body_ct, &body_mac).unwrap();
            assert_eq!(decrypted, msg.as_bytes());
        }
    }

    #[test]
    fn goodbye_frame() {
        let (key, nonce, _, _) = test_keys();
        let mut writer = BoxStreamWriter::new(key, nonce);
        let mut reader = BoxStreamReader::new(key, nonce);

        let frame = writer.goodbye().unwrap();
        let header: [u8; HEADER_ENCRYPTED_SIZE] =
            frame[..HEADER_ENCRYPTED_SIZE].try_into().unwrap();
        let result = reader.decrypt_header(&header).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn wrong_key_fails() {
        let (key, nonce, _, _) = test_keys();
        let mut writer = BoxStreamWriter::new(key, nonce);
        let mut reader = BoxStreamReader::new([99u8; 32], nonce);

        let frame = writer.encrypt_frame(b"secret").unwrap();
        let header: [u8; HEADER_ENCRYPTED_SIZE] =
            frame[..HEADER_ENCRYPTED_SIZE].try_into().unwrap();
        let result = reader.decrypt_header(&header);
        assert!(result.is_err());
    }

    #[test]
    fn body_too_large_rejected() {
        let (key, nonce, _, _) = test_keys();
        let mut writer = BoxStreamWriter::new(key, nonce);
        let big = vec![0u8; MAX_BODY_SIZE + 1];
        let result = writer.encrypt_frame(&big);
        assert!(result.is_err());
    }

    #[test]
    fn nonce_increments() {
        let mut nonce = [0u8; 24];
        nonce[23] = 254;
        increment_nonce(&mut nonce);
        assert_eq!(nonce[23], 255);
        increment_nonce(&mut nonce);
        assert_eq!(nonce[23], 0);
        assert_eq!(nonce[22], 1);
    }

    /// Regression test for nonce reuse vulnerability (issue #68).
    ///
    /// The original implementation used ChaCha20-Poly1305 (12-byte nonce) but
    /// stored 24-byte nonces. It truncated to bytes [0..12] while incrementing
    /// bytes [12..24], resulting in static effective nonces.
    ///
    /// This test verifies that encrypting the same plaintext twice produces
    /// different ciphertexts, which proves unique nonces are being used.
    #[test]
    fn nonce_uniqueness_regression() {
        let (key, nonce, _, _) = test_keys();
        let mut writer = BoxStreamWriter::new(key, nonce);

        let plaintext = b"identical message";

        // Encrypt same plaintext twice
        let frame1 = writer.encrypt_frame(plaintext).unwrap();
        let frame2 = writer.encrypt_frame(plaintext).unwrap();

        // With proper nonce handling, identical plaintexts MUST produce
        // different ciphertexts. If they're equal, nonces are being reused.
        assert_ne!(
            frame1, frame2,
            "CRITICAL: identical plaintexts produced identical ciphertexts - nonce reuse detected"
        );
    }

    /// Stress test for nonce uniqueness across many operations.
    #[test]
    fn nonce_uniqueness_stress() {
        use std::collections::HashSet;

        let (key, nonce, _, _) = test_keys();
        let mut writer = BoxStreamWriter::new(key, nonce);

        let mut seen_frames: HashSet<Vec<u8>> = HashSet::new();
        let plaintext = b"repeated payload";

        // Encrypt 1000 identical messages
        for i in 0..1000 {
            let frame = writer.encrypt_frame(plaintext).unwrap();
            assert!(
                seen_frames.insert(frame),
                "Nonce reuse detected at frame {i}"
            );
        }
    }
}
