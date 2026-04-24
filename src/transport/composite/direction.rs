//! `DirectionState` + `AuthorQueue` + `AckBarrier` — RFC 0002 §8.2 scaffolding.
//!
//! Phase 2 Wave 3 Step 14: data types for bounded per-(source, destination,
//! author) queues with watermark-driven backpressure and the per-message
//! multi-destination ack barrier (amendment §C.5).
//!
//! Behavior (ingress/egress tasks, `publish`, `start`, `shutdown`, `health`)
//! lands in Steps 15–20; this module only owns the shapes and their
//! invariants so later steps can be mechanical.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use parking_lot::{Mutex, RwLock};
use tokio::sync::Notify;

use crate::feed::models::Message;
use crate::identity::PublicId;

/// Maximum messages resident in a single per-author queue (RFC 0002 §8.2).
///
/// Not bytes, not credits — whole messages, because egregore's 64 KB per-message
/// cap already bounds memory per slot.
pub const BRIDGE_QUEUE_CAPACITY: usize = 256;

/// 75% of capacity: crossing this marks the (source, author) pair as
/// upstream-backpressured (RFC 0002 §8.2).
pub const BRIDGE_QUEUE_HIGH_WATERMARK: usize = 192;

/// 25% of capacity: dropping to or below this clears backpressure and wakes
/// blocked ingress tasks (RFC 0002 §8.2).
pub const BRIDGE_QUEUE_LOW_WATERMARK: usize = 64;

// Compile-time sanity so later tweaks don't accidentally invert the watermarks.
const _: () = {
    assert!(BRIDGE_QUEUE_HIGH_WATERMARK < BRIDGE_QUEUE_CAPACITY);
    assert!(BRIDGE_QUEUE_LOW_WATERMARK < BRIDGE_QUEUE_HIGH_WATERMARK);
};

/// Identifier for a child transport — `usize` index into
/// `CompositeTransport.children` (RFC 0002 §8.2).
pub type TransportId = usize;

/// Signals that a push was rejected because the destination queue is already
/// at `capacity`. The ingress task responds by awaiting the queue's
/// `space_available` notifier and retrying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueFull;

/// Outcome of a successful `AuthorQueue::push`.
///
/// Exposes whether the push crossed the high watermark so callers can
/// increment `backpressure_events` + insert into the `backpressured` set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PushOutcome {
    /// True iff the queue length transitioned from `< high_watermark` to
    /// `>= high_watermark` on this push. Crossings, not static state, are
    /// what the metric and the `backpressured` set key off — otherwise a
    /// steady-state-high queue would double-count every push.
    pub crossed_high_watermark: bool,
    /// True iff the queue was empty immediately before this push. Used by
    /// ingress to decide whether to arm the direction's `egress_wake` +
    /// stamp the direction's `oldest_queued_at` field.
    pub was_empty_before: bool,
}

/// Bounded FIFO holding `Arc<Message>` for a single (source, author) pair in
/// a single direction.
///
/// The queue itself is not thread-safe — callers protect it via
/// `DirectionState.queues`'s outer lock. `Arc<Message>` lets ingress share
/// one allocation across multiple destination queues (§8.2 ingress step 1).
#[derive(Debug)]
pub struct AuthorQueue {
    pub buf: VecDeque<Arc<Message>>,
    pub capacity: usize,
    pub high_watermark: usize,
    pub low_watermark: usize,
}

impl Default for AuthorQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl AuthorQueue {
    /// Construct with default watermarks per RFC 0002 §8.2 + the `BRIDGE_*`
    /// constants above.
    pub fn new() -> Self {
        Self {
            buf: VecDeque::with_capacity(BRIDGE_QUEUE_CAPACITY),
            capacity: BRIDGE_QUEUE_CAPACITY,
            high_watermark: BRIDGE_QUEUE_HIGH_WATERMARK,
            low_watermark: BRIDGE_QUEUE_LOW_WATERMARK,
        }
    }

    /// Push one message. Returns `Err(QueueFull)` when the queue is already
    /// at `capacity` so the caller can block on the per-queue `Notify` and
    /// retry after egress drains.
    pub fn push(&mut self, msg: Arc<Message>) -> Result<PushOutcome, QueueFull> {
        if self.buf.len() >= self.capacity {
            return Err(QueueFull);
        }
        let was_empty_before = self.buf.is_empty();
        let was_below_high = self.buf.len() < self.high_watermark;
        self.buf.push_back(msg);
        let crossed_high_watermark = was_below_high && self.buf.len() >= self.high_watermark;
        Ok(PushOutcome {
            crossed_high_watermark,
            was_empty_before,
        })
    }

    /// Pop the oldest message (FIFO). Returns `None` when empty.
    pub fn pop(&mut self) -> Option<Arc<Message>> {
        self.buf.pop_front()
    }

    /// Current queue length.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// True iff `len() == 0`.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// True iff the current length is at or above the high watermark.
    /// Used by ingress to detect a steady-state backpressured queue.
    pub fn crosses_high_watermark(&self) -> bool {
        self.buf.len() >= self.high_watermark
    }

    /// True iff the current length has fallen to or below the low watermark.
    /// Egress consults this after each pop to decide whether to fire the
    /// per-queue `space_available` notifier.
    pub fn at_or_below_low_watermark(&self) -> bool {
        self.buf.len() <= self.low_watermark
    }
}

/// A single (source, author) slot in a direction's queue table.
///
/// The `Notify` is per-queue (not per-direction) so that a wake fires the
/// ingress task that was blocked on *this* queue's space, not an unrelated
/// task that would wake, recheck its own still-full queue, and sleep again —
/// the classic `notify_one` lost-wakeup hazard RFC 0002 §8.2 calls out.
pub struct AuthorQueueEntry {
    pub queue: AuthorQueue,
    pub space_available: Arc<Notify>,
}

impl AuthorQueueEntry {
    pub fn new() -> Self {
        Self {
            queue: AuthorQueue::new(),
            space_available: Arc::new(Notify::new()),
        }
    }
}

impl Default for AuthorQueueEntry {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-destination state (RFC 0002 §8.2 + amendment §C.4/§C.5).
///
/// For a `CompositeTransport` with `n` children, the composite holds `n`
/// `DirectionState`s: `directions[j]` owns the queues holding messages
/// going TO child `j` (from every other child `i != j`).
///
/// `queues` and `backpressured` use `parking_lot::Mutex` rather than
/// `RwLock` because writers dominate (every push/pop mutates) and
/// `parking_lot::Mutex` avoids the poisoning footgun of the std primitive.
/// The `egress_wake` `Notify` is separate from the per-queue
/// `space_available` notifiers: the two signals have different recipients
/// (ingress vs egress) and mixing them on one `Notify` causes lost wakeups
/// in multi-ingress deployments.
pub struct DirectionState {
    /// `(source_transport_id, author) -> entry`. Populated lazily by ingress;
    /// entries are never removed at runtime (keeping them avoids re-allocating
    /// the `Notify` on every wave of messages from the same author).
    pub queues: Mutex<HashMap<(TransportId, PublicId), AuthorQueueEntry>>,

    /// The set of `(source, author)` pairs currently sitting at or above the
    /// high watermark. Ingress inserts on push-crosses-high; egress removes
    /// on pop-falls-to-low. Lives separately from `queues` so that health
    /// readers can snapshot `backpressured.len()` without holding the full
    /// queue table lock.
    pub backpressured: Mutex<HashSet<(TransportId, PublicId)>>,

    /// Awoken by ingress when ANY queue transitions empty → non-empty.
    /// Egress sleeps on this when its snapshot of `queues` has no non-empty
    /// entries.
    pub egress_wake: Arc<Notify>,

    /// Monotonic counter of upstream-ingress block events (queue crossed
    /// high watermark). Exposed via `BridgeQueuesHealth.backpressure_events_total`.
    pub backpressure_events: AtomicU64,

    /// Monotonic counter of destination publishes that returned `Err(_)`
    /// and were acked anyway per §8.2 ack-on-destination-error policy.
    pub ack_on_error_total: AtomicU64,

    /// Monotonic counter of NATS `ack_wait` expiries causing redelivery
    /// (bus-side destinations only; always zero for gossip destinations).
    /// Surfaced in `BridgeQueuesHealth.nats_redelivery_total`.
    pub nats_redelivery_total: AtomicU64,

    /// Most recent direction-specific error (amendment §C.4, auditor A2).
    /// MUST NOT contain Ed25519 pubkeys, hashes, or ciphertext — use short
    /// codes + source/destination identifiers only.
    pub last_error: RwLock<Option<String>>,

    /// Oldest queued timestamp across all queues in this direction. Set by
    /// ingress when a previously-empty queue receives a push; cleared by
    /// egress when the last non-empty queue drains. Powers the
    /// `oldest_queued_age_secs` health field — the signature of a hung
    /// destination with shallow queues (RFC 0002 §8.4 stuck-detection).
    pub oldest_queued_at: RwLock<Option<Instant>>,

    /// Instant when the currently-in-flight `publish` started (if any).
    /// Set immediately before each `dest.publish().await` and cleared
    /// immediately after. Powers `publish_in_flight_age_secs`.
    pub publish_in_flight_since: RwLock<Option<Instant>>,

    /// Amendment §C.4: monotonic counter of bus self-echoes dropped at
    /// ingest. Surfaced in `BridgeQueuesHealth.self_echo_total`. Distinct
    /// from `backpressure_events` because a self-echo is not a backpressure
    /// event (it never reaches a queue).
    pub self_echo_total: AtomicU64,
}

impl DirectionState {
    /// Fresh state for a destination that has never seen a message.
    pub fn new() -> Self {
        Self {
            queues: Mutex::new(HashMap::new()),
            backpressured: Mutex::new(HashSet::new()),
            egress_wake: Arc::new(Notify::new()),
            backpressure_events: AtomicU64::new(0),
            ack_on_error_total: AtomicU64::new(0),
            nats_redelivery_total: AtomicU64::new(0),
            last_error: RwLock::new(None),
            oldest_queued_at: RwLock::new(None),
            publish_in_flight_since: RwLock::new(None),
            self_echo_total: AtomicU64::new(0),
        }
    }
}

impl Default for DirectionState {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-message multi-destination ack barrier (amendment §C.5).
///
/// Created by ingress when a source message is fanned out to `n ≥ 1`
/// destinations; decremented by egress on each destination's publish
/// resolution; triggers source-side ack in the thread that observes the
/// counter transitioning to zero.
///
/// Ordering rationale (RFC 0002 §8.2 "Multi-destination ack barrier"):
/// `resolve_one` uses `AtomicUsize::fetch_sub(1, Ordering::AcqRel)` so the
/// thread that observes the transition to zero synchronizes with every
/// prior `resolve_one` call on this barrier — including those that set
/// `had_error` to `true`. A non-atomic RMW (load+cmp+store) would let two
/// concurrent last-resolvers both see "pending was 1, decrement to 0" and
/// double-fire the ack; `fetch_sub` serializes the RMW.
///
/// `had_error` is written with `Ordering::Release` (so a subsequent
/// `Acquire` load — which the barrier-completing thread performs
/// immediately after `fetch_sub` returns 1 — sees the latest `true`) and
/// is sticky: once set true it never returns to false.
pub struct AckBarrier {
    /// Remaining destinations that have yet to resolve. Starts at the
    /// fan-out count; decrements to zero; MAY NOT re-increment (the barrier
    /// is one-shot).
    pub pending_count: AtomicUsize,
    /// True iff any destination resolved with `Err`. Sticky: never goes
    /// back to false.
    pub had_error: AtomicBool,
    /// The source transport id — the thread that zeroes the counter calls
    /// `source.ack_after_publish` (bus) or a no-op (gossip). Stored here so
    /// the egress side does not have to thread the id through the call
    /// stack.
    pub source_id: TransportId,
}

impl AckBarrier {
    /// Construct a barrier for a source message fanned out to
    /// `destination_count` destinations.
    ///
    /// For N=1 the barrier fires on the single resolve_one call (degenerate
    /// but legitimate — a two-transport bridge has exactly one destination
    /// per source). For N=0 the barrier can never fire; callers MUST NOT
    /// create a barrier with `destination_count == 0` (it would leak
    /// forever). Debug builds panic on that case.
    pub fn new(source_id: TransportId, destination_count: usize) -> Self {
        debug_assert!(
            destination_count > 0,
            "AckBarrier requires at least one destination; N=0 cannot resolve"
        );
        Self {
            pending_count: AtomicUsize::new(destination_count),
            had_error: AtomicBool::new(false),
            source_id,
        }
    }

    /// Record one destination's resolution.
    ///
    /// Returns `Some((had_error, source_id))` iff this call was the one that
    /// transitioned the counter to zero — the caller MUST ack the source
    /// (for bus) or record metrics (for gossip). Returns `None` otherwise.
    ///
    /// `errored = true` sets `had_error` before the decrement, ensuring the
    /// thread that observes the transition-to-zero sees the final value.
    /// Uses `Ordering::AcqRel` per RFC 0002 §8.2 "Multi-destination ack
    /// barrier" for the decrement; `had_error` uses `Release` write +
    /// `Acquire` read so the final observer sees the sticky-true state.
    pub fn resolve_one(&self, errored: bool) -> Option<(bool, TransportId)> {
        if errored {
            // Release write so the thread that observes the final
            // fetch_sub==1 (Acquire half of AcqRel) will synchronize with
            // this store and see had_error=true.
            self.had_error.store(true, Ordering::Release);
        }
        // AcqRel so concurrent resolve_one calls serialize; the thread
        // whose previous value was 1 is uniquely the one that fires ack.
        let previous = self.pending_count.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(
            previous > 0,
            "AckBarrier::resolve_one called more times than destinations"
        );
        if previous == 1 {
            // Acquire load pairs with every prior Release store of had_error.
            let had_error = self.had_error.load(Ordering::Acquire);
            Some((had_error, self.source_id))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::models::Message;
    use crate::identity::PublicId;
    use chrono::Utc;

    fn sample_message(seq: u64) -> Arc<Message> {
        Arc::new(Message {
            author: PublicId("@author.ed25519".to_string()),
            sequence: seq,
            previous: None,
            timestamp: Utc::now(),
            content: serde_json::json!({"type": "note", "text": "x"}),
            schema_id: None,
            relates: None,
            tags: vec![],
            trace_id: None,
            span_id: None,
            expires_at: None,
            hash: format!("hash-{seq}"),
            signature: "sig".to_string(),
        })
    }

    #[test]
    fn author_queue_push_and_pop_are_fifo() {
        let mut q = AuthorQueue::new();
        for seq in 1..=5 {
            let outcome = q.push(sample_message(seq)).expect("not full");
            if seq == 1 {
                assert!(
                    outcome.was_empty_before,
                    "first push must report was_empty_before"
                );
            } else {
                assert!(!outcome.was_empty_before);
            }
            assert!(
                !outcome.crossed_high_watermark,
                "small push count never crosses 192 high watermark"
            );
        }
        assert_eq!(q.len(), 5);
        for seq in 1..=5 {
            let msg = q.pop().expect("not empty");
            assert_eq!(msg.sequence, seq, "FIFO order: got seq {}", msg.sequence);
        }
        assert!(q.pop().is_none(), "empty queue returns None");
    }

    #[test]
    fn author_queue_push_returns_queue_full_at_capacity() {
        let mut q = AuthorQueue::new();
        for seq in 1..=(BRIDGE_QUEUE_CAPACITY as u64) {
            q.push(sample_message(seq)).expect("under capacity");
        }
        assert_eq!(q.len(), BRIDGE_QUEUE_CAPACITY);
        let result = q.push(sample_message(9999));
        assert_eq!(
            result,
            Err(QueueFull),
            "push at capacity must report QueueFull"
        );
        assert_eq!(
            q.len(),
            BRIDGE_QUEUE_CAPACITY,
            "rejected push must not mutate queue length"
        );
    }

    #[test]
    fn author_queue_reports_high_watermark_crossing_exactly_once() {
        let mut q = AuthorQueue::new();
        let mut crossings = 0;
        for seq in 1..=(BRIDGE_QUEUE_HIGH_WATERMARK as u64 + 5) {
            let outcome = q.push(sample_message(seq)).unwrap();
            if outcome.crossed_high_watermark {
                crossings += 1;
                assert_eq!(
                    q.len(),
                    BRIDGE_QUEUE_HIGH_WATERMARK,
                    "crossing reported exactly when len first hits high_watermark (192)"
                );
            }
        }
        assert_eq!(
            crossings, 1,
            "crossing fires exactly once for a monotonic fill; got {crossings}"
        );
    }

    #[test]
    fn author_queue_crosses_high_and_at_low_predicates() {
        let mut q = AuthorQueue::new();
        assert!(!q.crosses_high_watermark());
        assert!(q.at_or_below_low_watermark());

        // Fill to high watermark.
        for seq in 1..=(BRIDGE_QUEUE_HIGH_WATERMARK as u64) {
            q.push(sample_message(seq)).unwrap();
        }
        assert!(q.crosses_high_watermark());
        assert!(!q.at_or_below_low_watermark());

        // Drain to just below low watermark.
        let to_drain = BRIDGE_QUEUE_HIGH_WATERMARK - BRIDGE_QUEUE_LOW_WATERMARK;
        for _ in 0..to_drain {
            q.pop();
        }
        assert_eq!(q.len(), BRIDGE_QUEUE_LOW_WATERMARK);
        assert!(q.at_or_below_low_watermark());
        assert!(!q.crosses_high_watermark());
    }

    #[test]
    fn author_queue_is_empty_and_len_stay_consistent_across_mixed_ops() {
        let mut q = AuthorQueue::new();
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);
        q.push(sample_message(1)).unwrap();
        assert!(!q.is_empty());
        assert_eq!(q.len(), 1);
        q.push(sample_message(2)).unwrap();
        assert_eq!(q.len(), 2);
        q.pop();
        assert_eq!(q.len(), 1);
        q.pop();
        assert!(q.is_empty());
    }

    #[test]
    fn ack_barrier_n1_fires_on_single_resolve() {
        let barrier = AckBarrier::new(0, 1);
        let result = barrier.resolve_one(false);
        assert_eq!(
            result,
            Some((false, 0)),
            "N=1 barrier fires on single resolve with had_error=false"
        );
    }

    #[test]
    fn ack_barrier_n3_fires_only_on_third_resolve() {
        let barrier = AckBarrier::new(7, 3);
        assert_eq!(barrier.resolve_one(false), None, "1st of 3 must not fire");
        assert_eq!(barrier.resolve_one(false), None, "2nd of 3 must not fire");
        let last = barrier.resolve_one(false);
        assert_eq!(
            last,
            Some((false, 7)),
            "3rd of 3 fires with source_id=7 and had_error=false"
        );
    }

    #[test]
    fn ack_barrier_had_error_is_sticky_across_resolves() {
        let barrier = AckBarrier::new(9, 3);
        // First resolution errors; should set had_error=true.
        assert_eq!(barrier.resolve_one(true), None);
        // Second resolution is Ok; must NOT reset had_error.
        assert_eq!(barrier.resolve_one(false), None);
        // Third resolution is Ok; had_error should still surface as true.
        let last = barrier.resolve_one(false);
        assert_eq!(
            last,
            Some((true, 9)),
            "had_error sticky: any prior Err observer forces final to true"
        );
    }

    #[test]
    fn ack_barrier_had_error_unset_when_no_errored_resolve() {
        let barrier = AckBarrier::new(3, 2);
        assert_eq!(barrier.resolve_one(false), None);
        let last = barrier.resolve_one(false);
        assert_eq!(last, Some((false, 3)));
    }

    #[test]
    fn ack_barrier_errored_false_preserves_already_true_had_error() {
        let barrier = AckBarrier::new(5, 3);
        // Errored=true observed first.
        barrier.resolve_one(true);
        // Subsequent errored=false MUST NOT overwrite had_error.
        barrier.resolve_one(false);
        let last = barrier.resolve_one(false);
        assert_eq!(
            last,
            Some((true, 5)),
            "errored=false does not reset a prior errored=true"
        );
    }

    #[test]
    fn ack_barrier_resolve_one_is_atomic_under_concurrent_access() {
        // This test documents the AcqRel contract from RFC 0002 §8.2 by
        // exercising many threads racing on a single barrier and checking
        // that exactly ONE observer sees the transition-to-zero. A
        // non-atomic RMW would double-fire or lose the transition;
        // `fetch_sub(1, AcqRel)` serializes.
        use std::sync::atomic::AtomicUsize;
        const DESTS: usize = 64;
        let barrier = Arc::new(AckBarrier::new(42, DESTS));
        let fires = Arc::new(AtomicUsize::new(0));
        let mut handles = vec![];
        for i in 0..DESTS {
            let b = barrier.clone();
            let f = fires.clone();
            handles.push(std::thread::spawn(move || {
                // Alternate errored to exercise the sticky-had_error path.
                let errored = i % 3 == 0;
                if b.resolve_one(errored).is_some() {
                    f.fetch_add(1, Ordering::AcqRel);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            fires.load(Ordering::Acquire),
            1,
            "exactly one observer must see the transition-to-zero; got {}",
            fires.load(Ordering::Acquire)
        );
    }

    #[test]
    fn author_queue_entry_exposes_shared_notify_across_clones() {
        // Sanity: the Arc<Notify> is the per-queue wake. Clones must alias
        // so ingress waiters and egress notifiers talk to the same slot.
        let entry = AuthorQueueEntry::new();
        let a = entry.space_available.clone();
        let b = entry.space_available.clone();
        assert!(
            Arc::ptr_eq(&a, &b),
            "space_available clones must alias (same Arc target)"
        );
    }

    #[test]
    fn direction_state_initializes_all_atomics_and_options_to_zero_none() {
        let dir = DirectionState::new();
        assert_eq!(dir.backpressure_events.load(Ordering::Relaxed), 0);
        assert_eq!(dir.ack_on_error_total.load(Ordering::Relaxed), 0);
        assert_eq!(dir.nats_redelivery_total.load(Ordering::Relaxed), 0);
        assert_eq!(dir.self_echo_total.load(Ordering::Relaxed), 0);
        assert!(dir.last_error.read().is_none());
        assert!(dir.oldest_queued_at.read().is_none());
        assert!(dir.publish_in_flight_since.read().is_none());
        assert!(dir.queues.lock().is_empty());
        assert!(dir.backpressured.lock().is_empty());
    }
}
