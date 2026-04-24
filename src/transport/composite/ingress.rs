//! Per-source ingress task — RFC 0002 §8.2 + amendment §C.5.
//!
//! One task per child transport. Subscribes to the child via
//! `Transport::subscribe(TopicFilter::default())` and, for each received
//! message, fans an `Arc<Message>` into the `DirectionState` queue for
//! every OTHER child (source → every destination except self). The ack
//! barrier (amendment §C.5) is created BEFORE any queue push so that a
//! fast-completing first destination cannot drive the counter to zero
//! before the later destinations have even been enqueued.
//!
//! Backpressure: when a destination queue is at `capacity`, the ingress
//! task awaits the per-queue `space_available` notifier and retries.
//! Blocking one destination does not block the others: we push to
//! destinations sequentially, so a full queue for destination `A` stalls
//! this ingress task (which is exactly the upstream-backpressure signal
//! §8.2 specifies) while sibling ingress tasks from OTHER sources
//! continue to push into destination `B`'s un-full queues.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::feed::models::Message;
use crate::transport::filter::TopicFilter;
use crate::transport::trait_def::Transport;

use super::direction::{AckBarrier, DirectionState, TransportId};

/// Run the ingress loop for source transport `source_id`.
///
/// Exits cleanly when the source subscription stream ends (Invariant 5
/// stream-end) or when `cancel` is fired. Holds no locks across `.await`
/// beyond the tiny critical sections inside `push_to_destination`.
pub(crate) async fn run_ingress(
    source_id: TransportId,
    source_transport: Arc<dyn Transport>,
    directions: Arc<Vec<Arc<DirectionState>>>,
    ack_barriers: Arc<DashMap<String, Arc<AckBarrier>>>,
    num_children: usize,
    cancel: CancellationToken,
) {
    // Open the source subscription with default filter — per RFC 0002 §8.2
    // the bridge forwards all signed messages (RL10 revised); per-tag or
    // per-author filtering is a client-layer concern. `.await` here blocks
    // until the child signals readiness.
    let (_sub_handle, mut stream) =
        match source_transport.subscribe(TopicFilter::default()).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(
                    source_id,
                    error = %e,
                    "composite ingress: subscribe on source transport failed"
                );
                return;
            }
        };

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                tracing::debug!(source_id, "composite ingress: cancel signal, exiting");
                break;
            }
            next = stream.next() => match next {
                None => {
                    tracing::debug!(
                        source_id,
                        "composite ingress: source subscription ended"
                    );
                    break;
                }
                Some(msg) => {
                    forward_one(
                        source_id,
                        msg,
                        &directions,
                        &ack_barriers,
                        num_children,
                        &cancel,
                    )
                    .await;
                }
            }
        }
    }
}

/// Fan one source message out to every destination queue.
///
/// Splits out so the cancel-aware `select!` body stays tight.
async fn forward_one(
    source_id: TransportId,
    msg: Message,
    directions: &Arc<Vec<Arc<DirectionState>>>,
    ack_barriers: &Arc<DashMap<String, Arc<AckBarrier>>>,
    num_children: usize,
    cancel: &CancellationToken,
) {
    // Destination count = every child except the source. For a 2-child
    // bridge this is 1; the AckBarrier::new debug_assert catches N=0.
    let destination_count = num_children.saturating_sub(1);
    if destination_count == 0 {
        // Degenerate (single-child) — CompositeTransport::new rejects this
        // at construction, so reaching this branch is a bug. Don't panic
        // in release; just drop the message to avoid the N=0 barrier.
        debug_assert!(false, "ingress: single-child composite is unconfigured");
        return;
    }

    let arc = Arc::new(msg);

    // §C.5 invariant: insert the barrier BEFORE pushing to ANY destination
    // queue. Without this ordering, a fast-completing first destination's
    // egress could call resolve_one on a barrier that never saw its
    // pending_count initialized against all (N-1) destinations — or worse,
    // see no barrier at all and skip the ack path entirely.
    let barrier = Arc::new(AckBarrier::new(source_id, destination_count));
    ack_barriers.insert(arc.hash.clone(), barrier);

    // Push to every destination j != source_id. If we're cancelled mid-fan
    // (a destination queue is full and we're awaiting space_available),
    // bail out of the fan loop; the barrier will be garbage-collected by
    // the shutdown drain path (Step 19). This is the only `.await` inside
    // forward_one.
    for (dest_id, dir) in directions.iter().enumerate() {
        if dest_id == source_id {
            continue;
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                tracing::debug!(
                    source_id,
                    dest_id,
                    "composite ingress: cancel during fan-out, dropping barrier"
                );
                ack_barriers.remove(&arc.hash);
                return;
            }
            () = push_to_destination(dir.clone(), source_id, arc.clone()) => {}
        }
    }
}

/// Push one `Arc<Message>` into `dir.queues[(source_id, author)]`,
/// blocking on `space_available` if the queue is at capacity.
///
/// Critical-section discipline: the `parking_lot::Mutex` guards are
/// tightly scoped — we never hold any guard across an `.await`. When a
/// queue is full we clone the `Arc<Notify>` out, drop the guard, then
/// `.notified().await`. If another thread happens to push between the
/// wake and the retry, our retry will simply observe that the queue is
/// again/still full and re-await.
async fn push_to_destination(
    dir: Arc<DirectionState>,
    source_id: TransportId,
    msg: Arc<Message>,
) {
    let key = (source_id, msg.author.clone());

    loop {
        // Scope: lock the queues map, try to push, extract what we need to
        // signal outside the critical section.
        let space_available = {
            let mut queues = dir.queues.lock();
            let entry = queues.entry(key.clone()).or_default();

            match entry.queue.push(msg.clone()) {
                Ok(outcome) => {
                    let space_notify_handle = entry.space_available.clone();
                    // Hold the lock through the outcome read so the next
                    // push into this same entry observes a consistent
                    // queue state.
                    drop(queues);

                    // Update direction-level state outside the queue lock.
                    if outcome.crossed_high_watermark {
                        let inserted = dir.backpressured.lock().insert(key.clone());
                        if inserted {
                            dir.backpressure_events.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    if outcome.was_empty_before {
                        // This push turned a previously-empty queue non-empty.
                        // Arm oldest_queued_at only if it wasn't already set
                        // (another queue in this direction may already hold
                        // older data).
                        let mut oldest = dir.oldest_queued_at.write();
                        if oldest.is_none() {
                            *oldest = Some(Instant::now());
                        }
                        drop(oldest);
                        // Wake the egress task — some queue just went non-empty.
                        dir.egress_wake.notify_one();
                    }

                    // Keep a handle alive so the compiler warns us if we
                    // drop it unused (lint guard against silent removal).
                    let _ = space_notify_handle;
                    return;
                }
                Err(_queue_full) => {
                    // Capture the per-queue wake handle + drop the guard
                    // before we await. The wake fires when egress pops
                    // this queue below low_watermark.
                    entry.space_available.clone()
                }
            }
        };
        // Full → block on this queue's Notify and retry. `notified()`
        // is an awaitable; awaiting yields until egress calls
        // `notify_waiters()` on the same handle.
        space_available.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Result;
    use crate::identity::PublicId;
    use crate::transport::composite::direction::{
        BRIDGE_QUEUE_CAPACITY, BRIDGE_QUEUE_HIGH_WATERMARK,
    };
    use crate::transport::subscription::SubscriptionHandle;
    use crate::transport::{TopicFilter, Transport, TransportHealth};
    use async_trait::async_trait;
    use chrono::Utc;
    use futures::stream::BoxStream;
    use parking_lot::Mutex as PlMutex;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;
    use tokio::sync::mpsc as tmpsc;
    use tokio::sync::oneshot;

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

    /// A minimal source-transport mock that exposes a scripted subscription
    /// stream via an mpsc channel. Tests push messages into `tx` and the
    /// ingress loop pulls from the paired stream on `subscribe`.
    struct ScriptedSource {
        tx: Arc<PlMutex<Option<tmpsc::Sender<Message>>>>,
        rx: Arc<PlMutex<Option<tmpsc::Receiver<Message>>>>,
        subscribed: Arc<AtomicBool>,
    }

    impl ScriptedSource {
        fn new(buffer: usize) -> (Arc<Self>, tmpsc::Sender<Message>) {
            let (tx, rx) = tmpsc::channel::<Message>(buffer);
            let src = Arc::new(Self {
                tx: Arc::new(PlMutex::new(Some(tx.clone()))),
                rx: Arc::new(PlMutex::new(Some(rx))),
                subscribed: Arc::new(AtomicBool::new(false)),
            });
            (src, tx)
        }
    }

    #[async_trait]
    impl Transport for ScriptedSource {
        async fn publish(&self, _msg: &Message) -> Result<()> {
            unreachable!("ScriptedSource is a source-only mock")
        }
        async fn subscribe(
            &self,
            _filter: TopicFilter,
        ) -> Result<(SubscriptionHandle, BoxStream<'static, Message>)> {
            self.subscribed.store(true, Ordering::Release);
            let (cancel_tx, _cancel_rx) = oneshot::channel::<()>();
            let handle = SubscriptionHandle::from_cancel(cancel_tx);
            let mut rx = self
                .rx
                .lock()
                .take()
                .expect("subscribe called twice on ScriptedSource");
            let stream = async_stream::stream! {
                while let Some(m) = rx.recv().await { yield m; }
            };
            // Drop the held tx clone so the stream ends when the test's
            // tx is dropped.
            drop(self.tx.lock().take());
            Ok((handle, Box::pin(stream)))
        }
        async fn request_from(
            &self,
            _a: PublicId,
            _s: u64,
        ) -> Result<BoxStream<'static, Message>> {
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
            }
        }
    }

    /// Spin briefly on a predicate. Used where we need to wait for an
    /// ingress task to drain a message into a queue without hard-coding
    /// a sleep. Returns true if predicate held within `timeout`.
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

    #[tokio::test]
    async fn push_to_destination_happy_path_sets_oldest_and_wakes_egress() {
        let dir = Arc::new(DirectionState::new());
        let msg = Arc::new(sample_message("@a.ed25519", 1));

        // Set up: spawn an egress-wake observer so we can detect the wake.
        let wake_notify = dir.egress_wake.clone();
        let wake_flag = Arc::new(AtomicBool::new(false));
        let wake_flag_clone = wake_flag.clone();
        let observer = tokio::spawn(async move {
            wake_notify.notified().await;
            wake_flag_clone.store(true, Ordering::Release);
        });

        push_to_destination(dir.clone(), 0, msg.clone()).await;

        // Queue got the message. Scope the guard so it drops before the
        // subsequent await (clippy: await_holding_lock).
        {
            let queues = dir.queues.lock();
            let entry = queues.get(&(0, msg.author.clone())).unwrap();
            assert_eq!(entry.queue.len(), 1);
        }

        // oldest_queued_at was armed.
        assert!(dir.oldest_queued_at.read().is_some());

        // Egress was woken.
        let _ = tokio::time::timeout(Duration::from_millis(200), observer).await;
        assert!(
            wake_flag.load(Ordering::Acquire),
            "egress_wake must fire on empty→non-empty push"
        );
    }

    #[tokio::test]
    async fn push_to_destination_crosses_high_watermark_increments_counter_once() {
        let dir = Arc::new(DirectionState::new());
        // Fill to just below high watermark.
        for seq in 1..BRIDGE_QUEUE_HIGH_WATERMARK as u64 {
            let msg = Arc::new(sample_message("@a.ed25519", seq));
            push_to_destination(dir.clone(), 0, msg).await;
        }
        assert_eq!(
            dir.backpressure_events.load(Ordering::Relaxed),
            0,
            "below high watermark: no backpressure event yet"
        );

        // The next push crosses the watermark.
        let msg = Arc::new(sample_message("@a.ed25519", BRIDGE_QUEUE_HIGH_WATERMARK as u64));
        push_to_destination(dir.clone(), 0, msg).await;
        assert_eq!(
            dir.backpressure_events.load(Ordering::Relaxed),
            1,
            "first high-watermark cross fires exactly one backpressure event"
        );
        assert!(dir
            .backpressured
            .lock()
            .contains(&(0, PublicId("@a.ed25519".into()))));

        // One more push at steady-state high does NOT double-count.
        let msg2 = Arc::new(sample_message(
            "@a.ed25519",
            BRIDGE_QUEUE_HIGH_WATERMARK as u64 + 1,
        ));
        push_to_destination(dir.clone(), 0, msg2).await;
        assert_eq!(
            dir.backpressure_events.load(Ordering::Relaxed),
            1,
            "steady-state high must not double-count; crossings are one-shot"
        );
    }

    #[tokio::test]
    async fn push_to_destination_blocks_on_full_queue_and_wakes_on_space() {
        let dir = Arc::new(DirectionState::new());
        // Pre-fill to capacity.
        {
            let mut queues = dir.queues.lock();
            let entry = queues.entry((0, PublicId("@a.ed25519".into()))).or_default();
            for seq in 1..=(BRIDGE_QUEUE_CAPACITY as u64) {
                entry.queue.push(Arc::new(sample_message("@a.ed25519", seq))).unwrap();
            }
            assert_eq!(entry.queue.len(), BRIDGE_QUEUE_CAPACITY);
        }

        // Spawn the push — it must block.
        let dir_clone = dir.clone();
        let push_task = tokio::spawn(async move {
            let msg = Arc::new(sample_message("@a.ed25519", 9_999));
            push_to_destination(dir_clone, 0, msg).await;
        });

        // Give the task a moment to hit the await.
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            !push_task.is_finished(),
            "push must block while queue is at capacity"
        );

        // Pop one, fire `notify_waiters`, push should complete.
        {
            let mut queues = dir.queues.lock();
            let entry = queues.get_mut(&(0, PublicId("@a.ed25519".into()))).unwrap();
            entry.queue.pop().unwrap();
            // Real egress calls notify_waiters only after low-watermark; for
            // the unit test we simulate the wake directly.
            entry.space_available.notify_waiters();
        }

        let completed = tokio::time::timeout(Duration::from_secs(2), push_task).await;
        assert!(
            completed.is_ok(),
            "push must wake and complete after space_available fires"
        );
    }

    #[tokio::test]
    async fn run_ingress_inserts_ack_barrier_before_queue_push() {
        // Three-child composite: source=0, destinations={1, 2}. Barrier
        // count should be (N-1) = 2. Verify the barrier exists as soon as
        // (or before) the queues see the message.
        let (src, tx) = ScriptedSource::new(16);
        let directions = Arc::new(vec![
            Arc::new(DirectionState::new()),
            Arc::new(DirectionState::new()),
            Arc::new(DirectionState::new()),
        ]);
        let ack_barriers = Arc::new(DashMap::new());
        let cancel = CancellationToken::new();

        let dirs_clone = directions.clone();
        let barriers_clone = ack_barriers.clone();
        let cancel_clone = cancel.clone();
        let ingress = tokio::spawn(async move {
            run_ingress(
                0,
                src as Arc<dyn Transport>,
                dirs_clone,
                barriers_clone,
                3,
                cancel_clone,
            )
            .await;
        });

        // Push one message.
        let msg = sample_message("@a.ed25519", 1);
        let expected_hash = msg.hash.clone();
        tx.send(msg).await.unwrap();

        // Wait for the ingress task to have fanned out.
        let hit = eventually(
            || {
                let dir1_has = !directions[1].queues.lock().is_empty();
                let dir2_has = !directions[2].queues.lock().is_empty();
                let barrier_has = ack_barriers.contains_key(&expected_hash);
                dir1_has && dir2_has && barrier_has
            },
            Duration::from_secs(2),
        )
        .await;
        assert!(hit, "ingress must fan out to both destinations and insert barrier");

        let barrier = ack_barriers.get(&expected_hash).unwrap().clone();
        assert_eq!(
            barrier.pending_count.load(Ordering::Acquire),
            2,
            "N=3 → 2 pending destinations"
        );

        // Cleanup: cancel ingress.
        drop(tx);
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), ingress).await;
    }

    #[tokio::test]
    async fn run_ingress_skips_self_destination() {
        let (src, tx) = ScriptedSource::new(16);
        let directions = Arc::new(vec![
            Arc::new(DirectionState::new()),
            Arc::new(DirectionState::new()),
        ]);
        let ack_barriers = Arc::new(DashMap::new());
        let cancel = CancellationToken::new();

        // source_id = 0 → messages must ONLY land in directions[1].
        let dirs_clone = directions.clone();
        let barriers_clone = ack_barriers.clone();
        let cancel_clone = cancel.clone();
        let ingress = tokio::spawn(async move {
            run_ingress(
                0,
                src as Arc<dyn Transport>,
                dirs_clone,
                barriers_clone,
                2,
                cancel_clone,
            )
            .await;
        });

        tx.send(sample_message("@a.ed25519", 1)).await.unwrap();

        let hit = eventually(
            || !directions[1].queues.lock().is_empty(),
            Duration::from_secs(2),
        )
        .await;
        assert!(hit);
        assert!(
            directions[0].queues.lock().is_empty(),
            "source_id's own direction must stay empty (self-destination skipped)"
        );

        drop(tx);
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), ingress).await;
    }

    #[tokio::test]
    async fn run_ingress_cancel_exits_loop() {
        let (src, _tx) = ScriptedSource::new(16);
        let directions = Arc::new(vec![
            Arc::new(DirectionState::new()),
            Arc::new(DirectionState::new()),
        ]);
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let ack_barriers = Arc::new(DashMap::new());

        let ingress = tokio::spawn(async move {
            run_ingress(
                0,
                src as Arc<dyn Transport>,
                directions,
                ack_barriers,
                2,
                cancel_clone,
            )
            .await;
        });

        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(2), ingress).await;
        assert!(
            result.is_ok(),
            "ingress must exit within the deadline when cancel fires"
        );
    }

    #[tokio::test]
    async fn run_ingress_exits_when_source_stream_ends() {
        let (src, tx) = ScriptedSource::new(16);
        let directions = Arc::new(vec![
            Arc::new(DirectionState::new()),
            Arc::new(DirectionState::new()),
        ]);
        let cancel = CancellationToken::new();
        let ack_barriers = Arc::new(DashMap::new());

        let ingress = tokio::spawn(async move {
            run_ingress(
                0,
                src as Arc<dyn Transport>,
                directions,
                ack_barriers,
                2,
                cancel,
            )
            .await;
        });

        // Close the source stream without cancelling.
        drop(tx);

        let result = tokio::time::timeout(Duration::from_secs(2), ingress).await;
        assert!(
            result.is_ok(),
            "ingress must exit when the source subscription stream ends"
        );
    }

    #[tokio::test]
    async fn concurrent_ingress_tasks_pushing_same_destination_do_not_race() {
        // Two source mocks, both pushing to directions[2]. The ingress
        // tasks run concurrently and should interleave pushes without
        // data corruption or lost messages.
        let (src_a, tx_a) = ScriptedSource::new(64);
        let (src_b, tx_b) = ScriptedSource::new(64);
        let directions = Arc::new(vec![
            Arc::new(DirectionState::new()),
            Arc::new(DirectionState::new()),
            Arc::new(DirectionState::new()),
        ]);
        let ack_barriers = Arc::new(DashMap::new());
        let cancel = CancellationToken::new();

        let ing_a = {
            let dirs = directions.clone();
            let bars = ack_barriers.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                run_ingress(0, src_a as Arc<dyn Transport>, dirs, bars, 3, cancel).await;
            })
        };
        let ing_b = {
            let dirs = directions.clone();
            let bars = ack_barriers.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                run_ingress(1, src_b as Arc<dyn Transport>, dirs, bars, 3, cancel).await;
            })
        };

        // Push 10 messages from each source, different authors so the
        // queues are separate (source_id, author) pairs → no contention
        // on a single queue.
        for seq in 1..=10 {
            tx_a.send(sample_message("@alpha.ed25519", seq)).await.unwrap();
            tx_b.send(sample_message("@beta.ed25519", seq)).await.unwrap();
        }
        drop(tx_a);
        drop(tx_b);

        // Every message lands in directions[2] (dest_id=2 != source_id for both).
        let hit = eventually(
            || {
                let q = directions[2].queues.lock();
                let from_a = q
                    .get(&(0, PublicId("@alpha.ed25519".into())))
                    .map(|e| e.queue.len())
                    .unwrap_or(0);
                let from_b = q
                    .get(&(1, PublicId("@beta.ed25519".into())))
                    .map(|e| e.queue.len())
                    .unwrap_or(0);
                from_a == 10 && from_b == 10
            },
            Duration::from_secs(5),
        )
        .await;
        assert!(
            hit,
            "both ingress tasks must push all 10 of their messages into directions[2]"
        );

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), ing_a).await;
        let _ = tokio::time::timeout(Duration::from_secs(1), ing_b).await;
    }

    #[tokio::test]
    async fn ack_barrier_has_exactly_n_minus_1_pending_on_fan_out() {
        // Explicit check of the §C.5 invariant for N=3: barrier starts
        // with pending_count = 2, not 3.
        let (src, tx) = ScriptedSource::new(4);
        let directions = Arc::new(vec![
            Arc::new(DirectionState::new()),
            Arc::new(DirectionState::new()),
            Arc::new(DirectionState::new()),
        ]);
        let ack_barriers = Arc::new(DashMap::new());
        let cancel = CancellationToken::new();

        let dirs = directions.clone();
        let bars = ack_barriers.clone();
        let c = cancel.clone();
        let ingress = tokio::spawn(async move {
            run_ingress(0, src as Arc<dyn Transport>, dirs, bars, 3, c).await;
        });

        let msg = sample_message("@a.ed25519", 1);
        let hash = msg.hash.clone();
        tx.send(msg).await.unwrap();

        let hit = eventually(
            || ack_barriers.contains_key(&hash),
            Duration::from_secs(2),
        )
        .await;
        assert!(hit);
        let barrier = ack_barriers.get(&hash).unwrap().clone();
        assert_eq!(barrier.pending_count.load(Ordering::Acquire), 2);
        assert_eq!(barrier.source_id, 0);

        drop(tx);
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), ingress).await;
    }

    #[tokio::test]
    async fn push_to_destination_noop_oldest_when_already_armed() {
        let dir = Arc::new(DirectionState::new());
        let earlier = Instant::now() - Duration::from_secs(1);
        *dir.oldest_queued_at.write() = Some(earlier);

        // Push into a previously-empty queue slot; oldest must NOT be
        // overwritten to "now" — we keep the older timestamp.
        let msg = Arc::new(sample_message("@a.ed25519", 1));
        push_to_destination(dir.clone(), 0, msg).await;

        let stored = dir.oldest_queued_at.read().unwrap();
        assert_eq!(
            stored, earlier,
            "oldest_queued_at must not regress to now when already armed"
        );
    }
}
