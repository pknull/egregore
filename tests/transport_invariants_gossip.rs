//! Wire-level invariant tests — a mix of MockTransport strict assertions and
//! a gossip-loopback smoke test (plan §5, §5.9).
//!
//! Covers:
//! - B.1a per-author FIFO delivery (strict on MockTransport, smoke on gossip)
//! - B.2 no silent drop / gap detectability
//! - B.3 at-least-once with dedup
//! - B.5 gap detectability via chain truncation
//! - B.7 shutdown drains
//!
//! Per plan §5.1 the strict ordering assertions run against the mock because
//! gossip-over-loopback is timing-sensitive; the loopback smoke test only
//! verifies end-to-end reachability, not strict ordering.

mod common;

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;

use egregore::feed::engine::FeedEngine;
use egregore::feed::store::FeedStore;
use egregore::transport::{TopicFilter, Transport};

use crate::common::build_chain;
use crate::common::mock_transport::MockTransport;

// ---------------------------------------------------------------------------
// B.1a — Per-author FIFO delivery (Invariant 1).
//
// Two flavors:
//
//   1. MockTransport strict: publish N messages in sequence order, subscribe
//      on a second mock, assert exact sequence preservation.
//
//   2. Gossip loopback smoke: build two real GossipTransports on loopback,
//      publish N messages through engine_a, subscribe on engine_b's
//      GossipTransport, assert at least ONE message arrives within 5s. Strict
//      ordering over real TCP is flaky — MockTransport carries the strict
//      invariant, loopback verifies end-to-end reachability only.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn b1a_per_author_fifo_delivery_mock() {
    let mock = MockTransport::new();
    let (identity, chain) = build_chain(20);

    // Subscribe BEFORE publishing so no message is missed.
    let filter = TopicFilter {
        authors: Some(vec![identity.public_id()]),
        tags: None,
    };
    let (_handle, mut stream) = mock.subscribe(filter).await.expect("subscribe");

    for msg in &chain {
        mock.publish(msg).await.expect("publish ok");
    }

    let mut observed_seqs = Vec::with_capacity(chain.len());
    let collect = async {
        while observed_seqs.len() < chain.len() {
            match stream.next().await {
                Some(m) => observed_seqs.push(m.sequence),
                None => break,
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(2), collect)
        .await
        .expect("FIFO delivery must complete within 2s");

    // Strict equality — MockTransport guarantees per-author FIFO.
    let expected: Vec<u64> = (1..=chain.len() as u64).collect();
    assert_eq!(
        observed_seqs, expected,
        "per-author FIFO violated: got {observed_seqs:?}, expected {expected:?}"
    );
}

// ---------------------------------------------------------------------------
// B.2 — No silent drop / gap detectability (Invariants 2 + 5).
//
// Publish m_1..=m_10 through a MockTransport with `inject_drop(author, 5)`
// set BEFORE publishing m_5. The mock records every publish to `published`
// (so the sender's Ok-return still honors Invariant 2 at the send level),
// but does not fan out m_5 to subscribers. Then, on the receiver side:
//
//   - Ingest every subscriber-delivered message into a local engine.
//   - Verify `store.get_message_at_sequence(author, 5)` is `None` — the gap
//     is real.
//   - Verify `is_chain_valid(m_6.hash) == false` — msg 6 has no predecessor.
//   - Call `request_from(author, 4)` on the mock; it returns m_5 (which is
//     present in `published`) — ingesting it closes the gap.
//   - Verify `is_chain_valid(m_6.hash) == true` after gap close.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn b2_no_silent_drop_gap_detectable() {
    let mock = MockTransport::new();
    let (identity, chain) = build_chain(10);

    // Inject drop BEFORE publishing m_5.
    mock.inject_drop(identity.public_id(), 5);

    let filter = TopicFilter {
        authors: Some(vec![identity.public_id()]),
        tags: None,
    };
    let (_handle, mut stream) = mock.subscribe(filter).await.expect("subscribe");

    // Publish every message. m_5 is recorded to `published` but not fanned
    // out (silent-drop-at-wire semantics, Invariant 2's edge case).
    for msg in &chain {
        mock.publish(msg).await.expect("publish ok");
    }

    // Receiver engine — ingests whatever the subscriber delivers.
    let local = FeedEngine::new(FeedStore::open_memory().expect("store"));

    // Expected deliveries: 9 messages (all but m_5). Collect with timeout so
    // a broken fan-out does not hang the test.
    let expected_delivered = chain.len() - 1;
    let mut delivered = 0usize;
    let collect = async {
        while delivered < expected_delivered {
            match stream.next().await {
                Some(m) => {
                    // Ingest; for m_6 this will set chain_valid=false because
                    // m_5 is absent.
                    let _ = local.ingest(&m);
                    delivered += 1;
                }
                None => break,
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(2), collect)
        .await
        .expect("expected 9 deliveries within 2s");

    // Gap confirmed — m_5 is not in the local store.
    assert!(
        local
            .store()
            .get_message_at_sequence(&identity.public_id(), 5)
            .expect("store query ok")
            .is_none(),
        "m_5 should be absent locally (wire-dropped) — if present, drop injection failed"
    );

    // m_6 arrived without its predecessor, so chain_valid must be false.
    assert!(
        !local
            .store()
            .is_chain_valid(&chain[5].hash)
            .expect("is_chain_valid"),
        "m_6 should have chain_valid=false while gap at m_5 is open"
    );

    // `published` still records the m_5 attempt — the wire drop did NOT
    // erase the sender's attempt log.
    let published = mock.published();
    assert_eq!(
        published.len(),
        chain.len(),
        "published log must record every publish attempt even when wire-dropped"
    );
    assert!(
        published.iter().any(|m| m.sequence == 5),
        "m_5 must be present in the published log (drop is wire-level, not publish-return)"
    );

    // Close the gap by requesting from after_seq = 4. The mock yields m_5
    // from `published`, which we ingest; chain_valid then promotes for m_6.
    let mut rf = mock
        .request_from(identity.public_id(), 4)
        .await
        .expect("request_from ok");
    let mut filled = false;
    while let Some(m) = rf.next().await {
        if m.sequence == 5 {
            local.ingest(&m).expect("ingest m_5 closes the gap");
            filled = true;
            break;
        }
    }
    assert!(filled, "request_from must yield m_5 to close the gap");

    assert!(
        local
            .store()
            .is_chain_valid(&chain[5].hash)
            .expect("is_chain_valid"),
        "m_6 should be promoted to chain_valid=true after gap close"
    );
}

// ---------------------------------------------------------------------------
// B.3 — At-least-once with dedup (Invariant 3).
//
// Publishing the same signed message twice produces two entries in `published`
// (at-least-once at the wire), but the receiver engine stores only one row and
// emits at-most-once on event_tx (duplicate detection at the engine layer).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn b3_duplicate_publish_idempotent() {
    let mock = MockTransport::new();
    let (_identity, chain) = build_chain(1);
    let msg = &chain[0];

    mock.publish(msg).await.expect("publish ok");
    mock.publish(msg).await.expect("duplicate publish ok");

    assert_eq!(
        mock.published().len(),
        2,
        "at-least-once: mock records both publish attempts"
    );

    // Receiver side — ingest both and verify engine dedup.
    let local = FeedEngine::new(FeedStore::open_memory().expect("store"));
    let mut rx = local.subscribe();

    local.ingest(msg).expect("first ingest ok");
    let second = local.ingest(msg);
    assert!(
        matches!(
            second,
            Err(egregore::error::EgreError::DuplicateMessage { .. })
        ),
        "engine must reject the duplicate with DuplicateMessage, got {second:?}"
    );

    // Engine emits at-most-once on event_tx; drain and count.
    let mut emissions = 0usize;
    while rx.try_recv().is_ok() {
        emissions += 1;
    }
    assert_eq!(
        emissions, 1,
        "engine must emit exactly one event_tx message for a duplicate publish pair"
    );

    // Store has exactly one row.
    assert_eq!(
        local.store().message_count().expect("count"),
        1,
        "engine dedup keeps the store at exactly one row"
    );
}

// ---------------------------------------------------------------------------
// B.5 — Gap detectability via chain truncation (Invariant 5).
//
// 1. Publish m_1..=m_100.
// 2. Call request_from(author, 9) to open a stream.
// 3. Consume through seq == 30. Drop the stream (simulates client close).
// 4. Set truncate_at to simulate broker replay-window pruning for any
//    subsequent request_from; deliver m_80 directly via inject_inbound.
// 5. Assert is_chain_valid(m_80.hash) == false (gap from 30 to 80 open).
// 6. Assert stream end produces no distinguishable error (next().await
//    returns None cleanly).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn b5_gap_detectable_after_truncation() {
    let mock = MockTransport::new();
    let (identity, chain) = build_chain(100);

    for msg in &chain {
        mock.publish(msg).await.expect("publish");
    }

    let local = FeedEngine::new(FeedStore::open_memory().expect("store"));

    // Subscribe for inbound (m_80 injection).
    let filter = TopicFilter {
        authors: Some(vec![identity.public_id()]),
        tags: None,
    };
    let (_sub_handle, mut sub_stream) = mock.subscribe(filter).await.expect("subscribe");

    // Step 1: open request_from(author, 9), consume up to seq 30, drop.
    {
        let mut rf = mock
            .request_from(identity.public_id(), 9)
            .await
            .expect("request_from");
        let mut consumed = 0usize;
        while let Some(m) = rf.next().await {
            local.ingest(&m).expect("ingest");
            consumed += 1;
            if m.sequence == 30 {
                break;
            }
        }
        assert!(
            consumed >= 21,
            "expected to consume at least 21 messages (seqs 10..=30), got {consumed}"
        );
        // rf dropped at end of block — stream ends with no error surfacing.
    }

    // Step 2: stream end produces no distinguishable error. Re-open a fresh
    // request_from stream, truncate at 50, drain to EOS, verify clean None.
    mock.inject_truncate_at(50);
    let mut rf2 = mock
        .request_from(identity.public_id(), 40)
        .await
        .expect("request_from ok");
    loop {
        if rf2.next().await.is_none() {
            break;
        }
    }
    // Reaching this line proves the stream ended via `None` without a
    // panic — the Invariant 5 contract: stream end is not a distinguishable
    // error.

    // Step 3: inject m_80 via inbound subscribe path. Ingest it; chain_valid
    // must be false because the gap from 30..80 is still open.
    mock.inject_inbound(chain[79].clone());
    let m80 = tokio::time::timeout(Duration::from_secs(1), sub_stream.next())
        .await
        .expect("m_80 delivery within 1s")
        .expect("sub_stream yielded m_80");
    assert_eq!(m80.sequence, 80);
    let _ = local.ingest(&m80); // stored even if chain_valid=false

    assert!(
        !local
            .store()
            .is_chain_valid(&chain[79].hash)
            .expect("is_chain_valid"),
        "m_80 ingested with seqs 31..79 absent must have chain_valid=false"
    );
}

// ---------------------------------------------------------------------------
// B.7 — Shutdown drains (Invariant 7).
//
// With slow_ack mode enabled, spawn N concurrent publishes; each one takes
// ~50ms to resolve. Immediately call shutdown(5s). Assert:
//   - every publish resolves (Ok or Err) — not pending forever.
//   - inflight counter returns to 0 after shutdown completes.
//   - shutdown returns within its deadline (5s).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn b7_shutdown_drains() {
    let mock = MockTransport::new();
    let (_identity, chain) = build_chain(5);
    mock.set_slow_ack(true);

    // Spawn 5 concurrent publishes. Each takes ~50ms due to slow_ack.
    let mut handles = Vec::new();
    for msg in &chain {
        let t = Arc::clone(&mock);
        let msg = msg.clone();
        handles.push(tokio::spawn(async move { t.publish(&msg).await }));
    }

    // Shutdown almost immediately.
    let sd_start = std::time::Instant::now();
    mock.shutdown(Duration::from_secs(5))
        .await
        .expect("shutdown ok");
    let sd_elapsed = sd_start.elapsed();
    assert!(
        sd_elapsed < Duration::from_secs(5),
        "shutdown must return within deadline, took {sd_elapsed:?}"
    );

    // Every publish resolves (Ok or Err) — not pending forever.
    for h in handles {
        tokio::time::timeout(Duration::from_secs(2), h)
            .await
            .expect("join within 2s post-shutdown")
            .expect("task did not panic")
            .expect("publish resolved");
    }

    // Inflight returns to 0.
    assert_eq!(
        mock.health().inflight_publishes,
        0,
        "inflight must return to 0 after shutdown drains"
    );
}

// ---------------------------------------------------------------------------
// B.1a smoke — Gossip loopback end-to-end reachability.
//
// NOT a strict FIFO assertion — loopback gossip is timing-sensitive per plan
// §5.1. This test just verifies the real GossipTransport::subscribe wiring
// produces at least one delivered message within a 5s window when driven by
// a local engine.ingest on the receiving side.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn b1a_smoke_gossip_subscribe_reaches_engine() {
    // Instead of spinning up two TCP-connected gossip transports (which
    // requires a full SHS handshake + replication loop and reintroduces the
    // flakiness this test is meant to avoid), we exercise the direct path:
    //
    //   GossipTransport.subscribe(filter) MUST yield a message when the
    //   engine emits one via its broadcast channel.
    //
    // This is the one Step 9 behavior on the real transport: bridging
    // engine.event_tx into a filtered BoxStream. The full TCP round-trip is
    // already covered by tests/gossip_test.rs (two_instance_gossip_replication)
    // and the dedicated engine-level dedup suite (engine_dedup_proptest.rs).

    use egregore::gossip::client::SyncConfig;
    use egregore::gossip::registry::ConnectionRegistry;
    use egregore::gossip::server::ServerConfig;
    use egregore::identity::Identity;
    use egregore::transport::gossip::{GossipTransport, GossipTransportConfig};

    let identity = Identity::generate();
    let store = FeedStore::open_memory().expect("store");
    let engine = Arc::new(FeedEngine::new(store));
    let registry = Arc::new(ConnectionRegistry::new(32));
    let network_key = [0u8; 32];
    let server_config = ServerConfig {
        bind_addr: "127.0.0.1:0".into(),
        network_key,
        identity: identity.clone(),
        push_enabled: false,
        max_persistent_connections: 32,
    };
    let sync_config = SyncConfig::new(
        Vec::new(),
        network_key,
        identity.clone(),
        Duration::from_secs(3600), // long — we do not wait on this
        false,
    );
    let cfg = GossipTransportConfig {
        registry: registry.clone(),
        identity: identity.clone(),
        engine: engine.clone(),
        server_config,
        sync_config,
    };
    let transport = GossipTransport::new(cfg);

    // Subscribe BEFORE publishing.
    let filter = TopicFilter {
        authors: Some(vec![identity.public_id()]),
        tags: None,
    };
    let (_handle, mut stream) = transport.subscribe(filter).await.expect("subscribe ok");

    // Drive the engine's event_tx via a normal engine.publish. The
    // GossipTransport's subscribe stream is sourced from engine.subscribe(),
    // so this must surface exactly one message.
    engine
        .publish(
            &identity,
            serde_json::json!({ "type": "test", "n": 1 }),
            None,
            vec![],
        )
        .expect("engine publish ok");

    let got = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("subscribe must deliver within 5s")
        .expect("subscribe stream yielded a message");
    assert_eq!(
        got.author,
        identity.public_id(),
        "delivered message author must match the filter"
    );
    assert_eq!(got.sequence, 1, "first publish has sequence 1");
}
