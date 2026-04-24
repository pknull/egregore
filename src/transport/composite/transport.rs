//! `CompositeTransport` — multi-transport bridge adapter (RFC 0002 §8).
//!
//! Composes two or more child `Transport`s into a single transport that
//! fans local publishes to every child and bridges child→child traffic
//! through per-destination ingress/egress task pairs that enforce the
//! RFC 0002 §8.2 bounded queue + watermark backpressure model.
//!
//! Wave 3 step landing:
//!
//! - Step 15 (this file, initial): struct + trait skeleton, ack_barriers
//!   field, lifecycle handles; subscribe/request_from/publish/start/shutdown
//!   carry `todo!()` bodies to be filled in Steps 16/19/20.
//! - Step 16: `publish` (local fan-out), `subscribe` (merged children),
//!   `request_from` (children in order).
//! - Step 17/18: `run_ingress` / `run_egress` spawned by `start`.
//! - Step 19: `start` / `shutdown` with drain semantics.
//! - Step 20: `health` aggregation + `BridgeQueuesHealth`.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
use futures::stream::{BoxStream, StreamExt};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::error::{EgreError, Result};
use crate::feed::models::Message;
use crate::identity::PublicId;
use crate::transport::bus::BusTransport;
use crate::transport::filter::TopicFilter;
use crate::transport::health::TransportHealth;
use crate::transport::subscription::SubscriptionHandle;
use crate::transport::trait_def::Transport;

use super::direction::{AckBarrier, DirectionState};

/// How a child transport is attached to the composite.
///
/// The composite cannot downcast `Arc<dyn Transport>` to `Arc<BusTransport>`
/// at construction time (trait object erasure), but the egress task needs
/// the concrete bus handle to call `ack_after_publish` on final-destination
/// resolution. Callers (Wave 4's `main.rs`) still have the concrete type
/// when they build the composite, so they pass both handles through
/// `ChildSpec`. Non-bus children leave `bus_handle = None`.
pub struct ChildSpec {
    /// The transport as a trait object — used for `publish`, `subscribe`,
    /// `request_from`, `start`, `shutdown`, `health`.
    pub transport: Arc<dyn Transport>,
    /// When the child is a `BusTransport`, the concrete handle is retained
    /// here so `egress` can call `ack_after_publish(hash)` after a
    /// bus-sourced message has resolved against every destination. `None`
    /// for gossip and other non-bus children.
    pub bus_handle: Option<Arc<BusTransport>>,
}

impl ChildSpec {
    /// Construct a gossip-style (no-ack) child spec.
    pub fn gossip(transport: Arc<dyn Transport>) -> Self {
        Self {
            transport,
            bus_handle: None,
        }
    }

    /// Construct a bus child spec — the concrete handle is required for
    /// the egress → ack_after_publish path.
    pub fn bus(transport: Arc<dyn Transport>, bus_handle: Arc<BusTransport>) -> Self {
        Self {
            transport,
            bus_handle: Some(bus_handle),
        }
    }
}

/// Multi-transport bridge adapter (RFC 0002 §8).
///
/// Field visibility is `pub(crate)` so Steps 17–20 modules
/// (`ingress`, `egress`, `health`) can read them without an accessor
/// ceremony. Callers outside the crate construct via `new` + drive via
/// the `Transport` trait only.
#[allow(dead_code)] // consumed in Steps 16–20 (publish, ingress, egress, start, shutdown, health)
pub struct CompositeTransport {
    /// Child transports as trait objects. Index = `TransportId`.
    pub(crate) children: Vec<Arc<dyn Transport>>,

    /// Concrete bus handles parallel to `children`. `bus_children[i]` is
    /// `Some(bus)` iff child `i` is a bus transport. Populated from
    /// `ChildSpec.bus_handle` at construction.
    pub(crate) bus_children: Vec<Option<Arc<BusTransport>>>,

    /// One `DirectionState` per child. `directions[j]` holds queues for
    /// messages going TO child `j` (from every `i != j`).
    pub(crate) directions: Vec<Arc<DirectionState>>,

    /// Single-shot start latch — `compare_exchange(false, true, AcqRel, Acquire)`
    /// makes `start` idempotent.
    pub(crate) started: AtomicBool,

    /// Cancel signal for every spawned ingress/egress task. Fired by
    /// `shutdown`; ingress/egress loops observe via `tokio::select!` +
    /// `cancel.cancelled()`.
    pub(crate) cancel: CancellationToken,

    /// Handles for the per-child ingress tasks. `JoinHandle` is abortable;
    /// `Mutex<Vec<_>>` protects the concurrent `start`/`shutdown` accessors.
    /// Wrapped in `Option` so `shutdown` can `take()` the vec without
    /// leaving a "poisoned empty" marker that confuses a second shutdown.
    pub(crate) ingress_handles: Mutex<Option<Vec<JoinHandle<()>>>>,

    /// Handles for the per-destination egress tasks. Parallel layout to
    /// `ingress_handles`.
    pub(crate) egress_handles: Mutex<Option<Vec<JoinHandle<()>>>>,

    /// Per-message ack barriers keyed by `message.hash` (amendment §C.5).
    /// Populated by ingress BEFORE pushing to any destination queue;
    /// decremented by egress on each destination's publish resolution;
    /// removed by the egress thread that observes the transition-to-zero.
    ///
    /// `DashMap` because there are many concurrent writers (ingress
    /// inserters + egress resolvers) and no single-writer phase. The map
    /// holds `Arc<AckBarrier>` so egress tasks on different destinations
    /// share the same barrier instance.
    pub(crate) ack_barriers: Arc<DashMap<String, Arc<AckBarrier>>>,
}

impl CompositeTransport {
    /// Construct a composite over `children`.
    ///
    /// Errors when fewer than 2 children are supplied — `CompositeTransport`
    /// is a multi-transport bridge; a single child would degrade to a
    /// pass-through and callers should use the concrete child directly.
    /// This mirrors RFC 0001 §5.2 + RFC 0002 §4 Principle 7 (the startup
    /// WARN line fires on `transport_count() >= 2`).
    pub fn new(specs: Vec<ChildSpec>) -> Result<Self> {
        if specs.len() < 2 {
            return Err(EgreError::Config {
                reason: format!(
                    "composite requires at least 2 children, got {}",
                    specs.len()
                ),
            });
        }
        let mut children = Vec::with_capacity(specs.len());
        let mut bus_children = Vec::with_capacity(specs.len());
        for spec in specs {
            children.push(spec.transport);
            bus_children.push(spec.bus_handle);
        }
        let directions = (0..children.len())
            .map(|_| Arc::new(DirectionState::new()))
            .collect();

        Ok(Self {
            children,
            bus_children,
            directions,
            started: AtomicBool::new(false),
            cancel: CancellationToken::new(),
            ingress_handles: Mutex::new(None),
            egress_handles: Mutex::new(None),
            ack_barriers: Arc::new(DashMap::new()),
        })
    }
}

#[async_trait]
impl Transport for CompositeTransport {
    /// Fan out a locally-authored publish to every child in parallel.
    ///
    /// `publish` is the LOCAL path — Wave 4's `publish_dispatcher` calls it
    /// for messages the node itself authored. Forwarded cross-transport
    /// traffic goes through the ingress→egress pipeline (Steps 17+18), not
    /// through this method.
    ///
    /// Aggregation: `Ok(())` iff every child returned `Ok`. On any failure
    /// we surface a compound error naming the first failing child; the
    /// aggregate `TransportHealth.last_error` carries the per-child
    /// detail. We do NOT stop fanning out on the first error — partial
    /// delivery (to the children that succeeded) is Invariant 3's
    /// at-least-once posture in action.
    async fn publish(&self, msg: &Message) -> Result<()> {
        let futures = self
            .children
            .iter()
            .map(|child| child.publish(msg))
            .collect::<Vec<_>>();
        let results = futures::future::join_all(futures).await;

        let errors: Vec<(usize, EgreError)> = results
            .into_iter()
            .enumerate()
            .filter_map(|(i, r)| r.err().map(|e| (i, e)))
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            let total = self.children.len();
            let failed = errors.len();
            let (first_idx, first_err) = &errors[0];
            Err(EgreError::Peer {
                reason: format!(
                    "composite publish: {failed} of {total} children failed (first child \
                     {first_idx}: {first_err})"
                ),
            })
        }
    }

    /// Merge-subscribe across every child.
    ///
    /// Local-bridge consumers (the forwarding ingress) subscribe per-child
    /// directly — they want per-source attribution for fairness accounting.
    /// This method is provided for the RFC 0001 §5.1 shape (completeness of
    /// the `Transport` trait on a composite) and for any future consumer
    /// that wants a single merged stream. Implementation spawns a pump task
    /// per child that forwards into a shared `mpsc`; dropping the returned
    /// `SubscriptionHandle` fires a cancel token that terminates each
    /// pump's loop.
    ///
    /// Invariant 1 (per-author FIFO) is preserved because every author
    /// originates on exactly one child in a correctly-configured bridge
    /// (the forwarding layer guarantees the other child got a copy via
    /// ingress, not via re-origination). Out-of-order interleavings across
    /// authors are acceptable under Invariant 1.
    async fn subscribe(
        &self,
        filter: TopicFilter,
    ) -> Result<(SubscriptionHandle, BoxStream<'static, Message>)> {
        // Shared cancel token — the returned handle holds the oneshot
        // sender; dropping it fires the child cancel token via a separate
        // supervisor task. This matches the opaque SubscriptionHandle
        // shape (oneshot::Sender<()>).
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        let cancel_token = CancellationToken::new();

        // Fan-in: per-child subscribes → forward into a single mpsc.
        // Buffer sized generously: this merged stream is used by non-hot
        // paths (observability, future tooling); for the bridge forwarding
        // path, per-child subscribe on the source transport is called
        // directly by the ingress task.
        let (tx, rx) = mpsc::channel::<Message>(1024);

        // Open per-child subscriptions up front so that any error on the
        // subscribe call surfaces synchronously (before the handle is
        // handed back). Streams + handles move into the pump tasks below.
        let mut pumps = Vec::with_capacity(self.children.len());
        for (idx, child) in self.children.iter().enumerate() {
            let (child_handle, child_stream) =
                child.subscribe(filter.clone()).await.map_err(|e| {
                    EgreError::Peer {
                        reason: format!("composite subscribe: child {idx} failed: {e}"),
                    }
                })?;
            pumps.push((child_handle, child_stream));
        }

        // Spawn one pump task per child. The child_handle is held inside
        // the task; dropping it (at task exit) fires the child's cancel.
        for (child_handle, child_stream) in pumps {
            let tx_clone = tx.clone();
            let cancel_child = cancel_token.clone();
            tokio::spawn(async move {
                // Own the handle in the task so it drops when the task
                // exits — this is the child's cancel signal.
                let _owned_handle = child_handle;
                let mut stream = child_stream;
                loop {
                    tokio::select! {
                        biased;
                        _ = cancel_child.cancelled() => break,
                        next = stream.next() => match next {
                            None => break,
                            Some(msg) => {
                                if tx_clone.send(msg).await.is_err() {
                                    // Consumer dropped the receiver side.
                                    break;
                                }
                            }
                        }
                    }
                }
            });
        }

        // Supervisor: when the opaque handle drops, `cancel_rx` resolves
        // `Err(RecvError)`, we fire the child cancel token, and every
        // pump task exits (which drops its child_handle, cancelling the
        // child subscription).
        let supervisor_token = cancel_token.clone();
        tokio::spawn(async move {
            let _ = cancel_rx.await;
            supervisor_token.cancel();
        });

        // Drop our tx clone so the receiver closes once every pump exits
        // (rather than hanging forever on an always-live sender).
        drop(tx);

        let handle = SubscriptionHandle::from_cancel(cancel_tx);
        let stream = async_stream::stream! {
            let mut rx = rx;
            while let Some(msg) = rx.recv().await {
                yield msg;
            }
        };
        Ok((handle, Box::pin(stream)))
    }

    /// Dumb-simple v1 per RFC 0002 §5.2: try children in order; return the
    /// first one that gives a non-empty stream. An empty stream is NOT
    /// treated as "try the next" because an empty stream is indistinguishable
    /// from head-of-feed under Invariant 5 — the caller's chain-gap
    /// detection recovers the missing range. We fall over only on `Err(_)`
    /// (the child itself couldn't serve the request).
    async fn request_from(
        &self,
        author: PublicId,
        after_seq: u64,
    ) -> Result<BoxStream<'static, Message>> {
        let total = self.children.len();
        let mut last_err: Option<EgreError> = None;
        for (idx, child) in self.children.iter().enumerate() {
            match child.request_from(author.clone(), after_seq).await {
                Ok(stream) => return Ok(stream),
                Err(e) => {
                    tracing::debug!(
                        child_idx = idx,
                        error = %e,
                        "composite request_from: child rejected, trying next"
                    );
                    last_err = Some(e);
                }
            }
        }
        let reason = match last_err {
            Some(e) => format!("composite request_from: all {total} children failed ({e})"),
            None => format!("composite request_from: no children (total={total})"),
        };
        Err(EgreError::Peer { reason })
    }

    async fn start(&self) -> Result<()> {
        // Step 19 implementation.
        unimplemented!("CompositeTransport::start lands in Wave 3 Step 19")
    }

    async fn shutdown(&self, _deadline: Duration) -> Result<()> {
        // Step 19 implementation.
        unimplemented!("CompositeTransport::shutdown lands in Wave 3 Step 19")
    }

    fn health(&self) -> TransportHealth {
        // Step 20 implementation.
        unimplemented!("CompositeTransport::health lands in Wave 3 Step 20")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::Utc;
    use futures::stream::{self, BoxStream};
    use parking_lot::Mutex as PlMutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tokio::sync::mpsc as tmpsc;
    use tokio::sync::oneshot;

    fn sample_message(author_id: &str, seq: u64) -> Message {
        Message {
            author: PublicId(author_id.to_string()),
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
            hash: format!("hash-{author_id}-{seq}"),
            signature: "sig".to_string(),
        }
    }

    /// Minimal no-op child for construction tests — all trait methods
    /// panic because Step 15 tests exercise only shape, not behavior.
    struct StubChild;

    #[async_trait]
    impl Transport for StubChild {
        async fn publish(&self, _msg: &Message) -> Result<()> {
            unreachable!("StubChild::publish not used in Step 15 tests")
        }
        async fn subscribe(
            &self,
            _filter: TopicFilter,
        ) -> Result<(SubscriptionHandle, BoxStream<'static, Message>)> {
            unreachable!("StubChild::subscribe not used in Step 15 tests")
        }
        async fn request_from(
            &self,
            _author: PublicId,
            _after_seq: u64,
        ) -> Result<BoxStream<'static, Message>> {
            unreachable!("StubChild::request_from not used in Step 15 tests")
        }
        async fn start(&self) -> Result<()> {
            unreachable!()
        }
        async fn shutdown(&self, _deadline: Duration) -> Result<()> {
            unreachable!()
        }
        fn health(&self) -> TransportHealth {
            unreachable!()
        }
    }

    /// Unit-scope mock — small and focused on Step 16's assertions.
    /// The full `tests/common/mock_transport.rs` lives in an integration
    /// crate and can't be imported into `#[cfg(test)]` lib code.
    struct MockChild {
        /// Messages handed to `publish`. FIFO preserved.
        published: Arc<PlMutex<Vec<Message>>>,
        /// Configured return value for `publish`.
        publish_result: Arc<PlMutex<Result<()>>>,
        /// Messages that `subscribe` should yield, in order.
        subscribe_script: Arc<PlMutex<Vec<Message>>>,
        /// Configured return for `request_from` — either yield messages or
        /// fail the call outright.
        request_from_outcome: Arc<PlMutex<RequestFromOutcome>>,
        /// Flag set on subscribe to verify subscribe was actually called.
        subscribe_called: Arc<AtomicBool>,
    }

    enum RequestFromOutcome {
        Stream(Vec<Message>),
        Err(String),
    }

    impl MockChild {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                published: Arc::new(PlMutex::new(Vec::new())),
                publish_result: Arc::new(PlMutex::new(Ok(()))),
                subscribe_script: Arc::new(PlMutex::new(Vec::new())),
                request_from_outcome: Arc::new(PlMutex::new(RequestFromOutcome::Stream(vec![]))),
                subscribe_called: Arc::new(AtomicBool::new(false)),
            })
        }

        fn set_publish_error(&self, reason: &str) {
            *self.publish_result.lock() = Err(EgreError::Peer {
                reason: reason.to_string(),
            });
        }

        fn push_subscribe_message(&self, msg: Message) {
            self.subscribe_script.lock().push(msg);
        }

        fn set_request_from_stream(&self, msgs: Vec<Message>) {
            *self.request_from_outcome.lock() = RequestFromOutcome::Stream(msgs);
        }

        fn set_request_from_err(&self, reason: &str) {
            *self.request_from_outcome.lock() = RequestFromOutcome::Err(reason.to_string());
        }

        fn published(&self) -> Vec<Message> {
            self.published.lock().clone()
        }
    }

    #[async_trait]
    impl Transport for MockChild {
        async fn publish(&self, msg: &Message) -> Result<()> {
            self.published.lock().push(msg.clone());
            // Clone the configured result so we can return it without
            // mutating the stored value.
            match &*self.publish_result.lock() {
                Ok(()) => Ok(()),
                Err(EgreError::Peer { reason }) => Err(EgreError::Peer {
                    reason: reason.clone(),
                }),
                Err(e) => Err(EgreError::Peer {
                    reason: format!("{e}"),
                }),
            }
        }

        async fn subscribe(
            &self,
            _filter: TopicFilter,
        ) -> Result<(SubscriptionHandle, BoxStream<'static, Message>)> {
            self.subscribe_called.store(true, Ordering::Release);
            let script: Vec<Message> = self.subscribe_script.lock().clone();
            let (cancel_tx, _cancel_rx) = oneshot::channel::<()>();
            let handle = SubscriptionHandle::from_cancel(cancel_tx);
            // Use an mpsc so the stream ends when the sender drops — matches
            // real adapters' "subscription ends on drop" shape (Invariant 5).
            let (tx, mut rx) = tmpsc::channel::<Message>(1024);
            for m in script {
                tx.send(m).await.unwrap();
            }
            drop(tx);
            let stream = async_stream::stream! {
                while let Some(m) = rx.recv().await { yield m; }
            };
            Ok((handle, Box::pin(stream)))
        }

        async fn request_from(
            &self,
            _author: PublicId,
            _after_seq: u64,
        ) -> Result<BoxStream<'static, Message>> {
            match &*self.request_from_outcome.lock() {
                RequestFromOutcome::Stream(msgs) => {
                    let cloned = msgs.clone();
                    Ok(Box::pin(stream::iter(cloned)))
                }
                RequestFromOutcome::Err(reason) => Err(EgreError::Peer {
                    reason: reason.clone(),
                }),
            }
        }

        async fn start(&self) -> Result<()> {
            Ok(())
        }
        async fn shutdown(&self, _deadline: Duration) -> Result<()> {
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

    // ---- Step 15 tests (construction) ----

    #[test]
    fn new_rejects_fewer_than_two_children() {
        let empty = CompositeTransport::new(vec![]);
        assert!(empty.is_err(), "zero children must be rejected");
        let single =
            CompositeTransport::new(vec![ChildSpec::gossip(Arc::new(StubChild) as Arc<_>)]);
        assert!(
            single.is_err(),
            "single-child composite is a pass-through; caller must use child directly"
        );
    }

    #[test]
    fn new_populates_one_direction_per_child() {
        let specs = vec![
            ChildSpec::gossip(Arc::new(StubChild) as Arc<_>),
            ChildSpec::gossip(Arc::new(StubChild) as Arc<_>),
            ChildSpec::gossip(Arc::new(StubChild) as Arc<_>),
        ];
        let composite = CompositeTransport::new(specs).unwrap();
        assert_eq!(composite.children.len(), 3);
        assert_eq!(composite.bus_children.len(), 3);
        assert_eq!(composite.directions.len(), 3);
        for slot in &composite.bus_children {
            assert!(slot.is_none(), "gossip spec leaves bus_handle=None");
        }
        for dir in &composite.directions {
            assert!(dir.queues.lock().is_empty());
            assert_eq!(dir.backpressure_events.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn new_starts_cancel_token_unfired() {
        let composite = CompositeTransport::new(vec![
            ChildSpec::gossip(Arc::new(StubChild) as Arc<_>),
            ChildSpec::gossip(Arc::new(StubChild) as Arc<_>),
        ])
        .unwrap();
        assert!(
            !composite.cancel.is_cancelled(),
            "fresh composite must have a live (unfired) cancel token"
        );
        assert!(!composite.started.load(Ordering::Acquire));
        assert!(composite.ack_barriers.is_empty());
    }

    // ---- Step 16 tests ----

    #[tokio::test]
    async fn publish_fans_to_all_children_on_success() {
        let mock_a = MockChild::new();
        let mock_b = MockChild::new();
        let mock_c = MockChild::new();
        let composite = CompositeTransport::new(vec![
            ChildSpec::gossip(mock_a.clone() as Arc<_>),
            ChildSpec::gossip(mock_b.clone() as Arc<_>),
            ChildSpec::gossip(mock_c.clone() as Arc<_>),
        ])
        .unwrap();
        let msg = sample_message("@alice.ed25519", 1);
        composite.publish(&msg).await.expect("publish should succeed");
        assert_eq!(mock_a.published().len(), 1);
        assert_eq!(mock_b.published().len(), 1);
        assert_eq!(mock_c.published().len(), 1);
        assert_eq!(mock_a.published()[0].hash, msg.hash);
    }

    #[tokio::test]
    async fn publish_reports_compound_error_when_one_fails() {
        let mock_a = MockChild::new();
        let mock_b = MockChild::new();
        mock_b.set_publish_error("simulated bus publish failure");
        let mock_c = MockChild::new();
        let composite = CompositeTransport::new(vec![
            ChildSpec::gossip(mock_a.clone() as Arc<_>),
            ChildSpec::gossip(mock_b.clone() as Arc<_>),
            ChildSpec::gossip(mock_c.clone() as Arc<_>),
        ])
        .unwrap();
        let msg = sample_message("@alice.ed25519", 1);
        let err = composite
            .publish(&msg)
            .await
            .expect_err("mock_b error must surface");
        let msg_str = format!("{err}");
        assert!(
            msg_str.contains("1 of 3 children failed"),
            "compound error must name count; got: {msg_str}"
        );
        assert!(
            msg_str.contains("child 1"),
            "compound error must name failing index; got: {msg_str}"
        );
        // Even on compound error, the children that succeeded did receive
        // the publish — at-least-once (Invariant 3) partial fan-out is
        // intentional.
        assert_eq!(
            mock_a.published().len(),
            1,
            "successful children still receive the publish"
        );
        assert_eq!(mock_c.published().len(), 1);
    }

    #[tokio::test]
    async fn subscribe_yields_messages_from_any_child() {
        let mock_a = MockChild::new();
        let mock_b = MockChild::new();
        mock_a.push_subscribe_message(sample_message("@alice.ed25519", 1));
        mock_b.push_subscribe_message(sample_message("@bob.ed25519", 1));
        let composite = CompositeTransport::new(vec![
            ChildSpec::gossip(mock_a.clone() as Arc<_>),
            ChildSpec::gossip(mock_b.clone() as Arc<_>),
        ])
        .unwrap();
        let (_handle, mut stream) = composite
            .subscribe(TopicFilter::default())
            .await
            .expect("subscribe");

        let mut received = Vec::new();
        for _ in 0..2 {
            match tokio::time::timeout(Duration::from_secs(2), stream.next()).await {
                Ok(Some(m)) => received.push(m),
                Ok(None) => break,
                Err(_) => panic!("subscribe stream timed out waiting for scripted message"),
            }
        }
        assert_eq!(received.len(), 2);
        let hashes: std::collections::HashSet<_> = received.iter().map(|m| &m.hash).collect();
        assert!(hashes.contains(&"hash-@alice.ed25519-1".to_string()));
        assert!(hashes.contains(&"hash-@bob.ed25519-1".to_string()));
        assert!(mock_a.subscribe_called.load(Ordering::Acquire));
        assert!(mock_b.subscribe_called.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn request_from_returns_first_successful_child() {
        let mock_a = MockChild::new();
        // mock_a fails — composite must fall over to mock_b.
        mock_a.set_request_from_err("simulated gossip failure");
        let mock_b = MockChild::new();
        let expected = vec![
            sample_message("@alice.ed25519", 2),
            sample_message("@alice.ed25519", 3),
        ];
        mock_b.set_request_from_stream(expected.clone());

        let composite = CompositeTransport::new(vec![
            ChildSpec::gossip(mock_a as Arc<_>),
            ChildSpec::gossip(mock_b as Arc<_>),
        ])
        .unwrap();
        let mut stream = composite
            .request_from(PublicId("@alice.ed25519".into()), 1)
            .await
            .expect("request_from");
        let mut out = Vec::new();
        while let Some(m) = stream.next().await {
            out.push(m);
        }
        assert_eq!(out.len(), 2, "mock_b's stream should be chosen");
        assert_eq!(out[0].sequence, 2);
        assert_eq!(out[1].sequence, 3);
    }

    #[tokio::test]
    async fn request_from_errors_when_all_children_fail() {
        let mock_a = MockChild::new();
        let mock_b = MockChild::new();
        mock_a.set_request_from_err("err_a");
        mock_b.set_request_from_err("err_b");
        let composite = CompositeTransport::new(vec![
            ChildSpec::gossip(mock_a as Arc<_>),
            ChildSpec::gossip(mock_b as Arc<_>),
        ])
        .unwrap();
        let err = match composite
            .request_from(PublicId("@alice.ed25519".into()), 0)
            .await
        {
            Err(e) => e,
            Ok(_) => panic!("all-children-fail must surface as Err, got Ok"),
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("all 2 children failed"),
            "error must name total; got {msg}"
        );
    }
}
