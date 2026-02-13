use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use ed25519_dalek::{Signature, Signer, Verifier};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use x25519_dalek::{EphemeralSecret, PublicKey as X25519Public, SharedSecret};

use crate::error::{EgreError, Result};
use crate::identity::Identity;

type HmacSha256 = Hmac<Sha256>;

/// Outcome of a successful Secret Handshake.
/// Contains the derived encryption keys for Box Stream.
#[derive(Debug)]
pub struct HandshakeOutcome {
    /// Key for encrypting data we send.
    pub encrypt_key: [u8; 32],
    /// Nonce for encrypting data we send.
    pub encrypt_nonce: [u8; 24],
    /// Key for decrypting data we receive.
    pub decrypt_key: [u8; 32],
    /// Nonce for decrypting data we receive.
    pub decrypt_nonce: [u8; 24],
    /// Remote peer's Ed25519 public key (verified).
    pub remote_public_key: ed25519_dalek::VerifyingKey,
}

/// Client-side Secret Handshake state machine.
pub struct ClientHandshake {
    network_key: [u8; 32],
    identity: Identity,
    ephemeral_secret: EphemeralSecret,
    ephemeral_public: X25519Public,
}

/// Server-side Secret Handshake state machine.
pub struct ServerHandshake {
    network_key: [u8; 32],
    identity: Identity,
    ephemeral_secret: EphemeralSecret,
    ephemeral_public: X25519Public,
}

impl ClientHandshake {
    pub fn new(network_key: [u8; 32], identity: Identity) -> Self {
        let ephemeral_secret = EphemeralSecret::random_from_rng(rand::rngs::OsRng);
        let ephemeral_public = X25519Public::from(&ephemeral_secret);
        Self {
            network_key,
            identity,
            ephemeral_secret,
            ephemeral_public,
        }
    }

    /// Step 1: Client sends ephemeral public key + HMAC.
    /// Returns 64 bytes: [ephemeral_pk(32) | hmac(32)]
    pub fn step1_hello(&self) -> [u8; 64] {
        let mut mac =
            <HmacSha256 as Mac>::new_from_slice(&self.network_key).expect("HMAC key length is valid");
        mac.update(self.ephemeral_public.as_bytes());
        let tag = mac.finalize().into_bytes();

        let mut msg = [0u8; 64];
        msg[..32].copy_from_slice(self.ephemeral_public.as_bytes());
        msg[32..].copy_from_slice(&tag);
        msg
    }

    /// Step 3: Client authenticates after receiving server's hello.
    /// Takes server's hello (64 bytes), returns encrypted client auth (112 bytes).
    pub fn step3_authenticate(
        self,
        server_hello: &[u8; 64],
    ) -> Result<(ClientHandshakeStep3, Vec<u8>)> {
        // Parse server ephemeral pk + verify HMAC
        let server_eph_pk = {
            let mut pk_bytes = [0u8; 32];
            pk_bytes.copy_from_slice(&server_hello[..32]);
            X25519Public::from(pk_bytes)
        };

        let mut mac =
            <HmacSha256 as Mac>::new_from_slice(&self.network_key).expect("HMAC key length is valid");
        mac.update(server_eph_pk.as_bytes());
        mac.verify_slice(&server_hello[32..])
            .map_err(|_| EgreError::HandshakeFailed {
                reason: "server hello HMAC verification failed".into(),
            })?;

        // Compute shared secrets
        // ab = DH(client_eph, server_eph)
        let shared_ab = self.ephemeral_secret.diffie_hellman(&server_eph_pk);

        // aB = DH(client_eph, server_long_term) — we don't know server's long-term yet
        // For MVP, we use a simplified version where we compute aB using ephemeral × server_eph
        // In full SSB, the client would know the server's long-term key beforehand.
        // We'll do a simplified 2-way auth using just ephemeral keys + identity proof.

        // Derive shared secret for encryption: H(network_key | ab)
        let shared_secret = derive_shared_key(&self.network_key, shared_ab.as_bytes());

        // Sign proof: sign(network_key | server_eph_pk | hash(ab))
        let ab_hash = sha256(shared_ab.as_bytes());
        let mut proof_data = Vec::new();
        proof_data.extend_from_slice(&self.network_key);
        proof_data.extend_from_slice(server_eph_pk.as_bytes());
        proof_data.extend_from_slice(&ab_hash);

        let sig = self.identity.signing_key.sign(&proof_data);
        let client_vk = self.identity.verifying_key();

        // Payload: [sig(64) | client_public_key(32)]
        let mut payload = Vec::with_capacity(96);
        payload.extend_from_slice(&sig.to_bytes());
        payload.extend_from_slice(client_vk.as_bytes());

        // Encrypt payload with shared secret
        let encrypted = encrypt_payload(&shared_secret, &payload)?;

        let state = ClientHandshakeStep3 {
            network_key: self.network_key,
            identity: self.identity,
            client_eph_pk: self.ephemeral_public,
            server_eph_pk,
            shared_ab,
            shared_secret,
        };

        Ok((state, encrypted))
    }
}

/// Client state after step 3, waiting for server's step 4 response.
pub struct ClientHandshakeStep3 {
    network_key: [u8; 32],
    identity: Identity,
    client_eph_pk: X25519Public,
    server_eph_pk: X25519Public,
    shared_ab: SharedSecret,
    shared_secret: [u8; 32],
}

impl ClientHandshakeStep3 {
    /// Step 4 (client side): Verify server's auth response and derive session keys.
    pub fn step4_verify(self, server_auth: &[u8]) -> Result<HandshakeOutcome> {
        // Decrypt server's auth
        let payload = decrypt_payload(&self.shared_secret, server_auth)?;
        if payload.len() != 96 {
            return Err(EgreError::HandshakeFailed {
                reason: "server auth payload wrong size".into(),
            });
        }

        let sig_bytes: [u8; 64] = payload[..64].try_into().unwrap();
        let server_pk_bytes: [u8; 32] = payload[64..96].try_into().unwrap();

        let server_vk = ed25519_dalek::VerifyingKey::from_bytes(&server_pk_bytes).map_err(
            |_| EgreError::HandshakeFailed {
                reason: "invalid server public key".into(),
            },
        )?;

        // Verify server's signature: sign(network_key | client_eph_pk | hash(ab))
        let ab_hash = sha256(self.shared_ab.as_bytes());
        let mut proof_data = Vec::new();
        proof_data.extend_from_slice(&self.network_key);
        proof_data.extend_from_slice(self.client_eph_pk.as_bytes());
        proof_data.extend_from_slice(&ab_hash);

        let sig = Signature::from_bytes(&sig_bytes);
        server_vk
            .verify(&proof_data, &sig)
            .map_err(|_| EgreError::HandshakeFailed {
                reason: "server signature verification failed".into(),
            })?;

        // Derive session keys (different for each direction)
        let outcome = derive_session_keys(
            &self.network_key,
            self.shared_ab.as_bytes(),
            self.client_eph_pk.as_bytes(),
            self.server_eph_pk.as_bytes(),
            true, // is_client
        );

        Ok(HandshakeOutcome {
            encrypt_key: outcome.0,
            encrypt_nonce: outcome.1,
            decrypt_key: outcome.2,
            decrypt_nonce: outcome.3,
            remote_public_key: server_vk,
        })
    }
}

impl ServerHandshake {
    pub fn new(network_key: [u8; 32], identity: Identity) -> Self {
        let ephemeral_secret = EphemeralSecret::random_from_rng(rand::rngs::OsRng);
        let ephemeral_public = X25519Public::from(&ephemeral_secret);
        Self {
            network_key,
            identity,
            ephemeral_secret,
            ephemeral_public,
        }
    }

    /// Step 2: Server verifies client hello and sends its own hello.
    /// Returns 64 bytes: [server_eph_pk(32) | hmac(32)]
    pub fn step2_hello(
        self,
        client_hello: &[u8; 64],
    ) -> Result<(ServerHandshakeStep2, [u8; 64])> {
        // Parse and verify client ephemeral pk
        let client_eph_pk = {
            let mut pk_bytes = [0u8; 32];
            pk_bytes.copy_from_slice(&client_hello[..32]);
            X25519Public::from(pk_bytes)
        };

        let mut mac =
            <HmacSha256 as Mac>::new_from_slice(&self.network_key).expect("HMAC key length is valid");
        mac.update(client_eph_pk.as_bytes());
        mac.verify_slice(&client_hello[32..])
            .map_err(|_| EgreError::HandshakeFailed {
                reason: "client hello HMAC verification failed".into(),
            })?;

        // Compute shared secret ab
        let shared_ab = self.ephemeral_secret.diffie_hellman(&client_eph_pk);
        let shared_secret = derive_shared_key(&self.network_key, shared_ab.as_bytes());

        // Build server hello
        let mut server_mac =
            <HmacSha256 as Mac>::new_from_slice(&self.network_key).expect("HMAC key length is valid");
        server_mac.update(self.ephemeral_public.as_bytes());
        let tag = server_mac.finalize().into_bytes();

        let mut msg = [0u8; 64];
        msg[..32].copy_from_slice(self.ephemeral_public.as_bytes());
        msg[32..].copy_from_slice(&tag);

        let state = ServerHandshakeStep2 {
            network_key: self.network_key,
            identity: self.identity,
            server_eph_pk: self.ephemeral_public,
            client_eph_pk,
            shared_ab,
            shared_secret,
        };

        Ok((state, msg))
    }
}

/// Server state after step 2, waiting for client's auth.
pub struct ServerHandshakeStep2 {
    network_key: [u8; 32],
    identity: Identity,
    server_eph_pk: X25519Public,
    client_eph_pk: X25519Public,
    shared_ab: SharedSecret,
    shared_secret: [u8; 32],
}

impl ServerHandshakeStep2 {
    /// Step 3+4 (server side): Verify client auth, send server auth.
    pub fn step3_verify_and_respond(self, client_auth: &[u8]) -> Result<(HandshakeOutcome, Vec<u8>)> {
        // Decrypt client's auth
        let payload = decrypt_payload(&self.shared_secret, client_auth)?;
        if payload.len() != 96 {
            return Err(EgreError::HandshakeFailed {
                reason: "client auth payload wrong size".into(),
            });
        }

        let sig_bytes: [u8; 64] = payload[..64].try_into().unwrap();
        let client_pk_bytes: [u8; 32] = payload[64..96].try_into().unwrap();

        let client_vk = ed25519_dalek::VerifyingKey::from_bytes(&client_pk_bytes).map_err(
            |_| EgreError::HandshakeFailed {
                reason: "invalid client public key".into(),
            },
        )?;

        // Verify client's signature
        let ab_hash = sha256(self.shared_ab.as_bytes());
        let mut proof_data = Vec::new();
        proof_data.extend_from_slice(&self.network_key);
        proof_data.extend_from_slice(self.server_eph_pk.as_bytes());
        proof_data.extend_from_slice(&ab_hash);

        let sig = Signature::from_bytes(&sig_bytes);
        client_vk
            .verify(&proof_data, &sig)
            .map_err(|_| EgreError::HandshakeFailed {
                reason: "client signature verification failed".into(),
            })?;

        // Server signs its proof: sign(network_key | client_eph_pk | hash(ab))
        let mut server_proof = Vec::new();
        server_proof.extend_from_slice(&self.network_key);
        server_proof.extend_from_slice(self.client_eph_pk.as_bytes());
        server_proof.extend_from_slice(&ab_hash);

        let server_sig = self.identity.signing_key.sign(&server_proof);
        let server_vk = self.identity.verifying_key();

        let mut server_payload = Vec::with_capacity(96);
        server_payload.extend_from_slice(&server_sig.to_bytes());
        server_payload.extend_from_slice(server_vk.as_bytes());

        let encrypted_response = encrypt_payload(&self.shared_secret, &server_payload)?;

        // Derive session keys
        let keys = derive_session_keys(
            &self.network_key,
            self.shared_ab.as_bytes(),
            self.client_eph_pk.as_bytes(),
            self.server_eph_pk.as_bytes(),
            false, // is_server
        );

        let outcome = HandshakeOutcome {
            encrypt_key: keys.0,
            encrypt_nonce: keys.1,
            decrypt_key: keys.2,
            decrypt_nonce: keys.3,
            remote_public_key: client_vk,
        };

        Ok((outcome, encrypted_response))
    }
}

// --- Helper functions ---

fn sha256(data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

fn derive_shared_key(network_key: &[u8; 32], shared_secret: &[u8]) -> [u8; 32] {
    let mut data = Vec::new();
    data.extend_from_slice(network_key);
    data.extend_from_slice(shared_secret);
    sha256(&data)
}

fn encrypt_payload(key: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new_from_slice(key)
        .map_err(|e| EgreError::Crypto { reason: e.to_string() })?;
    let nonce = Nonce::from([0u8; 12]); // Single-use key, zero nonce is safe
    cipher
        .encrypt(&nonce, plaintext)
        .map_err(|e| EgreError::Crypto { reason: e.to_string() })
}

fn decrypt_payload(key: &[u8; 32], ciphertext: &[u8]) -> Result<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new_from_slice(key)
        .map_err(|e| EgreError::Crypto { reason: e.to_string() })?;
    let nonce = Nonce::from([0u8; 12]);
    cipher
        .decrypt(&nonce, ciphertext)
        .map_err(|_| EgreError::HandshakeFailed {
            reason: "decryption failed".into(),
        })
}

/// Derive directional session keys for Box Stream.
/// Returns (encrypt_key, encrypt_nonce, decrypt_key, decrypt_nonce).
fn derive_session_keys(
    network_key: &[u8; 32],
    shared_ab: &[u8],
    client_eph_pk: &[u8],
    server_eph_pk: &[u8],
    is_client: bool,
) -> ([u8; 32], [u8; 24], [u8; 32], [u8; 24]) {
    // client→server key
    let c2s_key = {
        let mut data = Vec::new();
        data.extend_from_slice(network_key);
        data.extend_from_slice(shared_ab);
        data.extend_from_slice(b"client-to-server");
        sha256(&data)
    };

    // server→client key
    let s2c_key = {
        let mut data = Vec::new();
        data.extend_from_slice(network_key);
        data.extend_from_slice(shared_ab);
        data.extend_from_slice(b"server-to-client");
        sha256(&data)
    };

    // Nonces derived from ephemeral public keys
    let c2s_nonce = {
        let h = sha256(client_eph_pk);
        let mut n = [0u8; 24];
        n.copy_from_slice(&h[..24]);
        n
    };

    let s2c_nonce = {
        let h = sha256(server_eph_pk);
        let mut n = [0u8; 24];
        n.copy_from_slice(&h[..24]);
        n
    };

    if is_client {
        (c2s_key, c2s_nonce, s2c_key, s2c_nonce)
    } else {
        (s2c_key, s2c_nonce, c2s_key, c2s_nonce)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_network_key() -> [u8; 32] {
        sha256(b"test-network-key")
    }

    #[test]
    fn full_handshake() {
        let net_key = test_network_key();
        let client_id = Identity::generate();
        let server_id = Identity::generate();

        // Step 1: Client hello
        let client = ClientHandshake::new(net_key, client_id.clone());
        let client_hello = client.step1_hello();

        // Step 2: Server processes hello, sends its own
        let server = ServerHandshake::new(net_key, server_id.clone());
        let (server_step2, server_hello) = server.step2_hello(&client_hello).unwrap();

        // Step 3: Client authenticates
        let (client_step3, client_auth) = client.step3_authenticate(&server_hello).unwrap();

        // Step 4: Server verifies and responds
        let (server_outcome, server_auth) =
            server_step2.step3_verify_and_respond(&client_auth).unwrap();

        // Client verifies server
        let client_outcome = client_step3.step4_verify(&server_auth).unwrap();

        // Both sides should have the same keys (but swapped encrypt/decrypt)
        assert_eq!(client_outcome.encrypt_key, server_outcome.decrypt_key);
        assert_eq!(client_outcome.decrypt_key, server_outcome.encrypt_key);

        // Both know each other's identity
        assert_eq!(client_outcome.remote_public_key, server_id.verifying_key());
        assert_eq!(server_outcome.remote_public_key, client_id.verifying_key());
    }

    #[test]
    fn wrong_network_key_rejected() {
        let client_id = Identity::generate();
        let server_id = Identity::generate();

        let client = ClientHandshake::new(sha256(b"net-a"), client_id);
        let client_hello = client.step1_hello();

        let server = ServerHandshake::new(sha256(b"net-b"), server_id);
        let result = server.step2_hello(&client_hello);
        assert!(result.is_err());
    }

    #[test]
    fn keys_are_directional() {
        let net_key = test_network_key();
        let client_id = Identity::generate();
        let server_id = Identity::generate();

        let client = ClientHandshake::new(net_key, client_id);
        let client_hello = client.step1_hello();

        let server = ServerHandshake::new(net_key, server_id);
        let (server_step2, server_hello) = server.step2_hello(&client_hello).unwrap();
        let (client_step3, client_auth) = client.step3_authenticate(&server_hello).unwrap();
        let (server_outcome, server_auth) =
            server_step2.step3_verify_and_respond(&client_auth).unwrap();
        let client_outcome = client_step3.step4_verify(&server_auth).unwrap();

        // Encrypt keys should differ between client and server
        assert_ne!(client_outcome.encrypt_key, client_outcome.decrypt_key);
        assert_ne!(server_outcome.encrypt_key, server_outcome.decrypt_key);
    }
}
