//! Gossip server — incoming connection listener with optional authorization.
//!
//! Accepts TCP connections, performs server-side SHS handshake, then runs
//! bidirectional replication. Authorization runs AFTER handshake succeeds
//! (identity is verified before the authorization check).
//!
//! Default: no authorization — accepts any peer on the same network key.
//! With AuthorizeFn: checks known_peers.authorized, rejecting unauthorized peers.
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

/// Authorization callback: given a remote peer's VerifyingKey, return whether to allow.
pub type AuthorizeFn = Arc<dyn Fn(&ed25519_dalek::VerifyingKey) -> bool + Send + Sync>;

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

/// Start the gossip server listener (no authorization — accepts all peers).
pub async fn run_server(
    bind_addr: String,
    network_key: [u8; 32],
    identity: Identity,
    engine: Arc<FeedEngine>,
) -> Result<()> {
    run_server_with_auth(bind_addr, network_key, identity, engine, None).await
}

/// Start the gossip server with optional authorization callback.
pub async fn run_server_with_auth(
    bind_addr: String,
    network_key: [u8; 32],
    identity: Identity,
    engine: Arc<FeedEngine>,
    authorize: Option<AuthorizeFn>,
) -> Result<()> {
    let config = ServerConfig {
        bind_addr,
        network_key,
        identity,
        push_enabled: false,
        max_persistent_connections: 32,
    };
    run_server_with_push(config, engine, None, authorize).await
}

/// Start the gossip server with push support.
pub async fn run_server_with_push(
    config: ServerConfig,
    engine: Arc<FeedEngine>,
    registry: Option<Arc<ConnectionRegistry>>,
    authorize: Option<AuthorizeFn>,
) -> Result<()> {
    let listener = TcpListener::bind(&config.bind_addr).await?;
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_CONNECTIONS));
    tracing::info!(
        addr = %config.bind_addr,
        max_conns = MAX_CONCURRENT_CONNECTIONS,
        push_enabled = config.push_enabled,
        "gossip server listening"
    );

    loop {
        let (stream, peer_addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "failed to accept connection");
                continue;
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
        let auth = authorize.clone();
        let push_enabled = config.push_enabled;
        let reg = registry.clone();

        tokio::spawn(async move {
            let _permit = permit; // held until task completes
            tracing::info!(peer = %peer_addr, "incoming gossip connection");
            let result = tokio::time::timeout(CONNECTION_TIMEOUT, async {
                let mut conn = SecureConnection::accept(stream, net_key, id).await?;
                let remote_pub_id = PublicId::from_verifying_key(&conn.remote_public_key);

                // Check authorization if callback provided
                if let Some(ref auth_fn) = auth {
                    if !auth_fn(&conn.remote_public_key) {
                        tracing::warn!(peer = %peer_addr, "unauthorized peer rejected");
                        let _ = conn.close().await;
                        return Ok(());
                    }
                }

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
