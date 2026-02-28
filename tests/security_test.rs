//! Security test suite for Egregore gossip protocol.
//!
//! Threat model: adversary with TCP access to gossip peers, potentially with
//! valid network credentials, attempting injection, tampering, or auth bypass.
//!
//! Tests adversarial scenarios that existing unit tests don't cover at the
//! integration (over-the-wire) level.

use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use chrono::Utc;

use egregore::feed::content_types::Content;
use egregore::feed::engine::FeedEngine;
use egregore::feed::models::{Message, UnsignedMessage};
use egregore::feed::store::FeedStore;
use egregore::gossip::connection::SecureConnection;
use egregore::gossip::replication::{FeedState, GossipMessage, ReplicationConfig};
use egregore::identity::{sign_bytes, Identity, PublicId};

const NETWORK_KEY: [u8; 32] = [42u8; 32];

fn test_content(title: &str) -> serde_json::Value {
    Content::Insight {
        title: title.into(),
        context: Some("test".into()),
        observation: "obs".into(),
        evidence: Some("ev".into()),
        guidance: Some("guid".into()),
        confidence: Some(0.5),
        tags: vec![],
    }
    .to_value()
}

/// Create a legitimately signed message (sequence 1, no previous).
fn create_signed_message(identity: &Identity, content: serde_json::Value) -> Message {
    let unsigned = UnsignedMessage {
        author: identity.public_id(),
        sequence: 1,
        previous: None,
        timestamp: Utc::now(),
        content,
        schema_id: None,
        relates: None,
        tags: vec![],
        trace_id: None,
        span_id: None,
    };
    let hash = unsigned.compute_hash();
    let sig = sign_bytes(identity, hash.as_bytes());
    Message {
        author: unsigned.author,
        sequence: unsigned.sequence,
        previous: unsigned.previous,
        timestamp: unsigned.timestamp,
        content: unsigned.content,
        schema_id: None,
        relates: None,
        tags: vec![],
        trace_id: None,
        span_id: None,
        hash,
        signature: B64.encode(sig.to_bytes()),
    }
}

/// Malicious gossip server: follows protocol structure but injects crafted messages.
///
/// Implements the server side of the Have/Want/Messages/Done exchange, claiming
/// to have `victim_author`'s feed and responding with the provided messages.
async fn serve_malicious_messages(
    listener: &tokio::net::TcpListener,
    server_identity: Identity,
    victim_author: PublicId,
    messages: Vec<Message>,
) {
    let (stream, _) = listener.accept().await.unwrap();
    let mut conn = SecureConnection::accept(stream, NETWORK_KEY, server_identity)
        .await
        .unwrap();

    // Recv client Have
    let _ = conn.recv().await.unwrap();

    // Send Have claiming victim's feed
    let have = GossipMessage::Have {
        feeds: vec![FeedState {
            author: victim_author,
            latest_sequence: messages.len() as u64,
        }],
        peer_observations: vec![],
        bloom_summaries: vec![],
        subscribed_topics: vec![],
    };
    conn.send(&serde_json::to_vec(&have).unwrap())
        .await
        .unwrap();

    // Recv client Want
    let _ = conn.recv().await.unwrap();

    // Send malicious messages
    if !messages.is_empty() {
        let payload = GossipMessage::Messages { messages };
        conn.send(&serde_json::to_vec(&payload).unwrap())
            .await
            .unwrap();
    }

    // Send Done
    conn.send(&serde_json::to_vec(&GossipMessage::Done).unwrap())
        .await
        .unwrap();

    // Send empty Want (we don't need anything from client)
    let want = GossipMessage::Want { requests: vec![] };
    conn.send(&serde_json::to_vec(&want).unwrap())
        .await
        .unwrap();

    // Recv client Done
    let _ = conn.recv().await;
    let _ = conn.close().await;
}

/// Connect to a malicious server and attempt replication. Returns normally
/// even when messages are rejected (ingest errors are logged, not propagated).
async fn replicate_from_malicious_server(
    victim_author: PublicId,
    messages: Vec<Message>,
    client_engine: &Arc<FeedEngine>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server_handle = tokio::spawn(async move {
        serve_malicious_messages(&listener, Identity::generate(), victim_author, messages).await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut conn = SecureConnection::connect(stream, NETWORK_KEY, Identity::generate())
        .await
        .unwrap();
    egregore::gossip::replication::replicate_as_client(
        &mut conn,
        client_engine,
        &ReplicationConfig::default(),
    )
    .await
    .unwrap();
    let _ = conn.close().await;

    tokio::time::timeout(Duration::from_secs(5), server_handle)
        .await
        .expect("server timed out")
        .expect("server panicked");
}

/// Standard replication between two engines over TCP.
async fn replicate_once(
    server_identity: &Identity,
    server_engine: &Arc<FeedEngine>,
    client_engine: &Arc<FeedEngine>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let id = server_identity.clone();
    let eng = server_engine.clone();
    let handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut conn = SecureConnection::accept(stream, NETWORK_KEY, id).await.unwrap();
        let config = ReplicationConfig::default();
        egregore::gossip::replication::replicate_as_server(&mut conn, &eng, &config)
            .await
            .unwrap();
        let _ = conn.close().await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut conn = SecureConnection::connect(stream, NETWORK_KEY, Identity::generate())
        .await
        .unwrap();
    egregore::gossip::replication::replicate_as_client(
        &mut conn,
        client_engine,
        &ReplicationConfig::default(),
    )
    .await
    .unwrap();
    let _ = conn.close().await;

    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("server timed out")
        .expect("server panicked");
}

// ---------------------------------------------------------------------------
// Test 1: Network isolation — different network keys cannot cross-talk
// ---------------------------------------------------------------------------

#[tokio::test]
async fn wrong_network_key_rejects_gossip_connection() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("egregore=debug")
        .try_init();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server_key: [u8; 32] = [1u8; 32];
    let client_key: [u8; 32] = [2u8; 32];

    let server_handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        SecureConnection::accept(stream, server_key, Identity::generate()).await
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let client_result =
        SecureConnection::connect(stream, client_key, Identity::generate()).await;

    let server_result = tokio::time::timeout(Duration::from_secs(5), server_handle)
        .await
        .expect("server timed out")
        .expect("server panicked");

    // Both sides should fail: server rejects HMAC, client gets connection drop
    assert!(
        client_result.is_err() && server_result.is_err(),
        "both sides of handshake must fail with mismatched network keys \
         (client={:?}, server={:?})",
        client_result.err(),
        server_result.err()
    );
}

// ---------------------------------------------------------------------------
// Test 2: Forged signature — can't impersonate without private key
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forged_signature_rejected_during_replication() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("egregore=debug")
        .try_init();

    let victim = Identity::generate();
    let attacker = Identity::generate();

    // Forge: valid structure, victim as author, but signed with attacker's key
    let unsigned = UnsignedMessage {
        author: victim.public_id(),
        sequence: 1,
        previous: None,
        timestamp: Utc::now(),
        content: test_content("forged insight"),
        schema_id: None,
        relates: None,
        tags: vec![],
        trace_id: None,
        span_id: None,
    };
    let hash = unsigned.compute_hash();
    let sig = sign_bytes(&attacker, hash.as_bytes());
    let forged = Message {
        author: unsigned.author,
        sequence: unsigned.sequence,
        previous: unsigned.previous,
        timestamp: unsigned.timestamp,
        content: unsigned.content,
        schema_id: None,
        relates: None,
        tags: vec![],
        trace_id: None,
        span_id: None,
        hash,
        signature: B64.encode(sig.to_bytes()),
    };

    let engine_b = Arc::new(FeedEngine::new(FeedStore::open_memory().unwrap()));
    replicate_from_malicious_server(victim.public_id(), vec![forged], &engine_b).await;

    assert!(
        engine_b.store().get_all_feeds().unwrap().is_empty(),
        "forged signature must be rejected — cannot impersonate without private key"
    );
}

// ---------------------------------------------------------------------------
// Test 4: Tampered content — modifying payload invalidates hash
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tampered_content_rejected_during_replication() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("egregore=debug")
        .try_init();

    let identity = Identity::generate();
    let original = create_signed_message(&identity, test_content("original"));

    // Tamper: change content but keep original hash and signature
    let tampered = Message {
        author: original.author.clone(),
        sequence: original.sequence,
        previous: original.previous.clone(),
        timestamp: original.timestamp,
        content: test_content("tampered payload"),
        schema_id: None,
        relates: None,
        tags: vec![],
        trace_id: None,
        span_id: None,
        hash: original.hash,
        signature: original.signature,
    };

    let engine_b = Arc::new(FeedEngine::new(FeedStore::open_memory().unwrap()));
    replicate_from_malicious_server(identity.public_id(), vec![tampered], &engine_b).await;

    assert!(
        engine_b.store().get_all_feeds().unwrap().is_empty(),
        "tampered content must be rejected — recomputed hash won't match claimed hash"
    );
}

// ---------------------------------------------------------------------------
// Test 5: Tampered hash — even with correct hash, signature binds to original
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tampered_hash_rejected_during_replication() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("egregore=debug")
        .try_init();

    let identity = Identity::generate();
    let original = create_signed_message(&identity, test_content("original"));

    // Tamper: change content + recompute hash, but can't re-sign
    let tampered_unsigned = UnsignedMessage {
        author: identity.public_id(),
        sequence: 1,
        previous: None,
        timestamp: original.timestamp,
        content: test_content("tampered payload"),
        schema_id: None,
        relates: None,
        tags: vec![],
        trace_id: None,
        span_id: None,
    };
    let new_hash = tampered_unsigned.compute_hash();
    let tampered = Message {
        author: identity.public_id(),
        sequence: 1,
        previous: None,
        timestamp: original.timestamp,
        content: test_content("tampered payload"),
        schema_id: None,
        relates: None,
        tags: vec![],
        trace_id: None,
        span_id: None,
        hash: new_hash,
        signature: original.signature, // signed the original hash, not this one
    };

    let engine_b = Arc::new(FeedEngine::new(FeedStore::open_memory().unwrap()));
    replicate_from_malicious_server(identity.public_id(), vec![tampered], &engine_b).await;

    assert!(
        engine_b.store().get_all_feeds().unwrap().is_empty(),
        "tampered hash must be rejected — signature binds to original hash"
    );
}

// ---------------------------------------------------------------------------
// Test 6: Duplicate/replay — re-replication is idempotent
// ---------------------------------------------------------------------------

#[tokio::test]
async fn duplicate_message_rejected_during_replication() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("egregore=debug")
        .try_init();

    let identity_a = Identity::generate();
    let engine_a = Arc::new(FeedEngine::new(FeedStore::open_memory().unwrap()));
    engine_a
        .publish(&identity_a, test_content("insight"), None, vec![])
        .unwrap();

    let engine_b = Arc::new(FeedEngine::new(FeedStore::open_memory().unwrap()));

    // First replication: B gets A's message
    replicate_once(&identity_a, &engine_a, &engine_b).await;
    assert_eq!(
        engine_b
            .store()
            .get_messages_after(&identity_a.public_id(), 0, 10)
            .unwrap()
            .len(),
        1
    );

    // Second replication: should be idempotent, no phantom duplicates
    replicate_once(&identity_a, &engine_a, &engine_b).await;

    let msgs = engine_b
        .store()
        .get_messages_after(&identity_a.public_id(), 0, 10)
        .unwrap();
    assert_eq!(
        msgs.len(),
        1,
        "duplicate replication must not create phantom messages"
    );
}

// ---------------------------------------------------------------------------
// Test 7: Sequence gap — late join accepted but flagged as unvalidated chain
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sequence_gap_accepted_but_flagged_during_replication() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("egregore=debug")
        .try_init();

    let author = Identity::generate();

    // Craft message at sequence 5 (skipping 1-4) — valid signature, valid hash
    let unsigned = UnsignedMessage {
        author: author.public_id(),
        sequence: 5,
        previous: Some("fake_previous_hash".to_string()),
        timestamp: Utc::now(),
        content: test_content("late join message"),
        schema_id: None,
        relates: None,
        tags: vec![],
        trace_id: None,
        span_id: None,
    };
    let hash = unsigned.compute_hash();
    let sig = sign_bytes(&author, hash.as_bytes());
    let gap_msg = Message {
        author: unsigned.author,
        sequence: unsigned.sequence,
        previous: unsigned.previous,
        timestamp: unsigned.timestamp,
        content: unsigned.content,
        schema_id: None,
        relates: None,
        tags: vec![],
        trace_id: None,
        span_id: None,
        hash: hash.clone(),
        signature: B64.encode(sig.to_bytes()),
    };

    let engine_b = Arc::new(FeedEngine::new(FeedStore::open_memory().unwrap()));
    replicate_from_malicious_server(author.public_id(), vec![gap_msg], &engine_b).await;

    // Message accepted (not rejected) but flagged as unvalidated chain
    assert_eq!(
        engine_b.store().get_all_feeds().unwrap().len(),
        1,
        "gap message should be accepted (signature valid)"
    );
    assert!(
        !engine_b.store().is_chain_valid(&hash).unwrap(),
        "gap message must be flagged as chain_valid = false"
    );
}

// ---------------------------------------------------------------------------
// Test 8: Fork attack — wrong previous hash attempts to fork the chain
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fork_attack_rejected_during_replication() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("egregore=debug")
        .try_init();

    let identity = Identity::generate();
    let engine_b = Arc::new(FeedEngine::new(FeedStore::open_memory().unwrap()));

    // First, legitimately replicate message 1
    let m1 = create_signed_message(&identity, test_content("legitimate first"));
    replicate_from_malicious_server(identity.public_id(), vec![m1.clone()], &engine_b).await;
    assert_eq!(
        engine_b.store().get_all_feeds().unwrap().len(),
        1,
        "legitimate message 1 should be accepted"
    );

    // Now craft a forked message 2 with wrong previous hash
    let unsigned = UnsignedMessage {
        author: identity.public_id(),
        sequence: 2,
        previous: Some("forked_chain_fake_hash".to_string()),
        timestamp: Utc::now(),
        content: test_content("forked message"),
        schema_id: None,
        relates: None,
        tags: vec![],
        trace_id: None,
        span_id: None,
    };
    let hash = unsigned.compute_hash();
    let sig = sign_bytes(&identity, hash.as_bytes());
    let forked_m2 = Message {
        author: unsigned.author,
        sequence: unsigned.sequence,
        previous: unsigned.previous,
        timestamp: unsigned.timestamp,
        content: unsigned.content,
        schema_id: None,
        relates: None,
        tags: vec![],
        trace_id: None,
        span_id: None,
        hash,
        signature: B64.encode(sig.to_bytes()),
    };

    replicate_from_malicious_server(identity.public_id(), vec![forked_m2], &engine_b).await;

    // Should still have only 1 message — the fork was rejected
    let msgs = engine_b
        .store()
        .get_messages_after(&identity.public_id(), 0, 10)
        .unwrap();
    assert_eq!(
        msgs.len(),
        1,
        "fork attack must be rejected — wrong previous hash"
    );
    assert_eq!(msgs[0].sequence, 1);
}

// ---------------------------------------------------------------------------
// Test 9: Malformed protocol — garbage input doesn't corrupt state
// ---------------------------------------------------------------------------

#[tokio::test]
async fn malformed_gossip_message_handled() {

    let _ = tracing_subscriber::fmt()
        .with_env_filter("egregore=debug")
        .try_init();

    let server_identity = Identity::generate();
    let engine = Arc::new(FeedEngine::new(FeedStore::open_memory().unwrap()));
    engine
        .publish(&server_identity, test_content("server data"), None, vec![])
        .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server_id = server_identity.clone();
    let server_eng = engine.clone();
    let server_handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut conn = SecureConnection::accept(stream, NETWORK_KEY, server_id)
            .await
            .unwrap();
        let config = ReplicationConfig::default();
        let result =
            egregore::gossip::replication::replicate_as_server(&mut conn, &server_eng, &config)
                .await;
        assert!(result.is_err(), "malformed input should cause error, not panic");
        let _ = conn.close().await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut conn = SecureConnection::connect(stream, NETWORK_KEY, Identity::generate())
        .await
        .unwrap();
    conn.send(b"{{not valid json}}").await.unwrap();
    let _ = conn.close().await;

    tokio::time::timeout(Duration::from_secs(5), server_handle)
        .await
        .expect("server timed out")
        .expect("server panicked");

    assert_eq!(
        engine.store().get_all_feeds().unwrap().len(),
        1,
        "server state must be unchanged after malformed input"
    );
}

// ---------------------------------------------------------------------------
// Test 10: Empty feeds — empty state doesn't cause protocol failure
// ---------------------------------------------------------------------------

#[tokio::test]
async fn empty_feed_replication_is_safe() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("egregore=debug")
        .try_init();

    let engine_a = Arc::new(FeedEngine::new(FeedStore::open_memory().unwrap()));
    let engine_b = Arc::new(FeedEngine::new(FeedStore::open_memory().unwrap()));

    replicate_once(&Identity::generate(), &engine_a, &engine_b).await;

    assert!(engine_a.store().get_all_feeds().unwrap().is_empty());
    assert!(engine_b.store().get_all_feeds().unwrap().is_empty());
}
