//! Wire compatibility of Profiles signed while `Content::Profile` carried a
//! `broker` field (RFC 0001 §19 A6).
//!
//! Every Profile published before the field's removal carries a `broker`
//! key: `null` on gossip-only nodes, an object on bus-mode nodes. Such a
//! message must still pass a fresh node's ingest — size, schema, signature
//! and hash, chain — and still parse as a Profile. The tests build the
//! signed message by hand, exactly as a pre-removal node signed it, and
//! never construct `Content::Profile` from a struct literal.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use chrono::{Duration, TimeZone, Utc};
use egregore::feed::content_types::Content;
use egregore::feed::engine::FeedEngine;
use egregore::feed::models::{Message, UnsignedMessage};
use egregore::feed::profile_lifecycle::{peer_profile_validity, ProfileValidity, SystemClock};
use egregore::feed::store::FeedStore;
use egregore::identity::{sign_bytes, verify_signature, Identity};
use serde_json::{json, Value};

/// Sign `content` as sequence 1 of `author`'s feed, the way `publish_full`
/// signs: SHA-256 over the canonical `UnsignedMessage` JSON, Ed25519 over the
/// hex hash, base64 signature.
fn sign_first_message(author: &Identity, content: Value) -> Message {
    let unsigned = UnsignedMessage {
        author: author.public_id(),
        sequence: 1,
        previous: None,
        timestamp: Utc.with_ymd_and_hms(2026, 9, 1, 12, 0, 0).unwrap(),
        content,
        schema_id: Some("profile/v1".to_string()),
        relates: None,
        tags: vec![],
        trace_id: None,
        span_id: None,
        expires_at: None,
    };
    let hash = unsigned.compute_hash();
    let signature = B64.encode(sign_bytes(author, hash.as_bytes()).to_bytes());
    Message {
        author: unsigned.author,
        sequence: unsigned.sequence,
        previous: unsigned.previous,
        timestamp: unsigned.timestamp,
        content: unsigned.content,
        schema_id: unsigned.schema_id,
        relates: unsigned.relates,
        tags: unsigned.tags,
        trace_id: unsigned.trace_id,
        span_id: unsigned.span_id,
        expires_at: unsigned.expires_at,
        hash,
        signature,
    }
}

/// A Profile body as a pre-removal node serialized it, with `broker` set to
/// the given value.
fn legacy_profile(broker: Value) -> Value {
    let now = Utc::now();
    json!({
        "type": "profile",
        "name": "legacy-node",
        "description": "signed before Profile.broker was removed",
        "capabilities": ["gossip"],
        "broker": broker,
        "valid_from": (now - Duration::days(1)).to_rfc3339(),
        "valid_until": (now + Duration::days(89)).to_rfc3339(),
    })
}

fn populated_broker() -> Value {
    json!({
        "operator_name": "Example Operator",
        "jurisdiction": "US",
        "disclosure_policy": "https://example.invalid/disclosure",
        "tenancy": "dedicated",
        "broker_endpoint": "tls://nats.example.invalid:4222",
        "backend": "nats",
    })
}

/// A node whose schemas directory is initialized from the embedded defaults,
/// so ingest validates Profiles against the current `profile/v1` schema.
fn fresh_node_with_default_schemas() -> (FeedEngine, tempfile::TempDir) {
    let schemas = tempfile::tempdir().expect("schemas tempdir");
    let engine = FeedEngine::with_schemas_dir(FeedStore::open_memory().unwrap(), schemas.path());
    assert!(
        engine.schema_registry().get("profile/v1").is_some(),
        "default schemas must include profile/v1, or this test validates nothing"
    );
    (engine, schemas)
}

fn assert_legacy_profile_round_trips(broker: Value) {
    let author = Identity::generate();
    let signed = sign_first_message(&author, legacy_profile(broker.clone()));
    let (engine, _schemas) = fresh_node_with_default_schemas();

    engine
        .ingest(&signed)
        .expect("a legacy Profile carrying `broker` must pass schema, signature and chain checks");

    let stored = engine
        .get_message(&signed.hash)
        .unwrap()
        .expect("ingested Profile must be stored");
    assert_eq!(
        stored.content["broker"], broker,
        "ingest stores the signed content as received"
    );
    let signature: [u8; 64] = B64
        .decode(&stored.signature)
        .unwrap()
        .try_into()
        .expect("64-byte signature");
    verify_signature(
        &author.verifying_key(),
        stored.hash.as_bytes(),
        &ed25519_dalek::Signature::from_bytes(&signature),
    )
    .expect("the stored Profile must still verify against its author's key");

    let parsed: Content = serde_json::from_value(stored.content.clone())
        .expect("a legacy Profile carrying `broker` must still parse as Content");
    match &parsed {
        Content::Profile {
            name, capabilities, ..
        } => {
            assert_eq!(name, "legacy-node");
            assert_eq!(capabilities, &vec!["gossip".to_string()]);
        }
        other => panic!("expected Content::Profile, got {other:?}"),
    }
    assert!(
        parsed.to_value().get("broker").is_none(),
        "Content::Profile no longer carries `broker`; it must not round-trip"
    );

    assert_eq!(
        peer_profile_validity(&engine, &author.public_id(), &SystemClock).unwrap(),
        ProfileValidity::Valid,
        "validity fields of a legacy Profile still drive the peer soft filter"
    );
}

#[test]
fn legacy_profile_with_null_broker_ingests_verifies_and_parses() {
    assert_legacy_profile_round_trips(Value::Null);
}

#[test]
fn legacy_profile_with_populated_broker_ingests_verifies_and_parses() {
    assert_legacy_profile_round_trips(populated_broker());
}
