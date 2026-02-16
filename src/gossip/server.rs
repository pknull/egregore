//! Gossip server — incoming connection listener with optional authorization.
//!
//! Accepts TCP connections, performs server-side SHS handshake, then runs
//! bidirectional replication. Authorization runs AFTER handshake succeeds
//! (identity is verified before the authorization check).
//!
//! Node mode: no authorization — accepts any peer on the same network key.
//! Relay mode: AuthorizeFn checks known_peers.authorized in the database,
//!   rejecting peers that haven't called POST /v1/register.
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
use crate::gossip::replication;
use crate::identity::Identity;

const CONNECTION_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_CONCURRENT_CONNECTIONS: usize = 64;

/// Authorization callback: given a remote peer's VerifyingKey, return whether to allow.
pub type AuthorizeFn = Arc<dyn Fn(&ed25519_dalek::VerifyingKey) -> bool + Send + Sync>;

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
    let listener = TcpListener::bind(&bind_addr).await?;
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_CONNECTIONS));
    tracing::info!(addr = %bind_addr, max_conns = MAX_CONCURRENT_CONNECTIONS, "gossip server listening");

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

        let net_key = network_key;
        let id = identity.clone();
        let eng = engine.clone();
        let auth = authorize.clone();

        tokio::spawn(async move {
            let _permit = permit; // held until task completes
            tracing::info!(peer = %peer_addr, "incoming gossip connection");
            let result = tokio::time::timeout(CONNECTION_TIMEOUT, async {
                let mut conn = SecureConnection::accept(stream, net_key, id).await?;

                // Check authorization if callback provided
                if let Some(ref auth_fn) = auth {
                    if !auth_fn(&conn.remote_public_key) {
                        tracing::warn!(peer = %peer_addr, "unauthorized peer rejected");
                        let _ = conn.close().await;
                        return Ok(());
                    }
                }

                let config = build_replication_config(&eng).await;
                if let Err(e) =
                    replication::replicate_as_server(&mut conn, &eng, &config).await
                {
                    tracing::warn!(peer = %peer_addr, error = %e, "replication error");
                }
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
