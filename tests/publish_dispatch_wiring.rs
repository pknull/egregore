//! Phase 2 Wave 4 Step 22 — `publish_dispatcher` wiring integration tests.
//!
//! These tests exercise the engine ↔ dispatcher contract end-to-end:
//!
//! 1. `single_transport_deployment_publishes_through_dispatcher` — every
//!    locally-authored publish reaches the attached transport through the
//!    dispatcher.
//!
//! 2. `dispatcher_mpsc_full_does_not_lose_publish` — saturate the mpsc
//!    with a stalled dispatcher; publish many messages; assert all of
//!    them are durably stored. This is the amendment §C.16 acceptance
//!    test that a dispatcher lag does NOT lose publishes.
//!
//! 3. `pushmanager_retirement_behavior_preserved` — gossip-only
//!    deployment: locally-authored messages still flow through the
//!    effective transport's `publish` path (which is the
//!    `GossipTransport::publish` → `registry.broadcast` chain). The test
//!    attaches a sentinel mock and asserts its `published` log
//!    records the dispatched message.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use parking_lot::Mutex;

use egregore::error::Result;
use egregore::feed::engine::{DispatchTicket, FeedEngine};
use egregore::feed::models::Message;
use egregore::feed::store::FeedStore;
use egregore::identity::{Identity, PublicId};
use egregore::transport::{SubscriptionHandle, TopicFilter, Transport, TransportHealth};

/// Minimal mock that reports a configurable `backend` string. The
/// `publish` path records every call; tests compare the count against the
/// number of dispatched tickets.
struct BackendMock {
    published: Mutex<Vec<Message>>,
    backend: &'static str,
    connected: AtomicBool,
    inflight: AtomicUsize,
    /// When true, `publish` sleeps before returning — used to stall the
    /// dispatcher so the engine-side mpsc fills.
    stall: AtomicBool,
}

impl BackendMock {
    fn new(backend: &'static str) -> Arc<Self> {
        Arc::new(Self {
            published: Mutex::new(Vec::new()),
            backend,
            connected: AtomicBool::new(true),
            inflight: AtomicUsize::new(0),
            stall: AtomicBool::new(false),
        })
    }

    fn set_stall(&self, yes: bool) {
        self.stall.store(yes, Ordering::Release);
    }

    fn published(&self) -> Vec<Message> {
        self.published.lock().clone()
    }
}

#[async_trait]
impl Transport for BackendMock {
    async fn publish(&self, msg: &Message) -> Result<()> {
        self.inflight.fetch_add(1, Ordering::AcqRel);
        if self.stall.load(Ordering::Acquire) {
            // Long stall so the dispatcher's mpsc receiver stays busy on
            // this call while the test queues further publishes.
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
        self.published.lock().push(msg.clone());
        self.inflight.fetch_sub(1, Ordering::AcqRel);
        Ok(())
    }

    async fn subscribe(
        &self,
        _filter: TopicFilter,
    ) -> Result<(SubscriptionHandle, BoxStream<'static, Message>)> {
        let (cancel_tx, _cancel_rx) = tokio::sync::oneshot::channel::<()>();
        let handle = SubscriptionHandle::from_cancel(cancel_tx);
        let stream = stream::empty::<Message>();
        Ok((handle, Box::pin(stream)))
    }

    async fn request_from(
        &self,
        _author: PublicId,
        _after_seq: u64,
    ) -> Result<BoxStream<'static, Message>> {
        Ok(Box::pin(stream::empty::<Message>()))
    }

    async fn start(&self) -> Result<()> {
        Ok(())
    }

    async fn shutdown(&self, _deadline: Duration) -> Result<()> {
        Ok(())
    }

    fn health(&self) -> TransportHealth {
        TransportHealth {
            connected: self.connected.load(Ordering::Acquire),
            backend: self.backend,
            last_successful_publish: None,
            last_peer_contact: None,
            unreplicated_count: 0,
            inflight_publishes: self.inflight.load(Ordering::Acquire),
            last_error: None,
            children: vec![],
        }
    }
}

/// Assemble a minimal engine + identity harness. Returns the engine wrapped
/// in `Arc` so tests can clone it across spawn boundaries.
fn setup() -> (Arc<FeedEngine>, Identity) {
    let store = FeedStore::open_memory().unwrap();
    let engine = Arc::new(FeedEngine::new(store));
    let identity = Identity::generate();
    (engine, identity)
}

/// Spawn a simple dispatcher loop bound to `transport`. The caller owns the
/// returned `JoinHandle` and can drop it to abort.
fn spawn_dispatcher(
    transport: Arc<dyn Transport>,
    mut rx: tokio::sync::mpsc::Receiver<DispatchTicket>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(ticket) = rx.recv().await {
            let _ = transport.publish(&ticket.message).await;
        }
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn single_transport_deployment_publishes_through_dispatcher() {
    let (engine, identity) = setup();
    let gossip = BackendMock::new("gossip");
    engine.attach_transport(gossip.clone() as Arc<dyn Transport>);

    let (dispatch_tx, dispatch_rx) = tokio::sync::mpsc::channel::<DispatchTicket>(64);
    engine.set_dispatch_sender(dispatch_tx);
    let _dispatcher = spawn_dispatcher(gossip.clone() as Arc<dyn Transport>, dispatch_rx);

    for n in 1..=5 {
        engine
            .publish(
                &identity,
                serde_json::json!({"type": "test", "n": n}),
                None,
                vec![],
            )
            .unwrap();
    }

    // Dispatcher processed all 5 tickets.
    tokio::time::timeout(Duration::from_secs(2), async {
        while gossip.published().len() < 5 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("dispatcher should have handled 5 publishes in < 2s");
    assert_eq!(gossip.published().len(), 5);
}

#[tokio::test(flavor = "multi_thread")]
async fn dispatcher_mpsc_full_does_not_lose_publish() {
    let (engine, identity) = setup();
    let gossip = BackendMock::new("gossip");
    engine.attach_transport(gossip.clone() as Arc<dyn Transport>);

    // Small mpsc capacity + stalled dispatcher = Full errors on try_send.
    // Amendment §C.16 acceptance: all publishes are durably stored even
    // under a saturated dispatcher.
    gossip.set_stall(true);
    let (dispatch_tx, dispatch_rx) = tokio::sync::mpsc::channel::<DispatchTicket>(2);
    engine.set_dispatch_sender(dispatch_tx);
    let _dispatcher = spawn_dispatcher(gossip.clone() as Arc<dyn Transport>, dispatch_rx);

    // Let the dispatcher pick up one ticket and enter its stall.
    engine
        .publish(
            &identity,
            serde_json::json!({"type": "test", "n": 0}),
            None,
            vec![],
        )
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Publish many more; with capacity=2 and a stalled consumer, most
    // will hit TrySendError::Full. All MUST still be stored.
    let total: u64 = 20;
    for n in 1..=total {
        engine
            .publish(
                &identity,
                serde_json::json!({"type": "test", "n": n}),
                None,
                vec![],
            )
            .expect("publish returns Ok even under saturation");
    }

    let count = engine.store().message_count().unwrap();
    assert_eq!(
        count,
        total + 1,
        "all publishes must be durably stored; got {count}, expected {}",
        total + 1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pushmanager_retirement_behavior_preserved() {
    // Gossip-only deployment. Without PushManager (Step 23), locally-
    // authored messages flow through the dispatcher → gossip.publish
    // chain. This test verifies the dispatcher receives and forwards
    // every publish so the semantic is identical to Phase 1's
    // broadcast-channel → registry.broadcast path.
    let (engine, identity) = setup();
    let gossip = BackendMock::new("gossip");
    engine.attach_transport(gossip.clone() as Arc<dyn Transport>);

    let (dispatch_tx, dispatch_rx) = tokio::sync::mpsc::channel::<DispatchTicket>(64);
    engine.set_dispatch_sender(dispatch_tx);
    let _dispatcher = spawn_dispatcher(gossip.clone() as Arc<dyn Transport>, dispatch_rx);

    let mut expected_hashes = Vec::new();
    for n in 1..=10 {
        let msg = engine
            .publish(
                &identity,
                serde_json::json!({"type": "test", "n": n}),
                None,
                vec![],
            )
            .unwrap();
        expected_hashes.push(msg.hash);
    }

    tokio::time::timeout(Duration::from_secs(3), async {
        while gossip.published().len() < 10 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("dispatcher should route 10 publishes in < 3s (PushManager retirement parity)");

    let published_hashes: Vec<String> = gossip.published().iter().map(|m| m.hash.clone()).collect();
    assert_eq!(published_hashes.len(), 10);
    for h in &expected_hashes {
        assert!(
            published_hashes.contains(h),
            "published hash list must contain {h}"
        );
    }
}
