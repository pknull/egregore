//! Transport-agnostic invariant tests — run against `MockTransport` so the
//! invariants under test are isolated from real wire timing (plan §5).
//!
//! Covers:
//! - B.4 envelope preservation (Invariant 4)
//! - B.6 filter honesty (Invariant 6)
//!
//! Wire-specific tests (B.1a loopback smoke, B.2, B.3, B.5, B.7) live in
//! `transport_invariants_gossip.rs` where real transport timing and
//! fault-injection primitives matter.

mod common;

use std::time::Duration;

use futures::StreamExt;

use egregore::feed::models::Message;
use egregore::transport::{TopicFilter, Transport};

use crate::common::build_tagged_chain;
use crate::common::mock_transport::MockTransport;

// ---------------------------------------------------------------------------
// B.4 — Envelope preservation (Invariant 4).
//
// Every byte of the signed message delivered to the transport must be
// preserved verbatim through to both:
//   - the `published` log (records sender-side bytes exactly)
//   - any subscriber's delivery (forwards bytes exactly)
//
// The mock transport is a pure wire emulation — it does not re-sign, re-hash,
// or re-serialize. This test guards against any future mock evolution that
// would accidentally mutate. The trait contract (RFC 0001 §6 Invariant 4)
// requires the same of every real adapter; this is the transport-agnostic
// fixture.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn b4_envelope_preservation() {
    let mock = MockTransport::new();
    let (_identity, chain) = build_tagged_chain(3, vec!["t1".into()]);

    for msg in &chain {
        mock.publish(msg).await.expect("publish ok");
    }

    let captured = mock.published();
    assert_eq!(
        captured.len(),
        chain.len(),
        "published log must record every publish attempt"
    );

    for (before, after) in chain.iter().zip(captured.iter()) {
        // Byte-identical JSON serialization — canonical Serde ordering means
        // this comparison is the strongest lower bound short of memcmp.
        let before_bytes = serde_json::to_vec(before).expect("serialize before");
        let after_bytes = serde_json::to_vec(after).expect("serialize after");
        assert_eq!(
            before_bytes, after_bytes,
            "envelope mutated between publish call and published-log entry: \
             author={} seq={}",
            before.author.0, before.sequence
        );

        // Hash and signature MUST survive the wire exactly.
        assert_eq!(before.hash, after.hash, "hash field mutated");
        assert_eq!(before.signature, after.signature, "signature field mutated");
    }
}

// ---------------------------------------------------------------------------
// B.6 — Filter honesty (Invariant 6).
//
// `subscribe(filter)` delivers AT LEAST the matching set; superset delivery
// is permitted, subset is not. We publish a mix of messages — some matching
// the filter, some not — and assert every matching one arrives. We do NOT
// assert "only matching" because the invariant allows superset.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn b6_filter_honesty() {
    let mock = MockTransport::new();

    // Two authors with distinct tag sets.
    let (author_a, chain_a) = build_tagged_chain(5, vec!["ops".into()]);
    let (author_b, chain_b) = build_tagged_chain(5, vec!["research".into()]);

    // Subscribe with an author-specific filter BEFORE publishing, so the
    // sub is live for every fan-out.
    let filter = TopicFilter {
        authors: Some(vec![author_a.public_id()]),
        tags: None,
    };
    let (_handle, mut stream) = mock
        .subscribe(filter)
        .await
        .expect("subscribe on mock transport");

    // Publish a mix; expect every chain_a message to be delivered.
    for (a, b) in chain_a.iter().zip(chain_b.iter()) {
        mock.publish(a).await.expect("publish a");
        mock.publish(b).await.expect("publish b");
    }

    // Collect deliveries within a bounded timeout so a broken filter does not
    // hang the test indefinitely.
    let mut delivered: Vec<Message> = Vec::new();
    let collect = async {
        while let Some(m) = stream.next().await {
            delivered.push(m);
            if delivered.len() >= chain_a.len() {
                break;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(2), collect)
        .await
        .expect("filter delivery must complete within 2s — if it times out, \
                 the filter dropped a matching message (Invariant 6 subset bug)");

    // Invariant 6 lower bound: every chain_a message is present.
    for a in &chain_a {
        assert!(
            delivered.iter().any(|m| m.hash == a.hash),
            "filter dropped a matching message (author={}, seq={}) — \
             this is an Invariant 6 subset bug",
            a.author.0, a.sequence
        );
    }

    // Invariant 6 upper bound permits extras — we assert nothing here about
    // the presence/absence of chain_b. The mock happens to honor the filter
    // strictly; a real transport MAY deliver more without violating the
    // invariant.
    let _ = author_b; // keep author_b alive to silence unused-binding warnings
}
