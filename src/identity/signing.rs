//! Ed25519 sign/verify wrappers used by feed publishing and SHS authentication.

use ed25519_dalek::{Signature, Signer, Verifier, VerifyingKey};

use crate::error::{EgreError, Result};
use crate::identity::keys::Identity;

/// Sign arbitrary bytes with the local identity.
pub fn sign_bytes(identity: &Identity, data: &[u8]) -> Signature {
    identity.signing_key.sign(data)
}

/// Verify a signature against a public key and data.
pub fn verify_signature(
    verifying_key: &VerifyingKey,
    data: &[u8],
    signature: &Signature,
) -> Result<()> {
    verifying_key
        .verify(data, signature)
        .map_err(|_| EgreError::SignatureInvalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify() {
        let identity = Identity::generate();
        let data = b"egregore test message";

        let sig = sign_bytes(&identity, data);
        verify_signature(&identity.verifying_key(), data, &sig).unwrap();
    }

    #[test]
    fn verify_wrong_data_fails() {
        let identity = Identity::generate();
        let sig = sign_bytes(&identity, b"original");
        let result = verify_signature(&identity.verifying_key(), b"tampered", &sig);
        assert!(result.is_err());
    }

    #[test]
    fn verify_wrong_key_fails() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let sig = sign_bytes(&alice, b"data");
        let result = verify_signature(&bob.verifying_key(), b"data", &sig);
        assert!(result.is_err());
    }
}
