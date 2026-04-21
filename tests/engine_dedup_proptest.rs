//! Engine-level property tests for `FeedEngine::ingest` under chaos.
//!
//! Phase 1 plan §5.2 B.1b — codifies the load-bearing invariants of today's
//! production `ingest` path as a regression shield. These tests were added
//! AFTER the code shipped; they specify the contract, not drive new behavior.
//!
//! Invariants exercised (RFC 0001 §6):
//!   - Invariant 3: at-least-once delivery with duplicates allowed at the
//!     storage layer — duplicates must surface as `Err(DuplicateMessage)`,
//!     never as silent `Ok(())` (would break dedup accounting and emit
//!     duplicate broadcast events).
//!   - Invariant 5 (partial): chain gap detectability via the `previous`
//!     hash link. A message whose immediate predecessor is present must have
//!     `chain_valid == true`; backfill of a gap must promote the successor's
//!     flag.
//!
//! Out of scope (exercised elsewhere): signature correctness, schema
//! validation, key-rotation enforcement, cross-author interleaving, transport
//! behavior (B.1a lives in Step 9 against MockTransport).

use std::sync::Arc;

use egregore::feed::engine::FeedEngine;
use egregore::feed::models::Message;
use egregore::feed::store::FeedStore;
use egregore::identity::Identity;

use proptest::prelude::*;
use tokio::sync::broadcast::error::TryRecvError;

/// Build a chain of `n` signed messages for a fresh identity.
///
/// Spins up a throwaway "remote" engine, calls `publish` n times, returns the
/// resulting signed `Message`s in sequence order. The remote engine is dropped
/// on return; only the `Vec<Message>` outlives this call.
///
/// `n` must be >= 1. Content is a trivial `{"type":"test","n":i}` payload —
/// schema validation is non-strict in this test setup, so unknown `type` is
/// allowed.
fn build_chain(n: usize) -> Vec<Message> {
    assert!(n >= 1, "chain length must be at least 1");
    let remote_store = FeedStore::open_memory().expect("open remote store");
    let remote_engine = FeedEngine::new(remote_store);
    let identity = Identity::generate();

    let mut chain = Vec::with_capacity(n);
    for i in 1..=n {
        let msg = remote_engine
            .publish(
                &identity,
                serde_json::json!({"type": "test", "n": i}),
                None,
                vec![],
            )
            .expect("publish on remote engine");
        chain.push(msg);
    }
    chain
}

/// Fresh in-memory "local" engine for ingest testing.
fn fresh_local_engine() -> FeedEngine {
    let store = FeedStore::open_memory().expect("open local store");
    FeedEngine::new(store)
}

/// Drain a broadcast receiver, counting emissions. Returns the number of
/// `Arc<Message>` items that were buffered. A `Lagged` error means the
/// receiver missed events (channel overflow) — we treat that as a test
/// failure so silent event loss is caught.
fn drain_emissions(rx: &mut tokio::sync::broadcast::Receiver<Arc<Message>>) -> usize {
    let mut count = 0;
    loop {
        match rx.try_recv() {
            Ok(_) => count += 1,
            Err(TryRecvError::Empty) => return count,
            Err(TryRecvError::Closed) => return count,
            Err(TryRecvError::Lagged(n)) => {
                panic!(
                    "broadcast receiver lagged by {n} — raise channel capacity or reduce case size"
                );
            }
        }
    }
}

// Proptest config: 64 cases per test. Each case performs up to ~60 SQLite
// writes (publish on remote + ingest on local), so the full b1b suite
// budgets ~15k writes. Empirically fits well under the 30s target on the
// dev box. Do NOT drop below 50 without noting the tradeoff.
fn config() -> ProptestConfig {
    ProptestConfig::with_cases(64)
}

// ---------------------------------------------------------------------------
// Test 1 — duplicate-idempotent: re-ingesting any already-stored hash returns
// `Err(DuplicateMessage)`, never silent `Ok(())`. After feeding a permuted
// sequence with duplicates, the store's unique-message count equals the chain
// length (N), never N + duplicates.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(config())]

    #[test]
    fn b1b_duplicate_idempotent(
        n in 5usize..=30,
        duplicate_count in 0usize..=30,
        seed in any::<u64>(),
    ) {
        let chain = build_chain(n);
        let engine = fresh_local_engine();

        // Build feed sequence: the original N messages, plus `duplicate_count`
        // randomly-chosen re-copies. Shuffle the full list deterministically
        // by seed so failures are reproducible via the shrunk `(n, dup, seed)`
        // triple.
        let mut feed: Vec<Message> = chain.clone();
        for i in 0..duplicate_count {
            let pick = (seed.wrapping_mul(1 + i as u64)) as usize % n;
            feed.push(chain[pick].clone());
        }
        // Fisher-Yates shuffle seeded by `seed`.
        let mut state = seed;
        for i in (1..feed.len()).rev() {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let j = (state >> 33) as usize % (i + 1);
            feed.swap(i, j);
        }

        // Track which hashes we've already accepted; every subsequent ingest
        // of that hash MUST fail with DuplicateMessage.
        let mut accepted = std::collections::HashSet::new();
        for msg in &feed {
            let already_present = accepted.contains(&msg.hash);
            match engine.ingest(msg) {
                Ok(()) => {
                    prop_assert!(
                        !already_present,
                        "ingest returned Ok(()) for a hash already in store (author={}, seq={}): \
                         this would mean duplicate emission on event_tx — invariant violated",
                        msg.author.0, msg.sequence
                    );
                    accepted.insert(msg.hash.clone());
                }
                Err(egregore::error::EgreError::DuplicateMessage { .. }) => {
                    prop_assert!(
                        already_present,
                        "ingest returned DuplicateMessage for a hash we had not yet accepted \
                         (author={}, seq={})",
                        msg.author.0, msg.sequence
                    );
                }
                Err(other) => {
                    panic!(
                        "unexpected ingest error for well-formed chain message \
                         (author={}, seq={}): {other}",
                        msg.author.0, msg.sequence
                    );
                }
            }
        }

        // Final invariant: store contains exactly N unique messages, not N + dups.
        let stored = engine.store().message_count().expect("message_count");
        prop_assert_eq!(
            stored, n as u64,
            "store count ({}) != unique chain length ({}) after permuted ingest with {} duplicates",
            stored, n, duplicate_count
        );
    }
}

// ---------------------------------------------------------------------------
// Test 2 — out-of-order backfill: arbitrary permutation of a single-author
// chain results in all N messages stored, and every message whose
// predecessor ends up present also ends up chain_valid.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(config())]

    #[test]
    fn b1b_out_of_order_backfill(
        n in 5usize..=30,
        seed in any::<u64>(),
    ) {
        let chain = build_chain(n);
        let engine = fresh_local_engine();

        // Permute the chain deterministically.
        let mut order: Vec<usize> = (0..n).collect();
        let mut state = seed;
        for i in (1..n).rev() {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let j = (state >> 33) as usize % (i + 1);
            order.swap(i, j);
        }

        for &idx in &order {
            engine.ingest(&chain[idx]).expect("ingest of fresh chain message must succeed");
        }

        // All N present.
        let stored = engine.store().message_count().expect("count");
        prop_assert_eq!(stored, n as u64);

        // For every message whose predecessor is in the store (which is all
        // of them, post-permutation), chain_valid must be true. Message 1 has
        // no predecessor and is always chain_valid.
        for msg in &chain {
            let valid = engine.store().is_chain_valid(&msg.hash).expect("is_chain_valid");
            prop_assert!(
                valid,
                "message at seq {} has predecessor present but chain_valid=false — \
                 backfill promotion failed",
                msg.sequence
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Test 3 — gap-close promotes: creating a gap at seq 5 in a chain of 10
// leaves msg 6 chain_valid=false; filling the gap promotes msg 6 (and by
// extension the tail) to chain_valid=true. Non-probabilistic — a single
// deterministic scenario run under the proptest harness for format
// consistency and cheap repeat.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4))]

    #[test]
    fn b1b_gap_close_promotes(_seed in any::<u64>()) {
        let chain = build_chain(10);
        let engine = fresh_local_engine();

        // Ingest 1..=4 and 6..=10 — skip msg 5 (chain[4]).
        for (i, msg) in chain.iter().enumerate().take(10) {
            if i == 4 {
                continue;
            }
            engine.ingest(msg).expect("ingest");
        }

        // msg 6 has no predecessor in store yet — chain_valid=false.
        prop_assert!(
            !engine.store().is_chain_valid(&chain[5].hash).expect("is_chain_valid"),
            "msg 6 should be chain_valid=false while gap at msg 5 is open"
        );

        // Fill the gap.
        engine.ingest(&chain[4]).expect("ingest gap filler");

        // msg 5's successor (msg 6) must now be chain_valid.
        prop_assert!(
            engine.store().is_chain_valid(&chain[5].hash).expect("is_chain_valid"),
            "msg 6 should be promoted to chain_valid=true after gap close"
        );
        // msg 5 itself is chain_valid (its predecessor msg 4 was already in
        // store when it was ingested).
        prop_assert!(
            engine.store().is_chain_valid(&chain[4].hash).expect("is_chain_valid"),
            "msg 5 should be chain_valid=true on ingest (predecessor msg 4 present)"
        );
        // Note: msg 7..=10 chain_valid state is set at their own ingest time
        // based on whether their immediate predecessor was then present.
        // validate_chain_links only promotes the IMMEDIATE successor on
        // insert — tail promotion is not cascaded. This matches engine.rs
        // behavior circa the plan's snapshot of the code.
    }
}

// ---------------------------------------------------------------------------
// Test 4 — comprehensive chaos: Vec<Action> strategy lets the shrinker
// produce a minimal action trace when a case fails. Asserts all four
// invariants from plan §5.2 simultaneously.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Action {
    /// Ingest the message at index `idx` in the chain (may be the first time
    /// or a repeat — invariants are the same from `ingest`'s POV).
    Ingest(usize),
    /// Explicit duplicate — semantically identical to `Ingest` for the
    /// engine, but distinguished here so shrinker output is readable.
    IngestDuplicate(usize),
}

fn action_strategy(n: usize) -> impl Strategy<Value = Action> {
    prop_oneof![
        (0..n).prop_map(Action::Ingest),
        (0..n).prop_map(Action::IngestDuplicate),
    ]
}

fn chaos_strategy() -> impl Strategy<Value = (usize, Vec<Action>)> {
    (5usize..=20).prop_flat_map(|n| {
        // Between N and 3N actions — enough to produce duplicates and
        // reorderings without blowing up runtime.
        let action_count = n..=(3 * n);
        (
            Just(n),
            proptest::collection::vec(action_strategy(n), action_count),
        )
    })
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn b1b_random_permutation((n, actions) in chaos_strategy()) {
        let chain = build_chain(n);
        let engine = fresh_local_engine();
        let mut rx = engine.subscribe();

        let mut accepted: std::collections::HashSet<String> = std::collections::HashSet::new();

        for action in &actions {
            let msg = match action {
                Action::Ingest(i) | Action::IngestDuplicate(i) => &chain[*i],
            };
            let already = accepted.contains(&msg.hash);
            match engine.ingest(msg) {
                Ok(()) => {
                    prop_assert!(
                        !already,
                        "invariant A violated (dedup): ingest returned Ok for already-stored hash \
                         at seq {}",
                        msg.sequence
                    );
                    accepted.insert(msg.hash.clone());
                }
                Err(egregore::error::EgreError::DuplicateMessage { .. }) => {
                    prop_assert!(
                        already,
                        "invariant B violated: DuplicateMessage returned for hash not yet seen \
                         at seq {}",
                        msg.sequence
                    );
                }
                Err(other) => {
                    panic!(
                        "unexpected error from well-formed ingest at seq {}: {other}",
                        msg.sequence
                    );
                }
            }
        }

        // Invariant C: store count == unique hashes seen.
        let stored = engine.store().message_count().expect("count");
        prop_assert_eq!(
            stored, accepted.len() as u64,
            "store count ({}) != accepted hash count ({})", stored, accepted.len()
        );

        // Invariant D: event_tx emitted exactly once per unique hash. We
        // subscribed before the action loop; drain the buffered emissions.
        let emitted = drain_emissions(&mut rx);
        prop_assert_eq!(
            emitted, accepted.len(),
            "broadcast emissions ({}) != unique accepted messages ({}) — \
             duplicate emission or silent drop",
            emitted, accepted.len()
        );

        // Invariant E: every message whose predecessor is also in the store
        // is chain_valid. For a single-author chain, this reduces to: every
        // accepted message except possibly the lowest-seq accepted (if it
        // arrived before its predecessor) should be chain_valid.
        for msg in &chain {
            if !accepted.contains(&msg.hash) {
                continue;
            }
            let valid = engine.store().is_chain_valid(&msg.hash).expect("is_chain_valid");
            if msg.sequence == 1 {
                prop_assert!(valid, "seq 1 must always be chain_valid");
                continue;
            }
            // Predecessor index is (sequence - 1) in the original chain, which
            // is chain[sequence - 2].
            let pred = &chain[(msg.sequence - 2) as usize];
            let pred_accepted = accepted.contains(&pred.hash);
            if pred_accepted {
                prop_assert!(
                    valid,
                    "seq {} has predecessor seq {} in store but chain_valid=false — \
                     promotion failed",
                    msg.sequence, pred.sequence
                );
            }
            // If pred is NOT accepted, chain_valid may be true or false
            // depending on arrival order — not asserted here.
        }
    }
}
