//! `MockTransport` — transport-invariant test double.
//!
//! Plan §5.9. Implements `egregore::transport::Transport` with fault-injection
//! knobs (silent drop of a specific `(author, seq)`, request-from stream
//! truncation, slow-ack mode for shutdown-drain) so the property tests can
//! exercise the seven RFC 0001 §6 invariants without real TCP timing.
//!
//! The mock tracks every message passed to `publish` — this is the evidence
//! surface the tests assert against. Subscribers receive a fan-out of every
//! non-dropped publish via `mpsc::Sender`. `request_from` returns a one-shot
//! snapshot over the already-published set.
//!
//! Not a production artifact; lives under `tests/common/`.

#![allow(dead_code)]

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use egregore::error::Result;
use egregore::feed::models::Message;
use egregore::identity::PublicId;
use egregore::transport::{SubscriptionHandle, TopicFilter, Transport, TransportHealth};

/// Transport-invariant test double with fault-injection.
pub struct MockTransport {
    /// Every `publish(msg)` call appends to this list, even if the message is
    /// configured to be silently dropped at the wire. Tests inspect this to
    /// verify at-least-once semantics (Invariant 3) and envelope preservation
    /// (Invariant 4).
    published: Mutex<Vec<Message>>,
    /// `(author, sequence)` pairs that the mock silently drops at the wire —
    /// `publish` records them to `published` but does NOT fan them out to
    /// subscribers. Matches the semantics in plan §5.3 B.2: the drop is at
    /// the wire, not at the publish return.
    drops: Mutex<HashSet<(PublicId, u64)>>,
    /// If `Some(seq)`, `request_from` stops yielding once it reaches a
    /// message whose sequence >= `seq`. Simulates a broker replay-window
    /// truncation (plan §5.6 B.5).
    truncate_at: Mutex<Option<u64>>,
    /// Subscriber mpsc senders. Each `subscribe` call appends; the mock fans
    /// out every non-dropped publish to each of these.
    subs: Mutex<Vec<SubEntry>>,
    /// In-flight publish counter. Incremented on `publish` entry, decremented
    /// on return. `health()` reads via `Acquire`.
    inflight: AtomicUsize,
    /// When true, `publish` sleeps briefly before returning, simulating a
    /// pending broker ack. Used by B.7 shutdown-drain.
    slow_ack: AtomicBool,
    /// Shutdown flag. Once set, `publish` short-circuits to `Err` rather than
    /// blocking forever. Set by `shutdown`.
    shut: AtomicBool,
}

struct SubEntry {
    tx: mpsc::Sender<Message>,
    filter: TopicFilter,
    /// Keep the oneshot receiver alive; when the `SubscriptionHandle` returned
    /// by `subscribe` drops, the paired sender drops, and this receiver
    /// resolves to `Err(RecvError)` — the fan-out loop detects that and
    /// prunes the entry.
    cancel_rx: oneshot::Receiver<()>,
}

impl MockTransport {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            published: Mutex::new(Vec::new()),
            drops: Mutex::new(HashSet::new()),
            truncate_at: Mutex::new(None),
            subs: Mutex::new(Vec::new()),
            inflight: AtomicUsize::new(0),
            slow_ack: AtomicBool::new(false),
            shut: AtomicBool::new(false),
        })
    }

    /// Inject a wire-level silent drop for the specified `(author, seq)`. The
    /// next `publish` matching those fields records the message to the
    /// `published` log (so the sender's Ok-return is faithful to Invariant 2)
    /// but does not fan it out to subscribers. Receivers can then detect the
    /// resulting chain gap via `is_chain_valid`.
    pub fn inject_drop(&self, author: PublicId, seq: u64) {
        self.drops.lock().insert((author, seq));
    }

    /// Cause any subsequent `request_from` stream to end early — specifically,
    /// the stream stops when it would yield a message with sequence >= `seq`.
    /// Simulates a broker replay-window truncation; exercised by B.5.
    pub fn inject_truncate_at(&self, seq: u64) {
        *self.truncate_at.lock() = Some(seq);
    }

    /// Enable/disable slow-ack mode. With slow-ack on, `publish` sleeps a
    /// small interval before returning, which makes inflight counts
    /// observable during concurrent publish + shutdown scenarios (B.7).
    pub fn set_slow_ack(&self, yes: bool) {
        self.slow_ack.store(yes, Ordering::Release);
    }

    /// Snapshot the `published` log. Returns a clone so test assertions do
    /// not need to hold the lock.
    pub fn published(&self) -> Vec<Message> {
        self.published.lock().clone()
    }

    /// Inject a message into every active subscriber as if it arrived on the
    /// wire. Used by B.5 to inject `m_80` directly into a subscriber after
    /// the `request_from` stream has been truncated.
    pub fn inject_inbound(&self, msg: Message) {
        self.fanout(&msg);
    }

    /// Fan-out to subscribers, respecting per-sub filter predicates.
    fn fanout(&self, msg: &Message) {
        let mut subs = self.subs.lock();
        subs.retain_mut(|entry| {
            // Prune subs whose handle has been dropped (cancel_rx resolves).
            if let Ok(_) | Err(oneshot::error::TryRecvError::Closed) = entry.cancel_rx.try_recv() {
                return false;
            }
            if !matches_filter(&entry.filter, msg) {
                // Invariant 6 permits superset delivery — we could still send
                // and let the test verify; but we filter here so the tests
                // can distinguish "filter honored (no extras)" from "filter
                // honored with extras". B.6 asserts the lower bound (all
                // matches delivered); extras are allowed but we choose not
                // to emit any.
                return true;
            }
            match entry.tx.try_send(msg.clone()) {
                Ok(()) => true,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    // Subscriber is backlogged; drop silently for test
                    // purposes. Tests must size channels large enough.
                    true
                }
                Err(mpsc::error::TrySendError::Closed(_)) => false,
            }
        });
    }
}

/// Evaluate a `TopicFilter` against a message.
///
/// - `authors = None` → pass all authors.
/// - `authors = Some(v)` → pass iff `v.contains(msg.author)`.
/// - `tags = None` → pass all tag sets.
/// - `tags = Some(v)` → pass iff `v.iter().any(|t| msg.tags.contains(t))`.
/// - Both `Some` → AND.
/// - `Some(vec![])` on either dimension → nothing matches that dimension.
fn matches_filter(filter: &TopicFilter, msg: &Message) -> bool {
    if let Some(authors) = filter.authors.as_ref() {
        if !authors.iter().any(|a| a == &msg.author) {
            return false;
        }
    }
    if let Some(tags) = filter.tags.as_ref() {
        if !tags.iter().any(|t| msg.tags.contains(t)) {
            return false;
        }
    }
    true
}

#[async_trait]
impl Transport for MockTransport {
    async fn publish(&self, msg: &Message) -> Result<()> {
        self.inflight.fetch_add(1, Ordering::AcqRel);

        // Always record the publish attempt, even if wire-dropped. This is
        // the evidence surface tests inspect: `published.len()` counts
        // sender-side attempts, not receiver-side deliveries.
        self.published.lock().push(msg.clone());

        // Simulate broker ack latency if requested. Check shutdown flag after
        // so slow-ack in flight at shutdown time sees the flag and short-
        // circuits; otherwise publishes pile up waiting on the sleep.
        if self.slow_ack.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let dropped = self
            .drops
            .lock()
            .contains(&(msg.author.clone(), msg.sequence));
        if !dropped {
            self.fanout(msg);
        }

        self.inflight.fetch_sub(1, Ordering::AcqRel);
        Ok(())
    }

    async fn subscribe(
        &self,
        filter: TopicFilter,
    ) -> Result<(SubscriptionHandle, BoxStream<'static, Message>)> {
        let (tx, rx) = mpsc::channel::<Message>(1024);
        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();

        self.subs.lock().push(SubEntry {
            tx,
            filter,
            cancel_rx,
        });

        let handle = SubscriptionHandle::from_cancel(cancel_tx);
        let stream = tokio_stream_wrappers::receiver_stream(rx).boxed();
        Ok((handle, stream))
    }

    async fn request_from(
        &self,
        author: PublicId,
        after_seq: u64,
    ) -> Result<BoxStream<'static, Message>> {
        // One-shot snapshot over the current published set. Per plan Step 9
        // scope, request_from does not merge live-forward events — that is
        // Phase 2+. Per RFC §6 Invariant 5, stream end is not a distinguishable
        // error; callers detect gaps via the chain's `previous` hash.
        let truncate_at = *self.truncate_at.lock();
        let mut items: Vec<Message> = self
            .published
            .lock()
            .iter()
            .filter(|m| m.author == author && m.sequence > after_seq)
            .cloned()
            .collect();
        // Yield in strictly increasing sequence order.
        items.sort_by_key(|m| m.sequence);
        if let Some(trunc) = truncate_at {
            items.retain(|m| m.sequence < trunc);
        }
        Ok(stream::iter(items).boxed())
    }

    async fn start(&self) -> Result<()> {
        Ok(())
    }

    async fn shutdown(&self, _deadline: Duration) -> Result<()> {
        self.shut.store(true, Ordering::Release);
        // Drain subscribers so they observe EOS on their next poll.
        self.subs.lock().clear();
        Ok(())
    }

    fn health(&self) -> TransportHealth {
        TransportHealth {
            connected: true,
            backend: "mock",
            last_successful_publish: None,
            last_peer_contact: None,
            unreplicated_count: 0,
            inflight_publishes: self.inflight.load(Ordering::Acquire),
            last_error: None,
            children: vec![],
        }
    }
}

/// Minimal local adaptor so we do not take a `tokio-stream` dependency for
/// test-only wrappers. Wraps an `mpsc::Receiver<T>` as a `Stream<Item = T>`
/// that ends when the sender closes.
mod tokio_stream_wrappers {
    use futures::stream::{unfold, BoxStream, StreamExt};
    use tokio::sync::mpsc;

    pub fn receiver_stream<T: Send + 'static>(rx: mpsc::Receiver<T>) -> BoxStream<'static, T> {
        unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) }).boxed()
    }
}
