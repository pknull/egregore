//! Integration tests for Phase 1 Step 12 — peer-side soft filter for
//! expired Profiles.
//!
//! These tests guarantee three load-bearing properties of RFC 0001 §11.2:
//!
//! 1. An expired peer Profile surfaces `profile_expired: true` on `/v1/mesh`
//!    but the peer is NEVER removed from the response.
//! 2. The same flag + non-removal invariant holds on `build_node_status`.
//! 3. Messages from peers with an expired Profile STILL INGEST. This is the
//!    zero-behavior-change guardrail — Step 12 must never gate the wire.

use std::sync::Arc;
use std::time::Instant;

use chrono::{Duration, Utc};
use egregore::api::routes_mesh::build_mesh_health;
use egregore::api::AppState;
use egregore::config::Config;
use egregore::feed::content_types::Content;
use egregore::feed::engine::FeedEngine;
use egregore::feed::store::FeedStore;
use egregore::identity::Identity;
use egregore::status::build_node_status;

/// Build a minimal AppState wrapping the given engine + identity for tests
/// that exercise `build_mesh_health`. No HTTP router, no MCP.
fn test_app_state(identity: Identity, engine: Arc<FeedEngine>) -> AppState {
    let tmp = tempfile::tempdir().expect("tempdir");
    let blob_store = egregore::blob::BlobStore::new(tmp.path());
    AppState {
        identity,
        engine,
        config: Arc::new(Config::default()),
        started_at: Instant::now(),
        mcp_registry: egregore::api::mcp_registry::create_registry(),
        blob_store,
    }
}

/// Publish an expired Profile on `peer` into `engine`. The Profile has
/// `valid_from = t0 - 30d`, `valid_until = t0 - 1d`, so it is expired
/// relative to wall-clock `Utc::now()` at test time.
fn publish_expired_profile(engine: &FeedEngine, peer: &Identity) {
    let now = Utc::now();
    let expired = Content::Profile {
        name: "expired-peer".to_string(),
        description: None,
        capabilities: vec![],
        broker: None,
        valid_from: Some(now - Duration::days(30)),
        valid_until: Some(now - Duration::days(1)),
    };
    engine
        .publish_with_schema(peer, expired.to_value(), None, None, vec![])
        .expect("publish expired Profile");
}

/// Record a direct peer observation so the peer appears in mesh/status
/// output. Without this, the mesh-health query returns empty.
fn record_peer(engine: &FeedEngine, peer: &Identity) {
    engine
        .store()
        .record_direct_observation(&peer.public_id(), 1, 1)
        .expect("record peer observation");
}

#[tokio::test]
async fn expired_peer_remains_in_mesh_response_with_flag_set() {
    let self_identity = Identity::generate();
    let peer_identity = Identity::generate();
    let store = FeedStore::open_memory().unwrap();
    let engine = Arc::new(FeedEngine::new(store));

    // Peer publishes expired Profile + is observed by us.
    publish_expired_profile(&engine, &peer_identity);
    record_peer(&engine, &peer_identity);

    let state = test_app_state(self_identity, engine);
    let response = build_mesh_health(&state).await;

    // Peer MUST still be in the list.
    let peer_entry = response
        .peers
        .iter()
        .find(|p| p.peer_id == peer_identity.public_id().0)
        .expect("expired peer must remain in mesh response (RFC 0001 §11.2)");
    assert!(
        peer_entry.profile_expired,
        "profile_expired must be true for a peer whose Profile has valid_until < now"
    );
}

#[tokio::test]
async fn expired_peer_remains_in_node_status_with_flag_set() {
    let self_identity = Identity::generate();
    let peer_identity = Identity::generate();
    let store = FeedStore::open_memory().unwrap();
    let engine = FeedEngine::new(store);

    publish_expired_profile(&engine, &peer_identity);
    record_peer(&engine, &peer_identity);

    let status = build_node_status(
        &self_identity,
        &Config::default(),
        &engine,
        None,
        Instant::now(),
    )
    .expect("build_node_status");

    let peer_entry = status
        .peers
        .health
        .iter()
        .find(|p| p.peer == peer_identity.public_id().0)
        .expect("expired peer must remain in node_status response (RFC 0001 §11.2)");
    assert!(
        peer_entry.profile_expired,
        "profile_expired must be true for a peer whose Profile has valid_until < now"
    );
}

/// Regression for the node_status/v1 schema lock-step requirement (RFC 0001
/// §11.2 + Phase 1 plan §6.3): when a peer is present in node_status output,
/// `publish_with_schema` on `node_status/v1` MUST succeed — meaning the
/// embedded JSON Schema at `src/feed/schema.rs` includes `profile_expired`
/// as a permitted per-peer property. Before the fix, the schema's
/// `additionalProperties: false` rejected the extended content.
#[tokio::test]
async fn node_status_with_peer_roundtrips_through_schema_validation() {
    let self_identity = Identity::generate();
    let peer_identity = Identity::generate();
    let store = FeedStore::open_memory().unwrap();
    let engine = FeedEngine::new(store);

    publish_expired_profile(&engine, &peer_identity);
    record_peer(&engine, &peer_identity);

    let status = build_node_status(
        &self_identity,
        &Config::default(),
        &engine,
        None,
        Instant::now(),
    )
    .expect("build_node_status");

    // Sanity: peer is actually in the status output (otherwise this test
    // wouldn't exercise the schema path for per-peer objects).
    assert!(
        status
            .peers
            .health
            .iter()
            .any(|p| p.peer == peer_identity.public_id().0),
        "precondition: peer must appear in node_status.peers.health"
    );

    let status_json = serde_json::to_value(&status).expect("serialize node_status");

    // This is the load-bearing assertion: publish through the real schema
    // registry must accept the content. If the schema drifts from the Rust
    // struct, this fails with `Schema { reason: ... }`.
    let message = engine
        .publish_with_schema(
            &self_identity,
            status_json,
            Some("node_status/v1".to_string()),
            None,
            vec!["node_status".to_string()],
        )
        .expect("node_status with peer + profile_expired must pass schema validation");

    assert_eq!(message.schema_id.as_deref(), Some("node_status/v1"));
}

/// RFC 0001 §11.2 guardrail: messages from expired-Profile peers STILL flow
/// on the wire. Step 12 is a read-path flag, not an ingest gate. If this
/// test ever fails, Step 12 has accidentally become a filter on ingest —
/// which would be a breaking wire-behavior change.
#[test]
fn messages_from_expired_peer_still_ingest() {
    // Engine A is "us"; engine B is the peer (used only to sign + construct
    // valid signed messages).
    let peer_identity = Identity::generate();

    let store_a = FeedStore::open_memory().unwrap();
    let engine_a = FeedEngine::new(store_a);

    let store_b = FeedStore::open_memory().unwrap();
    let engine_b = FeedEngine::new(store_b);

    // Peer B publishes an expired Profile on its own feed (sequence 1).
    let now = Utc::now();
    let expired = Content::Profile {
        name: "expired-peer".to_string(),
        description: None,
        capabilities: vec![],
        broker: None,
        valid_from: Some(now - Duration::days(30)),
        valid_until: Some(now - Duration::days(1)),
    };
    let expired_msg = engine_b
        .publish_with_schema(&peer_identity, expired.to_value(), None, None, vec![])
        .expect("publish expired Profile on peer B");

    // Peer B then publishes a regular message (sequence 2).
    let content_msg = engine_b
        .publish(
            &peer_identity,
            serde_json::json!({"type": "message", "text": "hello from expired peer"}),
            None,
            vec![],
        )
        .expect("publish content message on peer B");

    // Engine A ingests both (as it would during gossip sync). Ingest must
    // succeed — expired Profile does NOT gate wire-level ingest.
    engine_a
        .ingest(&expired_msg)
        .expect("expired Profile must still ingest on engine A");
    engine_a
        .ingest(&content_msg)
        .expect("message from expired-Profile peer must still ingest");

    // Verify engine A actually stored the message.
    let stored = engine_a
        .get_message(&content_msg.hash)
        .expect("get_message must succeed")
        .expect("message from expired-Profile peer must be retrievable on engine A");
    assert_eq!(stored.hash, content_msg.hash);
    assert_eq!(stored.author, peer_identity.public_id());
}
