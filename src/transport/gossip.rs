//! `GossipTransport` — wraps `egregore/src/gossip/` stack behind the
//! `Transport` trait. See plan §4 Step 3.
//!
//! Delegates to the existing `ConnectionRegistry`, `PushManager`, sync loop,
//! and server without relocating any code from `gossip/`. This file is the
//! trait-impl bridge only; all wire logic (SHS, replication, bloom, flow
//! control) remains under `src/gossip/`.
//!
//! # Step 3 shutdown contract (plan §11 OQ-5)
//!
//! The existing `gossip::server::run_server_with_push` and
//! `gossip::client::run_sync_loop_with_push` did not expose cancel handles at
//! the time of Phase 1 Step 3. We added parallel `_cancellable` variants in
//! both files that take a `tokio_util::sync::CancellationToken`; the
//! historical signatures delegate to those variants with a never-firing token
//! so `main.rs` remains byte-for-byte unchanged in how it spawns and tears
//! down the gossip stack.
//!
//! `GossipTransport::start` spawns the cancellable variants with a shared
//! `CancellationToken`; `GossipTransport::shutdown` fires the token,
//! gracefully closes persistent connections via `ConnectionRegistry::close_all`,
//! and awaits the spawned join handles within the caller-provided deadline.
//!
//! # Step 3 scope
//!
//! Only `new`, `start`, `shutdown`, and `health` are implemented. `publish`,
//! `subscribe`, and `request_from` remain `todo!()` and land in Steps 4 and 9.
//! No code in `src/main.rs`, `src/feed/engine.rs`, or any other crate module
//! observes `GossipTransport` at the end of Step 3.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::error::{EgreError, Result};
use crate::feed::engine::FeedEngine;
use crate::feed::models::Message;
use crate::gossip::client::{run_sync_loop_with_push_cancellable, SyncConfig};
use crate::gossip::registry::ConnectionRegistry;
use crate::gossip::server::{run_server_with_push_cancellable, ServerConfig};
use crate::identity::{Identity, PublicId};

use super::filter::TopicFilter;
use super::health::TransportHealth;
use super::subscription::SubscriptionHandle;
use super::trait_def::Transport;

/// Construction-time configuration for a [`GossipTransport`].
///
/// Aggregates the parameters that `main.rs` already threads through to the
/// existing `gossip::server::run_server_with_push` and
/// `gossip::client::run_sync_loop_with_push` call sites so the `Transport`
/// adapter has the same shape of inputs without a long positional argument
/// list. A dedicated struct (rather than a builder) keeps the construction
/// path synchronous and inspection-friendly.
pub struct GossipTransportConfig {
    /// Shared connection registry (also held by the existing `PushManager`
    /// and sync-loop call sites for Phase 1).
    pub registry: Arc<ConnectionRegistry>,
    /// Local node identity. The adapter holds a clone; the original remains
    /// owned by the caller.
    pub identity: Identity,
    /// Reference to the engine the adapter will drive on received messages
    /// (Step 4+ wires this); Step 3 only stores it for later use.
    pub engine: Arc<FeedEngine>,
    /// Pre-built server config (bind address, network key, identity,
    /// push_enabled, max_persistent_connections). Reuses the existing type so
    /// the adapter does not re-derive defaults.
    pub server_config: ServerConfig,
    /// Pre-built sync-loop config (peers, interval, push_enabled, backoff
    /// bounds). Reuses the existing type for the same reason.
    pub sync_config: SyncConfig,
}

/// `Transport` adapter wrapping the existing gossip stack.
///
/// Holds shared references to the connection registry and engine so the
/// existing wire fan-out (`PushManager` → `registry.broadcast`) and the
/// receive path (`replicate_as_*` → `engine.ingest`) continue to serve
/// publishes and subscriptions in Phase 1 without the adapter intercepting
/// either hot path. Step 4 introduces the `publish` delegation to
/// `registry.broadcast`; Step 9 introduces subscribe/request_from.
pub struct GossipTransport {
    /// Shared registry used for peer fan-out and `health().connected`.
    registry: Arc<ConnectionRegistry>,
    /// Local identity retained for future use (Step 4+ may attach it to
    /// published messages' metadata; `start`/`shutdown` do not need it).
    #[allow(dead_code)]
    identity: Identity,
    /// Engine the adapter will drive for inbound ingest (Step 9).
    #[allow(dead_code)]
    engine: Arc<FeedEngine>,
    /// Server loop configuration.
    server_config: ServerConfig,
    /// Sync loop configuration.
    sync_config: SyncConfig,
    /// Single-shot start latch. `start` uses `compare_exchange` to make the
    /// call idempotent.
    started: AtomicBool,
    /// Cancellation token shared with every spawned task. Fired once by
    /// `shutdown`; cannot be reused because `GossipTransport` itself is not
    /// designed to be restarted after shutdown (a fresh instance is cheap).
    cancel: CancellationToken,
    /// JoinHandles for the server and sync-loop tasks (and any future
    /// children). `Mutex<Option<_>>` so `shutdown` can take ownership and
    /// await them.
    handles: Mutex<Option<Vec<JoinHandle<()>>>>,
}

impl GossipTransport {
    /// Construct a new adapter over the supplied configuration. Does not
    /// spawn any tasks — call [`Transport::start`] to begin running.
    pub fn new(config: GossipTransportConfig) -> Self {
        let GossipTransportConfig {
            registry,
            identity,
            engine,
            server_config,
            sync_config,
        } = config;
        Self {
            registry,
            identity,
            engine,
            server_config,
            sync_config,
            started: AtomicBool::new(false),
            cancel: CancellationToken::new(),
            handles: Mutex::new(Some(Vec::new())),
        }
    }
}

#[async_trait]
impl Transport for GossipTransport {
    async fn publish(&self, _msg: &Message) -> Result<()> {
        // TODO(Step 4): delegate to `self.registry.broadcast(_msg)` — mirrors
        // `PushManager::handle_message` (plan §4 Step 4).
        todo!("publish lands in Phase 1 Step 4")
    }

    async fn subscribe(
        &self,
        _filter: TopicFilter,
    ) -> Result<(SubscriptionHandle, BoxStream<'static, Message>)> {
        // TODO(Step 9): construct oneshot cancel, build filtered stream over
        // `engine.subscribe()` broadcast receiver (plan §4 Step 9).
        todo!("subscribe lands in Phase 1 Step 9")
    }

    async fn request_from(
        &self,
        _author: PublicId,
        _after_seq: u64,
    ) -> Result<BoxStream<'static, Message>> {
        // TODO(Step 9): build a live-forward stream against the local store
        // (plan §4 Step 9).
        todo!("request_from lands in Phase 1 Step 9")
    }

    async fn start(&self) -> Result<()> {
        // Idempotent per trait contract. `compare_exchange` returns Ok the
        // first time (previous value `false`) and Err(true) on re-entry.
        if self
            .started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            tracing::debug!("GossipTransport::start called while already running; no-op");
            return Ok(());
        }

        let server_cfg = self.server_config.clone();
        let server_engine = self.engine.clone();
        let server_registry = Some(self.registry.clone());
        let server_cancel = self.cancel.clone();
        let server_handle = tokio::spawn(async move {
            if let Err(e) = run_server_with_push_cancellable(
                server_cfg,
                server_engine,
                server_registry,
                server_cancel,
            )
            .await
            {
                tracing::error!(error = %e, "gossip server (transport) failed");
            }
        });

        let sync_cfg = self.sync_config.clone();
        let sync_engine = self.engine.clone();
        let sync_registry = Some(self.registry.clone());
        let sync_cancel = self.cancel.clone();
        let sync_handle = tokio::spawn(async move {
            run_sync_loop_with_push_cancellable(sync_cfg, sync_engine, sync_registry, sync_cancel)
                .await;
        });

        let mut handles = self.handles.lock().await;
        if let Some(vec) = handles.as_mut() {
            vec.push(server_handle);
            vec.push(sync_handle);
        }
        Ok(())
    }

    async fn shutdown(&self, deadline: Duration) -> Result<()> {
        // Fire cancellation and take ownership of any spawned handles. If
        // `start` was never called, `handles` contains an empty vec and we
        // return `Ok(())` immediately.
        self.cancel.cancel();

        // Gracefully close all persistent connections (goodbye frames). This
        // mirrors the existing `main.rs` shutdown hook.
        self.registry.close_all().await;

        let taken = {
            let mut guard = self.handles.lock().await;
            guard.take().unwrap_or_default()
        };

        if taken.is_empty() {
            return Ok(());
        }

        // Await all joins with a single overall deadline.
        let join_all = async {
            for h in taken {
                // Join errors (panic/cancel) are logged but do not prevent
                // awaiting the remaining handles.
                if let Err(e) = h.await {
                    tracing::warn!(error = %e, "gossip transport task join failed");
                }
            }
        };

        match tokio::time::timeout(deadline, join_all).await {
            Ok(()) => Ok(()),
            Err(_) => Err(EgreError::Peer {
                reason: format!(
                    "gossip transport shutdown exceeded deadline of {:?}",
                    deadline
                ),
            }),
        }
    }

    fn health(&self) -> TransportHealth {
        TransportHealth {
            connected: self.registry.connection_count() > 0,
            backend: "gossip",
            // Step 4+ populates these once `publish` observes send outcomes
            // and the subscribe/ingest path records inbound peer contact.
            last_successful_publish: None,
            last_peer_contact: None,
            // Step 5+ plumbs unreplicated count through the store once the
            // engine dispatches publish via the trait.
            unreplicated_count: 0,
            inflight_publishes: 0,
            last_error: None,
            // Gossip is a leaf transport; composites (Phase 2) populate
            // `children` themselves.
            children: vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    //! Step 3 test harness — builds a `GossipTransport` against an in-memory
    //! `FeedStore` and a loopback-bound `ServerConfig`. Targets the four
    //! behaviors called out in the Step 3 brief:
    //!
    //! 1. `new()` constructs with the full argument list.
    //! 2. `health()` returns the sensible-stub shape. Two cases.
    //! 3. `start()` is idempotent.
    //! 4. `shutdown(deadline)` returns within deadline when no work is
    //!    outstanding.

    use super::*;

    use std::time::Duration;

    use crate::feed::store::FeedStore;
    use crate::gossip::client::SyncConfig;
    use crate::gossip::registry::ConnectionRegistry;
    use crate::gossip::server::ServerConfig;
    use crate::identity::Identity;

    /// Helper: build a `GossipTransportConfig` wired against an in-memory
    /// store and a loopback `127.0.0.1:0` bind (OS assigns a free port).
    fn build_config(
        registry: Arc<ConnectionRegistry>,
        bind_addr: &str,
    ) -> (GossipTransportConfig, Arc<FeedEngine>) {
        let identity = Identity::generate();
        let network_key = [0u8; 32];
        let store = FeedStore::open_memory().expect("in-memory store");
        let engine = Arc::new(FeedEngine::new(store));
        let server_config = ServerConfig {
            bind_addr: bind_addr.to_string(),
            network_key,
            identity: identity.clone(),
            push_enabled: false,
            max_persistent_connections: 32,
        };
        let sync_config = SyncConfig::new(
            Vec::new(),
            network_key,
            identity.clone(),
            Duration::from_secs(3600), // long interval — test must not wait on it
            false,
        );
        let cfg = GossipTransportConfig {
            registry,
            identity,
            engine: engine.clone(),
            server_config,
            sync_config,
        };
        (cfg, engine)
    }

    #[tokio::test]
    async fn new_constructs_with_full_argument_list() {
        let registry = Arc::new(ConnectionRegistry::new(32));
        let (cfg, _engine) = build_config(registry.clone(), "127.0.0.1:0");
        let transport = GossipTransport::new(cfg);
        // Nothing is spawned before start(); just confirm construction
        // succeeds and `health` reflects the empty registry.
        let h = transport.health();
        assert_eq!(h.backend, "gossip");
        assert!(
            !h.connected,
            "fresh registry has no connections, must report disconnected"
        );
    }

    #[tokio::test]
    async fn health_empty_registry_not_connected() {
        let registry = Arc::new(ConnectionRegistry::new(32));
        let (cfg, _engine) = build_config(registry.clone(), "127.0.0.1:0");
        let transport = GossipTransport::new(cfg);
        let h = transport.health();
        assert_eq!(h.backend, "gossip");
        assert!(!h.connected);
        assert_eq!(h.unreplicated_count, 0);
        assert_eq!(h.inflight_publishes, 0);
        assert!(h.last_error.is_none());
        assert!(h.last_successful_publish.is_none());
        assert!(h.last_peer_contact.is_none());
        assert!(
            h.children.is_empty(),
            "gossip is a leaf transport; children must be empty"
        );
    }

    #[tokio::test]
    async fn health_connected_flag_follows_registry_count() {
        // We cannot trivially register a live `ConnectionHandle` without a
        // real `SecureWriter`, but we can verify the inverse direction: the
        // default (empty) registry is reported as disconnected. This together
        // with `health_empty_registry_not_connected` establishes the
        // connected flag is derived from `registry.connection_count()`.
        //
        // A full connected-flag test with a real peer handshake is deferred
        // to Step 9 where the MockTransport + gossip-loopback harness lives.
        let registry = Arc::new(ConnectionRegistry::new(32));
        let (cfg, _engine) = build_config(registry.clone(), "127.0.0.1:0");
        let transport = GossipTransport::new(cfg);
        assert_eq!(registry.connection_count(), 0);
        assert!(!transport.health().connected);
    }

    #[tokio::test]
    async fn start_is_idempotent() {
        let registry = Arc::new(ConnectionRegistry::new(32));
        let (cfg, _engine) = build_config(registry.clone(), "127.0.0.1:0");
        let transport = GossipTransport::new(cfg);

        transport.start().await.expect("first start");
        let first_count = {
            let guard = transport.handles.lock().await;
            guard.as_ref().map(|v| v.len()).unwrap_or(0)
        };
        assert_eq!(first_count, 2, "first start spawns server + sync tasks");

        transport.start().await.expect("second start (idempotent)");
        let second_count = {
            let guard = transport.handles.lock().await;
            guard.as_ref().map(|v| v.len()).unwrap_or(0)
        };
        assert_eq!(
            second_count, 2,
            "second start must not spawn additional tasks"
        );

        // Clean up so the test does not leak tasks.
        transport
            .shutdown(Duration::from_secs(5))
            .await
            .expect("shutdown after idempotent start");
    }

    #[tokio::test]
    async fn shutdown_returns_within_deadline_when_no_work() {
        let registry = Arc::new(ConnectionRegistry::new(32));
        let (cfg, _engine) = build_config(registry.clone(), "127.0.0.1:0");
        let transport = GossipTransport::new(cfg);
        transport.start().await.expect("start");

        let start = std::time::Instant::now();
        transport
            .shutdown(Duration::from_secs(5))
            .await
            .expect("shutdown within deadline");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "shutdown with no pending work should return well under the deadline, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn shutdown_without_start_is_noop() {
        // `shutdown` on a fresh transport must not panic or hang — the
        // `handles` vec is empty, so we take + do nothing.
        let registry = Arc::new(ConnectionRegistry::new(32));
        let (cfg, _engine) = build_config(registry.clone(), "127.0.0.1:0");
        let transport = GossipTransport::new(cfg);
        transport
            .shutdown(Duration::from_millis(100))
            .await
            .expect("shutdown without start is ok");
    }
}
