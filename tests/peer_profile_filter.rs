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

// ----------------------------------------------------------------------
// Phase 2 Wave 5 Step 25 — node_status/v1 lock-step with TransportHealth.
// Same pattern as the profile_expired lock-step test above: the Rust
// struct's `transport_health` field MUST be permitted by the embedded
// JSON Schema (src/feed/schema.rs), including the nested
// `BridgeQueuesHealth` sub-schema for composite/bridge deployments.
// ----------------------------------------------------------------------

use egregore::transport::health::{BridgeQueuesHealth, TransportHealth};

#[tokio::test]
async fn node_status_with_transport_health_validates_against_schema() {
    // Constructs a fully-populated `TransportHealth` — including
    // composite children with `bridge_queues` — and asserts
    // `publish_with_schema("node_status/v1", ...)` accepts it.
    //
    // This exercises the deepest branch of the schema: the
    // `$defs/transport_health` recursive reference (`children[]`) and
    // the `$defs/bridge_queues_health` branch for per-direction metrics.
    let self_identity = Identity::generate();
    let temp_dir = std::env::temp_dir().join(format!(
        "egregore_step25_with_th_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&temp_dir);
    let store = FeedStore::open_memory().unwrap();
    // Use with_schemas_dir so default schemas (including updated
    // node_status/v1) get written into a fresh dir and loaded.
    let engine = FeedEngine::with_schemas_dir(store, &temp_dir);

    let now = Utc::now();
    let child_bus = TransportHealth {
        connected: true,
        backend: "nats",
        last_successful_publish: Some(now),
        last_peer_contact: Some(now),
        unreplicated_count: 3,
        inflight_publishes: 1,
        last_error: None,
        children: vec![],
        bridge_queues: Some(BridgeQueuesHealth {
            destination: "nats".to_string(),
            depth_total: 12,
            authors_backpressured: 0,
            authors_active: 2,
            backpressure_events_total: 0,
            self_echo_total: 4,
            oldest_queued_age_secs: Some(7),
            publish_in_flight_age_secs: Some(1),
            ack_on_error_total: 0,
            nats_redelivery_total: 0,
            last_error: None,
        }),
    };
    let child_gossip = TransportHealth {
        connected: true,
        backend: "gossip",
        last_successful_publish: Some(now),
        last_peer_contact: Some(now),
        unreplicated_count: 0,
        inflight_publishes: 0,
        last_error: None,
        children: vec![],
        bridge_queues: Some(BridgeQueuesHealth {
            destination: "gossip".to_string(),
            depth_total: 0,
            authors_backpressured: 0,
            authors_active: 0,
            backpressure_events_total: 0,
            self_echo_total: 0,
            oldest_queued_age_secs: None,
            publish_in_flight_age_secs: None,
            ack_on_error_total: 0,
            nats_redelivery_total: 0,
            last_error: None,
        }),
    };
    let composite = TransportHealth {
        connected: true,
        backend: "composite",
        last_successful_publish: Some(now),
        last_peer_contact: Some(now),
        unreplicated_count: 3,
        inflight_publishes: 1,
        last_error: None,
        children: vec![child_bus, child_gossip],
        bridge_queues: None,
    };

    let status = egregore::status::NodeStatusMessage {
        msg_type: "node_status".to_string(),
        node: self_identity.public_id().0,
        ts: now,
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_secs: 42,
        peers: egregore::status::NodeStatusPeers {
            connected: 0,
            known: 0,
            health: vec![],
        },
        storage: egregore::status::NodeStatusStorage {
            bytes: 0,
            message_count: 0,
            feed_count: 0,
        },
        throughput: egregore::status::NodeStatusThroughput {
            msgs_in_last_hour: 0,
            msgs_out_last_hour: 0,
        },
        transport_health: Some(composite),
    };

    let status_json = serde_json::to_value(&status).expect("serialize node_status");

    // Load-bearing assertion: schema accepts full TransportHealth shape.
    let message = engine
        .publish_with_schema(
            &self_identity,
            status_json,
            Some("node_status/v1".to_string()),
            None,
            vec!["node_status".to_string()],
        )
        .expect("node_status with full TransportHealth must pass schema validation");
    assert_eq!(message.schema_id.as_deref(), Some("node_status/v1"));

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn node_status_without_transport_health_still_valid() {
    // Back-compat guardrail: a node_status message WITHOUT the new
    // `transport_health` field (the Phase-1 shape) MUST still validate.
    // `#[serde(default, skip_serializing_if = "Option::is_none")]` +
    // the schema's `transport_health` NOT appearing in `required` are
    // the load-bearing bits this test pins.
    let self_identity = Identity::generate();
    let temp_dir = std::env::temp_dir().join(format!(
        "egregore_step25_without_th_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&temp_dir);
    let store = FeedStore::open_memory().unwrap();
    let engine = FeedEngine::with_schemas_dir(store, &temp_dir);

    let status = egregore::status::NodeStatusMessage {
        msg_type: "node_status".to_string(),
        node: self_identity.public_id().0,
        ts: Utc::now(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_secs: 0,
        peers: egregore::status::NodeStatusPeers {
            connected: 0,
            known: 0,
            health: vec![],
        },
        storage: egregore::status::NodeStatusStorage {
            bytes: 0,
            message_count: 0,
            feed_count: 0,
        },
        throughput: egregore::status::NodeStatusThroughput {
            msgs_in_last_hour: 0,
            msgs_out_last_hour: 0,
        },
        transport_health: None,
    };

    let status_json = serde_json::to_value(&status).expect("serialize node_status");
    // Confirm the None serializes to "field absent" (not "null").
    assert!(
        status_json.get("transport_health").is_none(),
        "transport_health: None must be omitted from JSON; got: {status_json}"
    );

    let message = engine
        .publish_with_schema(
            &self_identity,
            status_json,
            Some("node_status/v1".to_string()),
            None,
            vec!["node_status".to_string()],
        )
        .expect(
            "node_status without transport_health must pass schema validation (back-compat)",
        );
    assert_eq!(message.schema_id.as_deref(), Some("node_status/v1"));

    let _ = std::fs::remove_dir_all(&temp_dir);
}
