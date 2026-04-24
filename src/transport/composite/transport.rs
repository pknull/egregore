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
use futures::stream::BoxStream;
use tokio::sync::Mutex;
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
    async fn publish(&self, _msg: &Message) -> Result<()> {
        // Step 16 implementation.
        unimplemented!("CompositeTransport::publish lands in Wave 3 Step 16")
    }

    async fn subscribe(
        &self,
        _filter: TopicFilter,
    ) -> Result<(SubscriptionHandle, BoxStream<'static, Message>)> {
        // Step 16 implementation.
        unimplemented!("CompositeTransport::subscribe lands in Wave 3 Step 16")
    }

    async fn request_from(
        &self,
        _author: PublicId,
        _after_seq: u64,
    ) -> Result<BoxStream<'static, Message>> {
        // Step 16 implementation.
        unimplemented!("CompositeTransport::request_from lands in Wave 3 Step 16")
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
    use futures::stream::BoxStream;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

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
}
