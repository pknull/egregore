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
use crate::transport::filter::TopicFilter;
use crate::transport::health::TransportHealth;
use crate::transport::subscription::SubscriptionHandle;
use crate::transport::trait_def::Transport;

use super::direction::{AckBarrier, DirectionState};

/// How a child transport is attached to the composite.
///
/// `ack_after_publish` and `self_echo_total` are now trait methods with
/// default no-op / default-zero implementations; bus-specific concerns are
/// resolved through trait dispatch on `Arc<dyn Transport>`, so the composite
/// no longer needs to retain a parallel concrete bus handle.
pub struct ChildSpec {
    /// The transport as a trait object — used for `publish`, `subscribe`,
    /// `request_from`, `start`, `shutdown`, `health`, and the composite-only
    /// extension hooks `ack_after_publish` / `self_echo_total`.
    pub transport: Arc<dyn Transport>,
}

impl ChildSpec {
    /// Construct a child spec from any `Transport` implementation.
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self { transport }
    }
}

/// Multi-transport bridge adapter (RFC 0002 §8).
///
/// Field visibility is `pub(crate)` so Steps 17–20 modules
/// (`ingress`, `egress`, `health`) can read them without an accessor
/// ceremony. Callers outside the crate construct via `new` + drive via
/// the `Transport` trait only.
pub struct CompositeTransport {
    /// Child transports as trait objects. Index = `TransportId`. Bus-specific
    /// hooks (`ack_after_publish`, `self_echo_total`) are dispatched through
    /// the trait so no parallel concrete handle is retained.
    pub(crate) children: Vec<Arc<dyn Transport>>,

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
        let children: Vec<Arc<dyn Transport>> =
            specs.into_iter().map(|spec| spec.transport).collect();
        let directions = (0..children.len())
            .map(|_| Arc::new(DirectionState::new()))
            .collect();

        Ok(Self {
            children,
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
    /// Aggregation: best-effort fanout. Once the message has been accepted
    /// and durably stored by the local engine, child transport errors are
    /// surfaced through health / backlog / retry paths rather than as a
    /// synchronous publish failure. We do NOT stop fanning out on the first
    /// error — partial delivery (to the children that succeeded) is
    /// Invariant 3's at-least-once posture in action.
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

        if !errors.is_empty() {
            let total = self.children.len();
            let failed = errors.len();
            let first = errors
                .first()
                .map(|(idx, err)| format!("first child {idx}: {err}"))
                .unwrap_or_else(|| "no child details".to_string());
            tracing::warn!(
                total_children = total,
                failed_children = failed,
                message_hash = %msg.hash,
                summary = %first,
                "composite publish fanout had child errors; local publish remains accepted"
            );
        }

        Ok(())
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
                child
                    .subscribe(filter.clone())
                    .await
                    .map_err(|e| EgreError::Peer {
                        reason: format!("composite subscribe: child {idx} failed: {e}"),
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

    /// Start every child, then spawn one ingress + one egress task per
    /// child.
    ///
    /// Idempotent per the `Transport` trait contract: a second start while
    /// already-started returns `Ok(())` without relaunching anything. On
    /// child-start failure, unwinds by firing the cancel token + resetting
    /// the started latch so a subsequent retry can run from a clean slate.
    async fn start(&self) -> Result<()> {
        use std::sync::atomic::Ordering;

        // Single-shot latch: only the thread that wins the compare_exchange
        // proceeds; the others see Ok(()).
        if self
            .started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(());
        }

        // 1. Start every child (sequentially — children are few, and
        //    sequential keeps error unwinding simple).
        for (i, child) in self.children.iter().enumerate() {
            if let Err(e) = child.start().await {
                // Unwind: cancel any partially-started children's spawned
                // work (we haven't spawned any yet at this point, but
                // the token is shared) + reset the latch.
                self.cancel.cancel();
                self.started.store(false, Ordering::Release);
                return Err(EgreError::Peer {
                    reason: format!("composite start: child {i} failed: {e}"),
                });
            }
        }

        let num_children = self.children.len();
        let directions_arc = Arc::new(self.directions.clone());
        let children_arc = Arc::new(self.children.clone());

        // 2. Spawn one ingress task per child (source).
        let mut ingress_handles = Vec::with_capacity(num_children);
        for (i, child) in self.children.iter().enumerate() {
            let source_id = i;
            let source = child.clone();
            let dirs = directions_arc.clone();
            let barriers = self.ack_barriers.clone();
            let cancel = self.cancel.clone();
            ingress_handles.push(tokio::spawn(async move {
                super::ingress::run_ingress(
                    source_id,
                    source,
                    dirs,
                    barriers,
                    num_children,
                    cancel,
                )
                .await;
            }));
        }
        *self.ingress_handles.lock().await = Some(ingress_handles);

        // 3. Spawn one egress task per destination.
        let mut egress_handles = Vec::with_capacity(num_children);
        for (j, child) in self.children.iter().enumerate() {
            let dest_id = j;
            let dest = child.clone();
            let dir = self.directions[j].clone();
            let barriers = self.ack_barriers.clone();
            let children = children_arc.clone();
            let cancel = self.cancel.clone();
            egress_handles.push(tokio::spawn(async move {
                super::egress::run_egress(dest_id, dest, dir, barriers, children, cancel).await;
            }));
        }
        *self.egress_handles.lock().await = Some(egress_handles);

        Ok(())
    }

    /// Graceful shutdown (Invariant 7).
    ///
    /// Split-deadline policy:
    /// 1. Fire the cancel token so every ingress/egress task exits its loop
    ///    naturally (no aborts yet).
    /// 2. Wait up to `deadline/2` for in-memory queues to drain — egress
    ///    tasks continue publishing until either queues empty or the
    ///    intermediate deadline elapses.
    /// 3. Abort any tasks still running.
    /// 4. Call each child's `shutdown(deadline/2)` in parallel.
    ///
    /// Returns `Err` iff one or more children returned `Err` from their
    /// own shutdown. Any in-memory messages not drained before the
    /// intermediate deadline will be redelivered by NATS (`ack_wait`
    /// expiry on unacked bus messages) or pulled via `request_from`
    /// after restart (gossip-sourced) — both paths survive this shutdown
    /// by design.
    async fn shutdown(&self, deadline: Duration) -> Result<()> {
        // Halve the deadline: half for in-memory drain, half for child
        // shutdowns. saturating_sub guards against zero-deadline callers
        // that would otherwise divide to Duration::ZERO.
        let mid_deadline = deadline / 2;

        self.cancel.cancel();

        // 2. In-memory drain: poll queue depth until all empty or the
        //    intermediate deadline elapses. The cancel is already fired,
        //    so egress tasks drain what they can and then exit their
        //    idle waits naturally.
        let drain_start = tokio::time::Instant::now();
        loop {
            let all_empty = self.directions.iter().all(|dir| {
                let queues = dir.queues.lock();
                queues.values().all(|e| e.queue.is_empty())
            });
            if all_empty {
                break;
            }
            if drain_start.elapsed() >= mid_deadline {
                tracing::warn!(
                    "composite shutdown: mid-deadline elapsed with queued messages remaining"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // 3. Abort any ingress/egress tasks that didn't exit naturally.
        //    `take()` leaves the slot as None so a second shutdown call
        //    is a no-op on these.
        if let Some(handles) = self.ingress_handles.lock().await.take() {
            for h in handles {
                h.abort();
            }
        }
        if let Some(handles) = self.egress_handles.lock().await.take() {
            for h in handles {
                h.abort();
            }
        }

        // 4. Shut down children in parallel with the remaining budget.
        let child_shutdowns = self.children.iter().map(|c| c.shutdown(mid_deadline));
        let results = futures::future::join_all(child_shutdowns).await;
        let errors: Vec<EgreError> = results.into_iter().filter_map(Result::err).collect();
        if errors.is_empty() {
            Ok(())
        } else {
            let total = self.children.len();
            let failed = errors.len();
            Err(EgreError::Peer {
                reason: format!(
                    "composite shutdown: {failed} of {total} children errored ({})",
                    errors[0]
                ),
            })
        }
    }

    /// Per RFC 0002 §8.4: aggregate `TransportHealth.aggregate(...)` across
    /// children, with each child's `bridge_queues` populated from the
    /// matching `DirectionState` snapshot.
    ///
    /// The top-level `TransportHealth.bridge_queues` is always `None` — the
    /// field describes queue state for a destination, not an aggregate.
    /// Each child's `bridge_queues` describes its INBOUND queues
    /// (`directions[j]`: messages going TO child `j` from every other child).
    fn health(&self) -> TransportHealth {
        let children: Vec<TransportHealth> = self
            .children
            .iter()
            .enumerate()
            .map(|(j, child)| {
                let mut child_health = child.health();
                let destination = child_health.backend;
                // Wave 4 Step 22 retcon: self-echo lives on the bus adapter
                // itself (amendment §C.4). The trait's default returns 0
                // for non-bus children, so the call is unconditional.
                child_health.bridge_queues = Some(super::health::compute_bridge_queues_health(
                    &self.directions[j],
                    destination,
                    Some(child.as_ref() as &dyn Transport),
                ));
                child_health
            })
            .collect();

        TransportHealth::aggregate("composite", children)
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
                bridge_queues: None,
            }
        }
    }

    // ---- Step 15 tests (construction) ----

    #[test]
    fn new_rejects_fewer_than_two_children() {
        let empty = CompositeTransport::new(vec![]);
        assert!(empty.is_err(), "zero children must be rejected");
        let single = CompositeTransport::new(vec![ChildSpec::new(Arc::new(StubChild) as Arc<_>)]);
        assert!(
            single.is_err(),
            "single-child composite is a pass-through; caller must use child directly"
        );
    }

    #[test]
    fn new_populates_one_direction_per_child() {
        let specs = vec![
            ChildSpec::new(Arc::new(StubChild) as Arc<_>),
            ChildSpec::new(Arc::new(StubChild) as Arc<_>),
            ChildSpec::new(Arc::new(StubChild) as Arc<_>),
        ];
        let composite = CompositeTransport::new(specs).unwrap();
        assert_eq!(composite.children.len(), 3);
        assert_eq!(composite.directions.len(), 3);
        for dir in &composite.directions {
            assert!(dir.queues.lock().is_empty());
            assert_eq!(dir.backpressure_events.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn new_starts_cancel_token_unfired() {
        let composite = CompositeTransport::new(vec![
            ChildSpec::new(Arc::new(StubChild) as Arc<_>),
            ChildSpec::new(Arc::new(StubChild) as Arc<_>),
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
            ChildSpec::new(mock_a.clone() as Arc<_>),
            ChildSpec::new(mock_b.clone() as Arc<_>),
            ChildSpec::new(mock_c.clone() as Arc<_>),
        ])
        .unwrap();
        let msg = sample_message("@alice.ed25519", 1);
        composite
            .publish(&msg)
            .await
            .expect("publish should succeed");
        assert_eq!(mock_a.published().len(), 1);
        assert_eq!(mock_b.published().len(), 1);
        assert_eq!(mock_c.published().len(), 1);
        assert_eq!(mock_a.published()[0].hash, msg.hash);
    }

    #[tokio::test]
    async fn publish_is_best_effort_when_one_child_fails() {
        let mock_a = MockChild::new();
        let mock_b = MockChild::new();
        mock_b.set_publish_error("simulated bus publish failure");
        let mock_c = MockChild::new();
        let composite = CompositeTransport::new(vec![
            ChildSpec::new(mock_a.clone() as Arc<_>),
            ChildSpec::new(mock_b.clone() as Arc<_>),
            ChildSpec::new(mock_c.clone() as Arc<_>),
        ])
        .unwrap();
        let msg = sample_message("@alice.ed25519", 1);
        composite
            .publish(&msg)
            .await
            .expect("composite publish should stay best-effort");
        // Even with one child failure, the children that succeeded did
        // receive the publish — at-least-once (Invariant 3) partial
        // fan-out is intentional.
        assert_eq!(
            mock_a.published().len(),
            1,
            "successful children still receive the publish"
        );
        assert_eq!(mock_c.published().len(), 1);
        assert_eq!(
            mock_b.published().len(),
            1,
            "the failing child should still be attempted once"
        );
    }

    #[tokio::test]
    async fn subscribe_yields_messages_from_any_child() {
        let mock_a = MockChild::new();
        let mock_b = MockChild::new();
        mock_a.push_subscribe_message(sample_message("@alice.ed25519", 1));
        mock_b.push_subscribe_message(sample_message("@bob.ed25519", 1));
        let composite = CompositeTransport::new(vec![
            ChildSpec::new(mock_a.clone() as Arc<_>),
            ChildSpec::new(mock_b.clone() as Arc<_>),
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
            ChildSpec::new(mock_a as Arc<_>),
            ChildSpec::new(mock_b as Arc<_>),
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

    // ---- Step 19 tests: start / shutdown ----

    /// A source-and-destination mock: subscribe yields scripted messages
    /// (from an mpsc controlled by the test), publish records. Used by
    /// start/shutdown tests to observe full ingress→egress flow.
    struct FullMock {
        published: Arc<PlMutex<Vec<Message>>>,
        tx: Arc<PlMutex<Option<tmpsc::Sender<Message>>>>,
        rx: Arc<PlMutex<Option<tmpsc::Receiver<Message>>>>,
        started: Arc<AtomicBool>,
        shutdown_called: Arc<AtomicBool>,
    }

    impl FullMock {
        fn new(buffer: usize) -> (Arc<Self>, tmpsc::Sender<Message>) {
            let (tx, rx) = tmpsc::channel::<Message>(buffer);
            let mock = Arc::new(Self {
                published: Arc::new(PlMutex::new(Vec::new())),
                tx: Arc::new(PlMutex::new(Some(tx.clone()))),
                rx: Arc::new(PlMutex::new(Some(rx))),
                started: Arc::new(AtomicBool::new(false)),
                shutdown_called: Arc::new(AtomicBool::new(false)),
            });
            (mock, tx)
        }
    }

    #[async_trait]
    impl Transport for FullMock {
        async fn publish(&self, msg: &Message) -> Result<()> {
            self.published.lock().push(msg.clone());
            Ok(())
        }
        async fn subscribe(
            &self,
            _filter: TopicFilter,
        ) -> Result<(SubscriptionHandle, BoxStream<'static, Message>)> {
            let (cancel_tx, _cancel_rx) = oneshot::channel::<()>();
            let handle = SubscriptionHandle::from_cancel(cancel_tx);
            let mut rx = self
                .rx
                .lock()
                .take()
                .expect("FullMock::subscribe called twice");
            drop(self.tx.lock().take());
            let stream = async_stream::stream! {
                while let Some(m) = rx.recv().await { yield m; }
            };
            Ok((handle, Box::pin(stream)))
        }
        async fn request_from(&self, _a: PublicId, _s: u64) -> Result<BoxStream<'static, Message>> {
            Ok(Box::pin(stream::iter(Vec::<Message>::new())))
        }
        async fn start(&self) -> Result<()> {
            self.started.store(true, Ordering::Release);
            Ok(())
        }
        async fn shutdown(&self, _d: Duration) -> Result<()> {
            self.shutdown_called.store(true, Ordering::Release);
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
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            if pred() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        pred()
    }

    #[tokio::test]
    async fn start_spawns_one_ingress_and_one_egress_per_child() {
        let (mock_a, _tx_a) = FullMock::new(16);
        let (mock_b, _tx_b) = FullMock::new(16);
        let composite = Arc::new(
            CompositeTransport::new(vec![
                ChildSpec::new(mock_a.clone() as Arc<_>),
                ChildSpec::new(mock_b.clone() as Arc<_>),
            ])
            .unwrap(),
        );

        composite.start().await.expect("start");

        // Children were started.
        assert!(mock_a.started.load(Ordering::Acquire));
        assert!(mock_b.started.load(Ordering::Acquire));

        // One ingress + one egress handle per child.
        let ingress = composite.ingress_handles.lock().await;
        assert_eq!(ingress.as_ref().unwrap().len(), 2);
        drop(ingress);
        let egress = composite.egress_handles.lock().await;
        assert_eq!(egress.as_ref().unwrap().len(), 2);
        drop(egress);

        // started latch set.
        assert!(composite.started.load(Ordering::Acquire));

        // Cleanup.
        let _ = composite.shutdown(Duration::from_millis(200)).await;
    }

    #[tokio::test]
    async fn start_is_idempotent() {
        let (mock_a, _tx_a) = FullMock::new(16);
        let (mock_b, _tx_b) = FullMock::new(16);
        let composite = Arc::new(
            CompositeTransport::new(vec![
                ChildSpec::new(mock_a.clone() as Arc<_>),
                ChildSpec::new(mock_b.clone() as Arc<_>),
            ])
            .unwrap(),
        );
        composite.start().await.expect("first start");
        // Second start must be a no-op — handle counts unchanged.
        composite.start().await.expect("second start is idempotent");

        let ingress = composite.ingress_handles.lock().await;
        assert_eq!(
            ingress.as_ref().unwrap().len(),
            2,
            "second start must not relaunch ingress tasks"
        );
        drop(ingress);

        let _ = composite.shutdown(Duration::from_millis(200)).await;
    }

    // ---- Step 20 tests: health aggregation ----

    #[tokio::test]
    async fn health_empty_queues_reports_zero_depth_and_no_aging() {
        let (mock_a, _tx_a) = FullMock::new(16);
        let (mock_b, _tx_b) = FullMock::new(16);
        let composite = Arc::new(
            CompositeTransport::new(vec![
                ChildSpec::new(mock_a as Arc<_>),
                ChildSpec::new(mock_b as Arc<_>),
            ])
            .unwrap(),
        );
        let h = composite.health();
        assert_eq!(h.backend, "composite");
        assert_eq!(h.children.len(), 2);
        for child_h in &h.children {
            let bq = child_h
                .bridge_queues
                .as_ref()
                .expect("composite children must carry bridge_queues");
            assert_eq!(bq.depth_total, 0);
            assert_eq!(bq.authors_active, 0);
            assert!(bq.oldest_queued_age_secs.is_none());
            assert!(bq.publish_in_flight_age_secs.is_none());
        }
        assert!(
            h.bridge_queues.is_none(),
            "top-level aggregate never has its own bridge_queues"
        );
    }

    #[tokio::test]
    async fn health_with_queued_messages_reports_depth_and_oldest_age() {
        let (mock_a, _tx_a) = FullMock::new(16);
        let (mock_b, _tx_b) = FullMock::new(16);
        let composite = Arc::new(
            CompositeTransport::new(vec![
                ChildSpec::new(mock_a as Arc<_>),
                ChildSpec::new(mock_b as Arc<_>),
            ])
            .unwrap(),
        );

        // Manually seed directions[1] (destination = mock_b) with 4 messages.
        {
            let mut queues = composite.directions[1].queues.lock();
            let entry = queues
                .entry((0, PublicId("@a.ed25519".into())))
                .or_default();
            for seq in 1..=4 {
                entry
                    .queue
                    .push(Arc::new(sample_message("@a.ed25519", seq)))
                    .unwrap();
            }
        }
        *composite.directions[1].oldest_queued_at.write() =
            Some(std::time::Instant::now() - Duration::from_secs(3));

        let h = composite.health();
        // Child index 1 is the destination with queued work.
        let bq = h.children[1].bridge_queues.as_ref().unwrap();
        assert_eq!(bq.depth_total, 4);
        assert_eq!(bq.authors_active, 1);
        assert!(bq.oldest_queued_age_secs.unwrap() >= 3);

        // Child index 0 is empty.
        let bq0 = h.children[0].bridge_queues.as_ref().unwrap();
        assert_eq!(bq0.depth_total, 0);
    }

    #[tokio::test]
    async fn health_during_publish_reports_in_flight_age() {
        let (mock_a, _tx_a) = FullMock::new(16);
        let (mock_b, _tx_b) = FullMock::new(16);
        let composite = Arc::new(
            CompositeTransport::new(vec![
                ChildSpec::new(mock_a as Arc<_>),
                ChildSpec::new(mock_b as Arc<_>),
            ])
            .unwrap(),
        );
        *composite.directions[1].publish_in_flight_since.write() =
            Some(std::time::Instant::now() - Duration::from_secs(4));
        let h = composite.health();
        let bq = h.children[1].bridge_queues.as_ref().unwrap();
        assert!(
            bq.publish_in_flight_age_secs.unwrap() >= 4,
            "armed publish_in_flight_since must surface as age"
        );
    }

    #[tokio::test]
    async fn health_aggregates_backpressure_events_counter() {
        let (mock_a, _tx_a) = FullMock::new(16);
        let (mock_b, _tx_b) = FullMock::new(16);
        let composite = Arc::new(
            CompositeTransport::new(vec![
                ChildSpec::new(mock_a as Arc<_>),
                ChildSpec::new(mock_b as Arc<_>),
            ])
            .unwrap(),
        );
        composite.directions[1]
            .backpressure_events
            .store(42, Ordering::Relaxed);
        let h = composite.health();
        let bq = h.children[1].bridge_queues.as_ref().unwrap();
        assert_eq!(bq.backpressure_events_total, 42);
    }

    #[tokio::test]
    async fn health_serializes_with_serde_default_fields_omitted_when_none() {
        let (mock_a, _tx_a) = FullMock::new(16);
        let (mock_b, _tx_b) = FullMock::new(16);
        let composite = Arc::new(
            CompositeTransport::new(vec![
                ChildSpec::new(mock_a as Arc<_>),
                ChildSpec::new(mock_b as Arc<_>),
            ])
            .unwrap(),
        );
        let h = composite.health();
        let json = serde_json::to_string(&h).unwrap();
        // Top-level bridge_queues is None → omitted. Children have
        // Some(bridge_queues) → present on each child.
        assert!(json.contains("\"bridge_queues\""));
        assert!(
            !json.contains("oldest_queued_age_secs"),
            "None option must be omitted in empty-queue snapshot; got: {json}"
        );
        assert!(
            !json.contains("publish_in_flight_age_secs"),
            "None option must be omitted; got: {json}"
        );
    }

    #[tokio::test]
    async fn composite_health_top_level_aggregates_children_connected_via_existing_helper() {
        // Aggregation rule: logical-AND on connected. One disconnected child
        // must produce a disconnected composite.
        let (mock_a, _tx_a) = FullMock::new(16);
        let (mock_b, _tx_b) = FullMock::new(16);
        // Both mocks report connected=true by default.
        let composite = Arc::new(
            CompositeTransport::new(vec![
                ChildSpec::new(mock_a.clone() as Arc<_>),
                ChildSpec::new(mock_b.clone() as Arc<_>),
            ])
            .unwrap(),
        );
        let h = composite.health();
        assert!(h.connected, "all children connected → composite connected");
        assert_eq!(h.children.len(), 2);
        assert_eq!(h.backend, "composite");
        // Children keep their own backend strings unchanged.
        assert_eq!(h.children[0].backend, "mock");
        assert_eq!(h.children[1].backend, "mock");
    }

    #[tokio::test]
    async fn shutdown_drains_in_memory_queues_then_cancels() {
        // Two children. Source publishes 3 messages; shutdown should
        // drain them to the destination before aborting.
        let (mock_a, tx_a) = FullMock::new(16);
        let (mock_b, _tx_b) = FullMock::new(16);
        let composite = Arc::new(
            CompositeTransport::new(vec![
                ChildSpec::new(mock_a.clone() as Arc<_>),
                ChildSpec::new(mock_b.clone() as Arc<_>),
            ])
            .unwrap(),
        );
        composite.start().await.expect("start");

        // Seed 3 messages from mock_a — they should flow to mock_b via
        // the composite's bridge pipeline.
        for seq in 1..=3 {
            let m = sample_message("@a.ed25519", seq);
            tx_a.send(m).await.unwrap();
        }
        // Wait for drain to mock_b.
        let hit = eventually(
            || mock_b.published.lock().len() == 3,
            Duration::from_secs(3),
        )
        .await;
        assert!(hit, "all 3 messages must bridge A → B");

        // Now shutdown should complete without error.
        let result = composite.shutdown(Duration::from_millis(500)).await;
        assert!(result.is_ok(), "clean shutdown should return Ok");

        // Children's shutdown was called.
        assert!(mock_a.shutdown_called.load(Ordering::Acquire));
        assert!(mock_b.shutdown_called.load(Ordering::Acquire));

        // Task handle slots are cleared (take()d).
        assert!(composite.ingress_handles.lock().await.is_none());
        assert!(composite.egress_handles.lock().await.is_none());
    }

    #[tokio::test]
    async fn request_from_errors_when_all_children_fail() {
        let mock_a = MockChild::new();
        let mock_b = MockChild::new();
        mock_a.set_request_from_err("err_a");
        mock_b.set_request_from_err("err_b");
        let composite = CompositeTransport::new(vec![
            ChildSpec::new(mock_a as Arc<_>),
            ChildSpec::new(mock_b as Arc<_>),
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
