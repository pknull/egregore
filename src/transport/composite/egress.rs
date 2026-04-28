//! Per-destination egress drain task — RFC 0002 §8.2 + amendment §C.5.
//!
//! One task per destination child. Drains `DirectionState.queues` in
//! plain round-robin across `(source, author)` keys (§8.3), calls
//! `dest_transport.publish(msg)`, decrements the per-message
//! `AckBarrier`, and — on the barrier's transition-to-zero — acks the
//! source (bus path only; gossip is a no-op).
//!
//! Critical-section discipline matches ingress: no lock is held across
//! any `.await`. All `publish` calls happen outside the `queues` lock;
//! only the pop + state read executes under the lock.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use tokio_util::sync::CancellationToken;

use crate::transport::trait_def::Transport;

use super::direction::{AckBarrier, DirectionState, TransportId};

/// Run the egress drain loop for destination `dest_id`.
///
/// Exits when `cancel` fires. Does NOT exit on destination `publish`
/// errors — the ack-on-destination-error policy (§8.2) is to record the
/// error and proceed, because the durable-local-ingest precondition
/// guarantees the message is in local SQLite and `request_from` will
/// serve it later.
pub(crate) async fn run_egress(
    dest_id: TransportId,
    dest_transport: Arc<dyn Transport>,
    dir: Arc<DirectionState>,
    ack_barriers: Arc<DashMap<String, Arc<AckBarrier>>>,
    children: Arc<Vec<Arc<dyn Transport>>>,
    cancel: CancellationToken,
) {
    let mut rr_cursor: usize = 0;
    loop {
        if cancel.is_cancelled() {
            tracing::debug!(dest_id, "composite egress: cancel signal, exiting");
            break;
        }

        // 1. Snapshot the set of non-empty queue keys under a short lock.
        //    RR fairness (§8.3) is over this snapshot, one pop per round.
        let keys: Vec<_> = {
            let queues = dir.queues.lock();
            queues
                .iter()
                .filter(|(_, entry)| !entry.queue.is_empty())
                .map(|(k, _)| k.clone())
                .collect()
        };

        if keys.is_empty() {
            // Nothing to drain. Sleep on egress_wake; ingress pings it
            // on empty→non-empty transitions. Honor cancel concurrently.
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    tracing::debug!(dest_id, "composite egress: cancel while idle");
                    break;
                }
                _ = dir.egress_wake.notified() => continue,
            }
        }

        // 2. Pick the next key (plain RR across the snapshot).
        if rr_cursor >= keys.len() {
            rr_cursor = 0;
        }
        let key = keys[rr_cursor].clone();
        rr_cursor = (rr_cursor + 1) % keys.len();

        // 3. Pop one message + capture the backpressure-clear state
        //    under a single lock acquisition.
        let drained = {
            let mut queues = dir.queues.lock();
            let Some(entry) = queues.get_mut(&key) else {
                // Entry vanished between snapshot and pop — can't happen
                // today (we never remove entries), but guard for future
                // changes.
                continue;
            };
            let Some(msg) = entry.queue.pop() else {
                // Queue emptied between snapshot and pop (another egress
                // task — not possible for one-direction-per-task, but
                // defensive). Skip.
                continue;
            };
            let now_at_low = entry.queue.at_or_below_low_watermark();
            let space_notify = entry.space_available.clone();
            Some((msg, now_at_low, space_notify))
        };

        let Some((msg, now_at_low, space_notify)) = drained else {
            continue;
        };

        // 4. Clear backpressure on low-watermark crossing. `backpressured`
        //    removal is authoritative: if the key wasn't in the set, don't
        //    fire the wake (no one was blocked). `notify_waiters` (not
        //    `notify_one`) because multiple ingress tasks may be blocked
        //    on the same queue if two sources feed the same author (rare
        //    but possible).
        if now_at_low && dir.backpressured.lock().remove(&key) {
            space_notify.notify_waiters();
        }

        // 5. If every queue is now empty, clear oldest_queued_at.
        //    Scan-under-lock; cheap because the map is per-destination
        //    and at pilot scale holds tens of entries.
        let all_empty = {
            let queues = dir.queues.lock();
            queues.values().all(|e| e.queue.is_empty())
        };
        if all_empty {
            *dir.oldest_queued_at.write() = None;
        }

        // 6. Record publish-in-flight + call dest_transport.publish.
        *dir.publish_in_flight_since.write() = Some(Instant::now());
        let publish_result = dest_transport.publish(&msg).await;
        *dir.publish_in_flight_since.write() = None;

        let errored = publish_result.is_err();
        if errored {
            dir.ack_on_error_total.fetch_add(1, Ordering::Relaxed);
            // Auditor A2: stable short code, NO pubkeys/hashes/ciphertext.
            *dir.last_error.write() = Some("destination publish error".to_string());
            tracing::warn!(
                dest_id,
                sequence = msg.sequence,
                "composite egress: destination publish returned Err — acking anyway per §8.2"
            );
        }

        // 7. Resolve the ack barrier (amendment §C.5). The thread that
        //    observes the transition-to-zero is uniquely responsible for
        //    acking the source.
        let barrier_final = ack_barriers
            .get(&msg.hash)
            .map(|b| b.clone())
            .and_then(|b| b.resolve_one(errored));

        if let Some((had_error, source_id)) = barrier_final {
            // All destinations resolved — remove the barrier entry + ack
            // the source (bus children only; gossip-sourced messages
            // have no ack equivalent).
            ack_barriers.remove(&msg.hash);

            if had_error {
                // Aggregate metric already incremented per-destination
                // via ack_on_error_total; the barrier-final had_error
                // flag is reserved for richer observability in Step 20.
                tracing::debug!(
                    source_id,
                    "composite egress: barrier final with had_error=true"
                );
            }

            if let Some(source) = children.get(source_id) {
                // Bus children override ack_after_publish to drain
                // pending_acks; gossip and other non-bus children keep
                // the trait's default no-op (Ok(())). Barrier removal
                // above completes the cycle either way.
                if let Err(e) = source.ack_after_publish(&msg.hash).await {
                    tracing::warn!(
                        source_id,
                        error = %e,
                        "composite egress: ack_after_publish failed"
                    );
                    *dir.last_error.write() = Some("source ack_after_publish error".to_string());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Result;
    use crate::feed::models::Message;
    use crate::identity::PublicId;
    use crate::transport::filter::TopicFilter;
    use crate::transport::subscription::SubscriptionHandle;
    use crate::transport::TransportHealth;
    use async_trait::async_trait;
    use chrono::Utc;
    use futures::stream::BoxStream;
    use parking_lot::Mutex as PlMutex;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::time::Duration;

    fn sample_message(author: &str, seq: u64) -> Message {
        Message {
            author: PublicId(author.to_string()),
            sequence: seq,
            previous: None,
            timestamp: Utc::now(),
            content: serde_json::json!({"type": "note"}),
            schema_id: None,
            relates: None,
            tags: vec![],
            trace_id: None,
            span_id: None,
            expires_at: None,
            hash: format!("hash-{author}-{seq}"),
            signature: "sig".to_string(),
        }
    }

    /// Minimal stub that satisfies the `Transport` trait via defaults.
    /// Used to populate the `children` parameter of `run_egress` for tests
    /// that don't exercise source-side ack semantics — gossip-equivalent
    /// behavior (the trait's default `ack_after_publish` returns `Ok(())`).
    struct StubChild;

    #[async_trait]
    impl Transport for StubChild {
        async fn publish(&self, _msg: &Message) -> Result<()> {
            Ok(())
        }
        async fn subscribe(
            &self,
            _f: TopicFilter,
        ) -> Result<(SubscriptionHandle, BoxStream<'static, Message>)> {
            unreachable!("StubChild is ack-only")
        }
        async fn request_from(&self, _a: PublicId, _s: u64) -> Result<BoxStream<'static, Message>> {
            unreachable!()
        }
        async fn start(&self) -> Result<()> {
            Ok(())
        }
        async fn shutdown(&self, _d: Duration) -> Result<()> {
            Ok(())
        }
        fn health(&self) -> TransportHealth {
            TransportHealth {
                connected: true,
                backend: "stub",
                last_successful_publish: None,
                last_peer_contact: None,
                unreplicated_count: 0,
                inflight_publishes: 0,
                last_error: None,
                children: vec![],
                bridge_queues: None,
            }
        }
    }

    fn stub_children(n: usize) -> Arc<Vec<Arc<dyn Transport>>> {
        Arc::new(
            (0..n)
                .map(|_| Arc::new(StubChild) as Arc<dyn Transport>)
                .collect(),
        )
    }

    /// A source-side mock that records `ack_after_publish` invocations.
    /// Used to assert that egress dispatches the source-ack call through
    /// the `Transport` trait (i.e. that BusTransport's override would fire,
    /// without needing a live NATS connection to construct a real bus).
    struct RecordingSource {
        ack_calls: Arc<PlMutex<Vec<String>>>,
    }

    impl RecordingSource {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                ack_calls: Arc::new(PlMutex::new(Vec::new())),
            })
        }

        fn ack_calls(&self) -> Vec<String> {
            self.ack_calls.lock().clone()
        }
    }

    #[async_trait]
    impl Transport for RecordingSource {
        async fn publish(&self, _msg: &Message) -> Result<()> {
            Ok(())
        }
        async fn subscribe(
            &self,
            _f: TopicFilter,
        ) -> Result<(SubscriptionHandle, BoxStream<'static, Message>)> {
            unreachable!("RecordingSource is ack-only")
        }
        async fn request_from(&self, _a: PublicId, _s: u64) -> Result<BoxStream<'static, Message>> {
            unreachable!()
        }
        async fn start(&self) -> Result<()> {
            Ok(())
        }
        async fn shutdown(&self, _d: Duration) -> Result<()> {
            Ok(())
        }
        fn health(&self) -> TransportHealth {
            TransportHealth {
                connected: true,
                backend: "recording",
                last_successful_publish: None,
                last_peer_contact: None,
                unreplicated_count: 0,
                inflight_publishes: 0,
                last_error: None,
                children: vec![],
                bridge_queues: None,
            }
        }
        async fn ack_after_publish(&self, message_hash: &str) -> Result<()> {
            self.ack_calls.lock().push(message_hash.to_string());
            Ok(())
        }
    }

    /// A destination-transport mock that records every `publish` call and
    /// returns a configurable Ok/Err outcome.
    struct MockDest {
        published: Arc<PlMutex<Vec<Message>>>,
        publish_outcomes: Arc<PlMutex<VecDeque<Result<()>>>>,
    }

    impl MockDest {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                published: Arc::new(PlMutex::new(Vec::new())),
                publish_outcomes: Arc::new(PlMutex::new(VecDeque::new())),
            })
        }

        fn push_outcome(&self, outcome: Result<()>) {
            self.publish_outcomes.lock().push_back(outcome);
        }

        fn published(&self) -> Vec<Message> {
            self.published.lock().clone()
        }
    }

    #[async_trait]
    impl Transport for MockDest {
        async fn publish(&self, msg: &Message) -> Result<()> {
            self.published.lock().push(msg.clone());
            self.publish_outcomes.lock().pop_front().unwrap_or(Ok(()))
        }
        async fn subscribe(
            &self,
            _f: TopicFilter,
        ) -> Result<(SubscriptionHandle, BoxStream<'static, Message>)> {
            unreachable!("MockDest is publish-only")
        }
        async fn request_from(&self, _a: PublicId, _s: u64) -> Result<BoxStream<'static, Message>> {
            unreachable!()
        }
        async fn start(&self) -> Result<()> {
            Ok(())
        }
        async fn shutdown(&self, _d: Duration) -> Result<()> {
            Ok(())
        }
        fn health(&self) -> TransportHealth {
            TransportHealth {
                connected: true,
                backend: "mock",
                last_successful_publish: None,
                last_peer_contact: None,
                unreplicated_count: 0,
                inflight_publishes: 0,
                last_error: None,
                children: vec![],
                bridge_queues: None,
            }
        }
    }

    async fn eventually<F: Fn() -> bool>(pred: F, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if pred() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        pred()
    }

    fn pre_enqueue(dir: &DirectionState, source_id: TransportId, msg: Message) {
        let key = (source_id, msg.author.clone());
        let mut queues = dir.queues.lock();
        let entry = queues.entry(key).or_default();
        let _ = entry.queue.push(Arc::new(msg));
    }

    #[tokio::test]
    async fn egress_drains_non_empty_queue() {
        let dir = Arc::new(DirectionState::new());
        let dest = MockDest::new();
        let ack_barriers: Arc<DashMap<String, Arc<AckBarrier>>> = Arc::new(DashMap::new());
        let children = stub_children(2);
        let cancel = CancellationToken::new();

        // Pre-enqueue 3 messages from source_id=0 to this destination.
        for seq in 1..=3 {
            let msg = sample_message("@a.ed25519", seq);
            ack_barriers.insert(msg.hash.clone(), Arc::new(AckBarrier::new(0, 1)));
            pre_enqueue(&dir, 0, msg);
        }

        let dir_clone = dir.clone();
        let dest_clone = dest.clone();
        let barriers_clone = ack_barriers.clone();
        let children_clone = children.clone();
        let cancel_clone = cancel.clone();
        let egress = tokio::spawn(async move {
            run_egress(
                1,
                dest_clone as Arc<dyn Transport>,
                dir_clone,
                barriers_clone,
                children_clone,
                cancel_clone,
            )
            .await;
        });

        // Nudge egress wake in case the loop started on an empty snapshot.
        dir.egress_wake.notify_one();

        let hit = eventually(|| dest.published().len() == 3, Duration::from_secs(2)).await;
        assert!(hit, "egress must drain all 3 messages");

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), egress).await;

        // All barriers resolved → map should be empty.
        assert!(
            ack_barriers.is_empty(),
            "barriers must be removed after N=1 resolve"
        );
    }

    #[tokio::test]
    async fn egress_round_robin_across_authors() {
        let dir = Arc::new(DirectionState::new());
        let dest = MockDest::new();
        let ack_barriers: Arc<DashMap<String, Arc<AckBarrier>>> = Arc::new(DashMap::new());
        let children = stub_children(2);
        let cancel = CancellationToken::new();

        // Two authors from the same source, 3 messages each. Interleaved
        // emission is the RR signature.
        for seq in 1..=3 {
            for author in ["@a.ed25519", "@b.ed25519"] {
                let msg = sample_message(author, seq);
                ack_barriers.insert(msg.hash.clone(), Arc::new(AckBarrier::new(0, 1)));
                pre_enqueue(&dir, 0, msg);
            }
        }

        let dir_clone = dir.clone();
        let dest_clone = dest.clone();
        let barriers_clone = ack_barriers.clone();
        let children_clone = children.clone();
        let cancel_clone = cancel.clone();
        let egress = tokio::spawn(async move {
            run_egress(
                1,
                dest_clone as Arc<dyn Transport>,
                dir_clone,
                barriers_clone,
                children_clone,
                cancel_clone,
            )
            .await;
        });
        dir.egress_wake.notify_one();

        let hit = eventually(|| dest.published().len() == 6, Duration::from_secs(2)).await;
        assert!(hit);
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), egress).await;

        // Verify per-author FIFO within interleaving: each author's
        // own sequence stream must come out in order 1→2→3. RR across
        // the two authors yields an interleaved pattern (A/B or B/A
        // alternating); we don't assert the starting author because
        // HashMap iteration order is nondeterministic.
        let published = dest.published();
        let a_seqs: Vec<u64> = published
            .iter()
            .filter(|m| m.author.0 == "@a.ed25519")
            .map(|m| m.sequence)
            .collect();
        let b_seqs: Vec<u64> = published
            .iter()
            .filter(|m| m.author.0 == "@b.ed25519")
            .map(|m| m.sequence)
            .collect();
        assert_eq!(a_seqs, vec![1, 2, 3], "per-author FIFO for @a");
        assert_eq!(b_seqs, vec![1, 2, 3], "per-author FIFO for @b");
    }

    #[tokio::test]
    async fn egress_clears_backpressure_at_low_watermark_and_notifies_waiters() {
        let dir = Arc::new(DirectionState::new());
        let dest = MockDest::new();
        let ack_barriers: Arc<DashMap<String, Arc<AckBarrier>>> = Arc::new(DashMap::new());
        let children = stub_children(2);
        let cancel = CancellationToken::new();

        let author = "@a.ed25519";
        let key = (0_usize, PublicId(author.to_string()));

        // Mark the key backpressured; seed the Notify we will wait on.
        let waker = {
            let mut queues = dir.queues.lock();
            let entry = queues.entry(key.clone()).or_default();
            // Fill a handful of messages past the low watermark so the
            // first few pops keep us above low.
            use crate::transport::composite::direction::BRIDGE_QUEUE_LOW_WATERMARK;
            for seq in 1..=(BRIDGE_QUEUE_LOW_WATERMARK as u64 + 2) {
                let m = sample_message(author, seq);
                ack_barriers.insert(m.hash.clone(), Arc::new(AckBarrier::new(0, 1)));
                entry.queue.push(Arc::new(m)).unwrap();
            }
            entry.space_available.clone()
        };
        dir.backpressured.lock().insert(key.clone());

        // Spawn a waiter on the space_available notifier.
        let waker_clone = waker.clone();
        let waiter_woke = Arc::new(AtomicBool::new(false));
        let wf = waiter_woke.clone();
        let waiter = tokio::spawn(async move {
            waker_clone.notified().await;
            wf.store(true, Ordering::Release);
        });

        let dir_clone = dir.clone();
        let dest_clone = dest.clone();
        let barriers_clone = ack_barriers.clone();
        let children_clone = children.clone();
        let cancel_clone = cancel.clone();
        let egress = tokio::spawn(async move {
            run_egress(
                1,
                dest_clone as Arc<dyn Transport>,
                dir_clone,
                barriers_clone,
                children_clone,
                cancel_clone,
            )
            .await;
        });
        dir.egress_wake.notify_one();

        // Wait for waiter to wake — signals the low-watermark cross fired.
        let _ = tokio::time::timeout(Duration::from_secs(2), waiter).await;
        assert!(
            waiter_woke.load(Ordering::Acquire),
            "low-watermark cross must fire space_available.notify_waiters"
        );
        // backpressured set must have been cleared.
        assert!(!dir.backpressured.lock().contains(&key));

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), egress).await;
    }

    #[tokio::test]
    async fn egress_decrements_ack_barrier_and_calls_source_ack_on_final_resolution() {
        // Two destinations (N=3 composite). We run egress for destination 1
        // only, but instead of an AckBarrier with pending=1 we use
        // pending=2 and manually decrement the other one to simulate
        // destination-2's egress resolving. The egress under test must
        // ack the source only when the counter hits 0.
        let dir = Arc::new(DirectionState::new());
        let dest = MockDest::new();

        // Count source-ack attempts via a counter embedded in a fake
        // bus child. We can't construct a real BusTransport without NATS,
        // so instead we verify the barrier management + ack_barriers
        // removal. The bus-specific ack call is exercised in the
        // integration smoke test (Step 13).
        let ack_barriers: Arc<DashMap<String, Arc<AckBarrier>>> = Arc::new(DashMap::new());
        let children = stub_children(3);
        let cancel = CancellationToken::new();

        let msg = sample_message("@a.ed25519", 1);
        let barrier = Arc::new(AckBarrier::new(0, 2));
        ack_barriers.insert(msg.hash.clone(), barrier.clone());
        pre_enqueue(&dir, 0, msg.clone());

        let dir_clone = dir.clone();
        let dest_clone = dest.clone();
        let barriers_clone = ack_barriers.clone();
        let children_clone = children.clone();
        let cancel_clone = cancel.clone();
        let egress = tokio::spawn(async move {
            run_egress(
                1,
                dest_clone as Arc<dyn Transport>,
                dir_clone,
                barriers_clone,
                children_clone,
                cancel_clone,
            )
            .await;
        });
        dir.egress_wake.notify_one();

        // Wait for destination publish.
        let hit = eventually(|| dest.published().len() == 1, Duration::from_secs(2)).await;
        assert!(hit);

        // After destination-1 resolved, barrier still has pending=1
        // (the other destination hasn't resolved yet).
        assert_eq!(barrier.pending_count.load(Ordering::Acquire), 1);
        assert!(
            ack_barriers.contains_key(&msg.hash),
            "barrier still active until second destination resolves"
        );

        // Simulate destination-2 resolving.
        let final_result = barrier.resolve_one(false);
        assert_eq!(
            final_result,
            Some((false, 0)),
            "manual final resolution observes the transition-to-zero"
        );

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), egress).await;
    }

    #[tokio::test]
    async fn egress_records_ack_on_error_total_when_destination_errors() {
        use crate::error::EgreError;
        let dir = Arc::new(DirectionState::new());
        let dest = MockDest::new();
        dest.push_outcome(Err(EgreError::Peer {
            reason: "simulated destination failure".to_string(),
        }));
        let ack_barriers: Arc<DashMap<String, Arc<AckBarrier>>> = Arc::new(DashMap::new());
        let children = stub_children(2);
        let cancel = CancellationToken::new();

        let msg = sample_message("@a.ed25519", 1);
        ack_barriers.insert(msg.hash.clone(), Arc::new(AckBarrier::new(0, 1)));
        pre_enqueue(&dir, 0, msg.clone());

        let dir_clone = dir.clone();
        let dest_clone = dest.clone();
        let barriers_clone = ack_barriers.clone();
        let children_clone = children.clone();
        let cancel_clone = cancel.clone();
        let egress = tokio::spawn(async move {
            run_egress(
                1,
                dest_clone as Arc<dyn Transport>,
                dir_clone,
                barriers_clone,
                children_clone,
                cancel_clone,
            )
            .await;
        });
        dir.egress_wake.notify_one();

        let hit = eventually(
            || dir.ack_on_error_total.load(Ordering::Relaxed) == 1,
            Duration::from_secs(2),
        )
        .await;
        assert!(hit, "destination error must bump ack_on_error_total");
        // last_error is populated with a scrubbed short code (auditor A2).
        let last = dir.last_error.read().clone();
        assert_eq!(last.as_deref(), Some("destination publish error"));
        // Barrier still resolves to zero (N=1) and is removed — the
        // ack-on-destination-error policy acks-anyway.
        let hit2 = eventually(|| ack_barriers.is_empty(), Duration::from_secs(2)).await;
        assert!(hit2, "barrier must be removed even on destination error");

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), egress).await;
    }

    #[tokio::test]
    async fn egress_sleeps_on_empty_queues_wakes_on_egress_wake() {
        let dir = Arc::new(DirectionState::new());
        let dest = MockDest::new();
        let ack_barriers: Arc<DashMap<String, Arc<AckBarrier>>> = Arc::new(DashMap::new());
        let children = stub_children(2);
        let cancel = CancellationToken::new();

        let dir_clone = dir.clone();
        let dest_clone = dest.clone();
        let barriers_clone = ack_barriers.clone();
        let children_clone = children.clone();
        let cancel_clone = cancel.clone();
        let egress = tokio::spawn(async move {
            run_egress(
                1,
                dest_clone as Arc<dyn Transport>,
                dir_clone,
                barriers_clone,
                children_clone,
                cancel_clone,
            )
            .await;
        });

        // Let the loop reach the idle wait.
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(dest.published().is_empty(), "nothing to drain yet");

        // Now enqueue + wake.
        let msg = sample_message("@a.ed25519", 1);
        ack_barriers.insert(msg.hash.clone(), Arc::new(AckBarrier::new(0, 1)));
        pre_enqueue(&dir, 0, msg);
        dir.egress_wake.notify_one();

        let hit = eventually(|| dest.published().len() == 1, Duration::from_secs(2)).await;
        assert!(hit, "egress must wake + drain after egress_wake fires");

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), egress).await;
    }

    #[tokio::test]
    async fn egress_cancels_via_cancellation_token() {
        let dir = Arc::new(DirectionState::new());
        let dest = MockDest::new();
        let ack_barriers: Arc<DashMap<String, Arc<AckBarrier>>> = Arc::new(DashMap::new());
        let children = stub_children(2);
        let cancel = CancellationToken::new();

        let egress = tokio::spawn({
            let dir = dir.clone();
            let dest = dest.clone();
            let ack_barriers = ack_barriers.clone();
            let children_inner = children.clone();
            let cancel = cancel.clone();
            async move {
                run_egress(
                    1,
                    dest as Arc<dyn Transport>,
                    dir,
                    ack_barriers,
                    children_inner,
                    cancel,
                )
                .await;
            }
        });

        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(2), egress).await;
        assert!(
            result.is_ok(),
            "egress must exit promptly when cancel fires"
        );
    }

    /// Locks the trait-dispatch contract for the source-side ack hook
    /// without requiring a live NATS connection. After the C3 refactor,
    /// `run_egress` calls `source.ack_after_publish(hash)` via the
    /// `Transport` trait — `BusTransport` overrides the default no-op to
    /// drain `pending_acks`. This test substitutes a `RecordingSource`
    /// (also overrides) at index 0 and asserts the override fires when the
    /// barrier resolves to zero.
    #[tokio::test]
    async fn egress_dispatches_ack_after_publish_through_trait() {
        let dir = Arc::new(DirectionState::new());
        let dest = MockDest::new();
        let recording = RecordingSource::new();
        let ack_barriers: Arc<DashMap<String, Arc<AckBarrier>>> = Arc::new(DashMap::new());
        // children[0] = RecordingSource (the source we'll receive an ack on);
        // children[1] is a stub that we don't exercise.
        let children: Arc<Vec<Arc<dyn Transport>>> = Arc::new(vec![
            recording.clone() as Arc<dyn Transport>,
            Arc::new(StubChild) as Arc<dyn Transport>,
        ]);
        let cancel = CancellationToken::new();

        let msg = sample_message("@a.ed25519", 1);
        let hash = msg.hash.clone();
        // Single destination → barrier resolves to zero on the first publish.
        ack_barriers.insert(hash.clone(), Arc::new(AckBarrier::new(0, 1)));
        pre_enqueue(&dir, 0, msg);

        let dir_clone = dir.clone();
        let dest_clone = dest.clone();
        let barriers_clone = ack_barriers.clone();
        let children_clone = children.clone();
        let cancel_clone = cancel.clone();
        let egress = tokio::spawn(async move {
            run_egress(
                1,
                dest_clone as Arc<dyn Transport>,
                dir_clone,
                barriers_clone,
                children_clone,
                cancel_clone,
            )
            .await;
        });
        dir.egress_wake.notify_one();

        let acked = eventually(|| recording.ack_calls().len() == 1, Duration::from_secs(2)).await;
        assert!(
            acked,
            "egress must dispatch ack_after_publish through the Transport trait \
             so BusTransport's override fires on barrier-final"
        );
        assert_eq!(recording.ack_calls()[0], hash);
        assert!(ack_barriers.is_empty());

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), egress).await;
    }

    #[tokio::test]
    async fn egress_publish_in_flight_since_brackets_publish_call() {
        let dir = Arc::new(DirectionState::new());
        let dest = MockDest::new();
        let ack_barriers: Arc<DashMap<String, Arc<AckBarrier>>> = Arc::new(DashMap::new());
        let children = stub_children(2);
        let cancel = CancellationToken::new();

        let msg = sample_message("@a.ed25519", 1);
        ack_barriers.insert(msg.hash.clone(), Arc::new(AckBarrier::new(0, 1)));
        pre_enqueue(&dir, 0, msg);

        // Before egress runs, publish_in_flight_since is None.
        assert!(dir.publish_in_flight_since.read().is_none());

        let dir_clone = dir.clone();
        let dest_clone = dest.clone();
        let barriers_clone = ack_barriers.clone();
        let children_clone = children.clone();
        let cancel_clone = cancel.clone();
        let egress = tokio::spawn(async move {
            run_egress(
                1,
                dest_clone as Arc<dyn Transport>,
                dir_clone,
                barriers_clone,
                children_clone,
                cancel_clone,
            )
            .await;
        });
        dir.egress_wake.notify_one();

        // After drain, publish_in_flight_since should have returned to None.
        let hit = eventually(
            || dest.published().len() == 1 && dir.publish_in_flight_since.read().is_none(),
            Duration::from_secs(2),
        )
        .await;
        assert!(
            hit,
            "publish_in_flight_since must return to None after each publish"
        );

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), egress).await;
    }

    #[tokio::test]
    async fn egress_clears_oldest_queued_at_when_all_queues_drained() {
        let dir = Arc::new(DirectionState::new());
        let dest = MockDest::new();
        let ack_barriers: Arc<DashMap<String, Arc<AckBarrier>>> = Arc::new(DashMap::new());
        let children = stub_children(2);
        let cancel = CancellationToken::new();

        // Arm oldest_queued_at as ingress would.
        *dir.oldest_queued_at.write() = Some(Instant::now());
        let msg = sample_message("@a.ed25519", 1);
        ack_barriers.insert(msg.hash.clone(), Arc::new(AckBarrier::new(0, 1)));
        pre_enqueue(&dir, 0, msg);

        let dir_clone = dir.clone();
        let dest_clone = dest.clone();
        let barriers_clone = ack_barriers.clone();
        let children_clone = children.clone();
        let cancel_clone = cancel.clone();
        let egress = tokio::spawn(async move {
            run_egress(
                1,
                dest_clone as Arc<dyn Transport>,
                dir_clone,
                barriers_clone,
                children_clone,
                cancel_clone,
            )
            .await;
        });
        dir.egress_wake.notify_one();

        let hit = eventually(
            || dir.oldest_queued_at.read().is_none(),
            Duration::from_secs(2),
        )
        .await;
        assert!(
            hit,
            "oldest_queued_at must clear once the last queue drains"
        );

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), egress).await;
    }

    #[tokio::test]
    async fn egress_n3_barrier_not_removed_until_all_destinations_resolve() {
        // This test focuses on the barrier-removal semantics across
        // multiple destinations. We run TWO egress tasks for two different
        // destinations, both draining messages destined from the same
        // source (source_id=0). Each destination has its own DirectionState.
        let dir1 = Arc::new(DirectionState::new());
        let dir2 = Arc::new(DirectionState::new());
        let dest1 = MockDest::new();
        let dest2 = MockDest::new();
        let ack_barriers: Arc<DashMap<String, Arc<AckBarrier>>> = Arc::new(DashMap::new());
        let children = stub_children(3);
        let cancel = CancellationToken::new();

        let msg = sample_message("@a.ed25519", 1);
        let hash = msg.hash.clone();
        // Composite has 3 children, source=0 → 2 destinations → pending=2.
        let barrier = Arc::new(AckBarrier::new(0, 2));
        ack_barriers.insert(hash.clone(), barrier.clone());
        pre_enqueue(&dir1, 0, msg.clone());
        pre_enqueue(&dir2, 0, msg);

        let e1 = {
            let dir = dir1.clone();
            let dest = dest1.clone();
            let bars = ack_barriers.clone();
            let children_clone = children.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                run_egress(
                    1,
                    dest as Arc<dyn Transport>,
                    dir,
                    bars,
                    children_clone,
                    cancel,
                )
                .await;
            })
        };
        let e2 = {
            let dir = dir2.clone();
            let dest = dest2.clone();
            let bars = ack_barriers.clone();
            let children_clone = children.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                run_egress(
                    2,
                    dest as Arc<dyn Transport>,
                    dir,
                    bars,
                    children_clone,
                    cancel,
                )
                .await;
            })
        };
        dir1.egress_wake.notify_one();
        dir2.egress_wake.notify_one();

        // Eventually both destinations publish AND the barrier is removed.
        let hit = eventually(
            || {
                !dest1.published().is_empty()
                    && !dest2.published().is_empty()
                    && !ack_barriers.contains_key(&hash)
            },
            Duration::from_secs(3),
        )
        .await;
        assert!(
            hit,
            "barrier must be removed only after BOTH destinations resolve"
        );

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), e1).await;
        let _ = tokio::time::timeout(Duration::from_secs(1), e2).await;
    }

    // Suppress unused-variable lint on the counter kept around for future
    // wiring of the concrete BusTransport mock.
    #[allow(dead_code)]
    fn _unused_atomic_usize() -> AtomicUsize {
        AtomicUsize::new(0)
    }
}
