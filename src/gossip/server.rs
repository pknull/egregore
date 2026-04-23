//! Gossip server — incoming connection listener.
//!
//! Accepts TCP connections, performs server-side SHS handshake, then runs
//! bidirectional replication. The network key serves as the trust boundary —
//! any peer with the matching key can connect.
//!
//! With push_enabled: After replication, responds to Subscribe requests and
//! establishes persistent connections for push-based message broadcasting.
//!
//! Bounded by semaphore (64 concurrent) and per-connection timeout (120s)
//! to prevent resource exhaustion.

use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::error::Result;
use crate::feed::engine::FeedEngine;
use crate::gossip::client::build_replication_config;
use crate::gossip::connection::SecureConnection;
use crate::gossip::persistent::PersistentConnectionTask;
use crate::gossip::registry::{ConnectionHandle, ConnectionRegistry};
use crate::gossip::replication;
use crate::identity::{Identity, PublicId};

const CONNECTION_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_CONCURRENT_CONNECTIONS: usize = 64;

/// Configuration for the gossip server.
#[derive(Clone)]
pub struct ServerConfig {
    /// Address to bind to.
    pub bind_addr: String,
    /// Network key for SHS.
    pub network_key: [u8; 32],
    /// Local identity.
    pub identity: Identity,
    /// Enable persistent push connections.
    pub push_enabled: bool,
    /// Maximum persistent connections (separate from replication semaphore).
    pub max_persistent_connections: usize,
}

/// Start the gossip server listener.
pub async fn run_server(
    bind_addr: String,
    network_key: [u8; 32],
    identity: Identity,
    engine: Arc<FeedEngine>,
) -> Result<()> {
    let config = ServerConfig {
        bind_addr,
        network_key,
        identity,
        push_enabled: false,
        max_persistent_connections: 32,
    };
    run_server_with_push(config, engine, None).await
}

/// Start the gossip server with push support.
///
/// Runs until the underlying `TcpListener` errors fatally — in practice, until
/// the process is torn down. For a cancellable variant that can be stopped
/// cooperatively (used by `transport::gossip::GossipTransport::shutdown`), see
/// [`run_server_with_push_cancellable`]. This wrapper delegates to the
/// cancellable form with a token that never fires, so `main.rs` does not need
/// to construct or thread a `CancellationToken` through for single-transport
/// boot.
pub async fn run_server_with_push(
    config: ServerConfig,
    engine: Arc<FeedEngine>,
    registry: Option<Arc<ConnectionRegistry>>,
) -> Result<()> {
    // Never-firing token keeps the loop shape identical to the historical
    // behavior: `ctrl_c` + `registry.close_all()` tears the process down.
    run_server_with_push_cancellable(config, engine, registry, CancellationToken::new()).await
}

/// Start the gossip server with push support and cooperative cancellation.
///
/// When `cancel` is triggered, the accept loop returns `Ok(())` at the next
/// `select!` decision point. Accepted connections already handed off to
/// per-connection tasks are *not* forcibly aborted here — shutdown drain of
/// those is driven by `ConnectionRegistry::close_all()` at the caller (see
/// `GossipTransport::shutdown`).
///
/// This is the plumbing added in Phase 1 Step 3 to satisfy `Transport::shutdown`
/// (plan §11 OQ-5).
pub async fn run_server_with_push_cancellable(
    config: ServerConfig,
    engine: Arc<FeedEngine>,
    registry: Option<Arc<ConnectionRegistry>>,
    cancel: CancellationToken,
) -> Result<()> {
    run_server_with_push_cancellable_ready(config, engine, registry, cancel, None).await
}

/// Variant of `run_server_with_push_cancellable` that signals bind completion
/// via a `oneshot::Sender<std::io::Result<()>>`. `GossipTransport::start` uses
/// this to surface TCP bind failures synchronously instead of silently in a
/// detached background task.
///
/// Signal semantics:
/// - On successful bind, the sender is fired with `Ok(())` before the accept
///   loop starts.
/// - On bind failure, the sender is fired with the underlying `io::Error`
///   clone (via `to_string`) wrapped in `Err(io::Error)` and the function
///   returns the original error.
/// - If the caller does not care about the ready signal, pass `None`.
pub async fn run_server_with_push_cancellable_ready(
    config: ServerConfig,
    engine: Arc<FeedEngine>,
    registry: Option<Arc<ConnectionRegistry>>,
    cancel: CancellationToken,
    ready: Option<tokio::sync::oneshot::Sender<std::io::Result<()>>>,
) -> Result<()> {
    let listener = match TcpListener::bind(&config.bind_addr).await {
        Ok(l) => {
            if let Some(tx) = ready {
                // Receiver may have dropped (caller timed out waiting); that
                // is not a server error — proceed with the accept loop.
                let _ = tx.send(Ok(()));
            }
            l
        }
        Err(e) => {
            if let Some(tx) = ready {
                // Clone error kind + message into a fresh io::Error so the
                // original can be returned from this fn.
                let _ = tx.send(Err(std::io::Error::new(e.kind(), e.to_string())));
            }
            return Err(e.into());
        }
    };
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_CONNECTIONS));
    tracing::info!(
        addr = %config.bind_addr,
        max_conns = MAX_CONCURRENT_CONNECTIONS,
        push_enabled = config.push_enabled,
        "gossip server listening"
    );

    loop {
        let (stream, peer_addr) = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                tracing::info!(addr = %config.bind_addr, "gossip server cancelled; accept loop exiting");
                return Ok(());
            }
            accept_result = listener.accept() => match accept_result {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to accept connection");
                    continue;
                }
            }
        };

        let permit = match semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                tracing::warn!(peer = %peer_addr, "connection limit reached, rejecting");
                drop(stream);
                continue;
            }
        };

        let net_key = config.network_key;
        let id = config.identity.clone();
        let eng = engine.clone();
        let push_enabled = config.push_enabled;
        let reg = registry.clone();

        tokio::spawn(async move {
            let _permit = permit; // held until task completes
            tracing::info!(peer = %peer_addr, "incoming gossip connection");
            let result = tokio::time::timeout(CONNECTION_TIMEOUT, async {
                let mut conn = SecureConnection::accept(stream, net_key, id).await?;
                let remote_pub_id = PublicId::from_verifying_key(&conn.remote_public_key);

                let repl_config = build_replication_config(&eng).await;
                if let Err(e) =
                    replication::replicate_as_server(&mut conn, &eng, &repl_config).await
                {
                    tracing::warn!(peer = %peer_addr, error = %e, "replication error");
                    let _ = conn.close().await;
                    return Ok(());
                }

                // Check for persistent mode negotiation
                if push_enabled {
                    if let Some(ref registry) = reg {
                        // Check if already connected
                        let already_connected = registry.is_connected(&remote_pub_id);

                        if let Ok(accepted) = replication::negotiate_persistent_mode_server(
                            &mut conn,
                            push_enabled,
                            already_connected,
                        )
                        .await
                        {
                            if accepted {
                                // Split connection for concurrent I/O
                                let socket_addr = conn.peer_addr().unwrap_or(peer_addr);
                                let (reader, writer) = conn.into_split();

                                // Register the writer half
                                let handle = ConnectionHandle::new(
                                    writer,
                                    remote_pub_id.clone(),
                                    socket_addr,
                                    false, // incoming connection
                                );

                                if registry.register(handle) {
                                    // Spawn task to handle incoming Push messages
                                    let task = PersistentConnectionTask::new(
                                        reader,
                                        remote_pub_id.clone(),
                                        eng.clone(),
                                        registry.clone(),
                                    );
                                    tokio::spawn(async move {
                                        if let Err(e) = task.run().await {
                                            tracing::debug!(
                                                error = %e,
                                                "persistent connection task ended"
                                            );
                                        }
                                    });
                                    return Ok(());
                                }
                                // Registration failed, reader/writer dropped, conn closed
                                return Ok(());
                            }
                        }
                    }
                }

                // Fall back to closing connection
                let _ = conn.close().await;
                Ok::<(), crate::error::EgreError>(())
            })
            .await;

            match result {
                Ok(Ok(())) => tracing::info!(peer = %peer_addr, "gossip connection closed"),
                Ok(Err(e)) => tracing::warn!(peer = %peer_addr, error = %e, "handshake failed"),
                Err(_) => tracing::warn!(peer = %peer_addr, "gossip connection timed out"),
            }
        });
    }
}
