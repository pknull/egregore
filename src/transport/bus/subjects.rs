//! NATS subject mapping — Phase 2 Wave 1 Step 3.
//!
//! Per Phase 2 base plan §5.1: one JetStream stream `egregore-feed` covers
//! all `egregore.feed.>` subjects, with per-author subjects computed as
//! `egregore.feed.{16-byte hex hash of decoded pubkey bytes}`.
//!
//! The hash is computed against the raw 32-byte Ed25519 public key bytes
//! (after stripping the `@` prefix and `.ed25519` suffix from the `PublicId`
//! wire string and base64-decoding the middle). This avoids NATS subject
//! special characters (`.`, `=`) present in the `@...ed25519` wire form,
//! and keeps the subject length bounded at 32 hex chars.
//!
//! The hash is one-way: the subject does not reveal the pubkey. An
//! adversary observing the subject cannot reconstruct the author identity
//! without the broker-side mapping table that bus-subscribers already
//! maintain (`bus_author_seq_index` — Wave 1 Step 6).
//!
//! Consumer names are derived similarly so each bridge node has its own
//! durable consumer without a registration round-trip.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use sha2::{Digest, Sha256};

use crate::identity::{Identity, PublicId};

/// Subject prefix — all bus messages sit under `egregore.feed.`.
pub const SUBJECT_PREFIX: &str = "egregore.feed";

/// Wildcard filter subject for the durable consumer (covers every author).
pub const WILDCARD_FILTER: &str = "egregore.feed.>";

/// Consumer name prefix for bridge nodes.
///
/// Bridge nodes share one stream but each needs its own durable consumer
/// so their ack progress is tracked independently.
pub const CONSUMER_NAME_PREFIX: &str = "bridge";

/// Compute the per-author NATS subject for a message.
///
/// `egregore.feed.{first 16 bytes of sha256(pubkey_bytes) as lowercase hex}`
/// where `pubkey_bytes` is the 32-byte Ed25519 key material decoded from the
/// `@<base64>.ed25519` wire form.
///
/// Falls back to hashing the raw `PublicId` string when the wire form fails
/// to decode (malformed input). This path is not expected in practice —
/// `PublicId::to_verifying_key` rejects malformed IDs at ingest — but the
/// fallback means `author_subject` never panics on adversarial input, and
/// the resulting subject is still deterministic for that input.
pub fn author_subject(author: &PublicId) -> String {
    let pubkey_bytes = decode_pubkey_bytes(author).unwrap_or_else(|| author.0.as_bytes().to_vec());
    let digest = Sha256::digest(&pubkey_bytes);
    format!("{}.{}", SUBJECT_PREFIX, hex::encode(&digest[..16]))
}

/// Derive a durable consumer name from the local node identity.
///
/// `bridge-{first 8 bytes (16 hex chars) of sha256(pubkey_bytes)}`.
///
/// Deterministic: restarting the same node produces the same consumer
/// name, so NATS's durable consumer state is reused (ack progress
/// survives across bridge restarts — a direct requirement of the
/// pending-forwarding design).
pub fn derive_consumer_name(identity: &Identity) -> String {
    let pub_id = identity.public_id();
    let pubkey_bytes = decode_pubkey_bytes(&pub_id).unwrap_or_else(|| pub_id.0.as_bytes().to_vec());
    let digest = Sha256::digest(&pubkey_bytes);
    format!("{}-{}", CONSUMER_NAME_PREFIX, hex::encode(&digest[..8]))
}

/// Strip the `@` prefix and `.ed25519` suffix, base64-decode the middle.
/// Returns `None` when the wire form is not the expected `@<b64>.ed25519`
/// shape. Caller falls back to hashing the raw string (deterministic but
/// detached from the key bytes).
fn decode_pubkey_bytes(id: &PublicId) -> Option<Vec<u8>> {
    let inner = id.0.strip_prefix('@')?.strip_suffix(".ed25519")?;
    B64.decode(inner).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A well-formed PublicId for deterministic-hash testing.
    fn sample_author() -> PublicId {
        PublicId("@AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=.ed25519".to_string())
    }

    #[test]
    fn author_subject_is_deterministic_for_same_author() {
        let author = sample_author();
        let s1 = author_subject(&author);
        let s2 = author_subject(&author);
        assert_eq!(s1, s2, "same author must produce same subject");
    }

    #[test]
    fn author_subject_shape_is_prefix_plus_32_hex_chars() {
        let author = sample_author();
        let subject = author_subject(&author);
        assert!(
            subject.starts_with("egregore.feed."),
            "subject must start with egregore.feed.; got: {subject}"
        );
        let hash_part = subject.trim_start_matches("egregore.feed.");
        assert_eq!(
            hash_part.len(),
            32,
            "first 16 bytes as hex must be 32 chars; got: {hash_part}"
        );
        assert!(
            hash_part
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
            "hex segment must be lowercase ASCII hex; got: {hash_part}"
        );
    }

    #[test]
    fn author_subjects_differ_for_distinct_authors() {
        // Two distinct public IDs MUST hash to distinct subjects — any
        // collision at the 128-bit truncation would be a design-level
        // concern, but we're also asserting that the input bytes actually
        // participate in the hash (i.e., we didn't accidentally hash a
        // constant).
        let a = PublicId("@AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=.ed25519".to_string());
        let b = PublicId("@BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBA=.ed25519".to_string());
        assert_ne!(author_subject(&a), author_subject(&b));
    }

    #[test]
    fn author_subject_is_one_way() {
        // We can't recover the pubkey from the subject — the subject is
        // only the first 16 bytes of sha256(pubkey). Assert the subject
        // does NOT contain any part of the base64 form of the pubkey.
        let author = sample_author();
        let subject = author_subject(&author);
        // Strip the "A"s from the base64 form (they all decode to zero)
        // and confirm they do not appear in the subject.
        assert!(
            !subject.contains("AAAAAA"),
            "subject must not leak the pubkey's base64 form; got: {subject}"
        );
    }

    #[test]
    fn derive_consumer_name_is_deterministic() {
        // The underlying pubkey is random, but the derivation must be
        // deterministic for a given identity so restart reuses the
        // NATS durable consumer.
        let identity = Identity::generate();
        let n1 = derive_consumer_name(&identity);
        let n2 = derive_consumer_name(&identity);
        assert_eq!(n1, n2, "same identity must produce same consumer name");
        assert!(
            n1.starts_with("bridge-"),
            "consumer name must start with 'bridge-'; got: {n1}"
        );
        let hash_part = n1.trim_start_matches("bridge-");
        assert_eq!(
            hash_part.len(),
            16,
            "first 8 bytes as hex must be 16 chars; got: {hash_part}"
        );
    }

    #[test]
    fn distinct_identities_produce_distinct_consumer_names() {
        // Two freshly-generated identities MUST produce distinct consumer
        // names so two bridges sharing the same NATS server don't
        // collide on the same durable consumer.
        let a = Identity::generate();
        let b = Identity::generate();
        assert_ne!(
            derive_consumer_name(&a),
            derive_consumer_name(&b),
            "distinct identities must have distinct consumer names"
        );
    }
}
