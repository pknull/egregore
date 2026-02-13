use std::sync::Arc;

use tokio::net::TcpListener;

use crate::error::Result;
use crate::feed::engine::FeedEngine;
use crate::gossip::connection::SecureConnection;
use crate::gossip::replication;
use crate::identity::Identity;

/// Start the gossip server listener.
pub async fn run_server(
    bind_addr: String,
    network_key: [u8; 32],
    identity: Identity,
    engine: Arc<FeedEngine>,
) -> Result<()> {
    let listener = TcpListener::bind(&bind_addr).await?;
    tracing::info!(addr = %bind_addr, "gossip server listening");

    loop {
        let (stream, peer_addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "failed to accept connection");
                continue;
            }
        };

        let net_key = network_key;
        let id = identity.clone();
        let eng = engine.clone();

        tokio::spawn(async move {
            tracing::info!(peer = %peer_addr, "incoming gossip connection");
            match SecureConnection::accept(stream, net_key, id).await {
                Ok(mut conn) => {
                    if let Err(e) = replication::replicate_as_server(&mut conn, &eng).await {
                        tracing::warn!(peer = %peer_addr, error = %e, "replication error");
                    }
                    let _ = conn.close().await;
                    tracing::info!(peer = %peer_addr, "gossip connection closed");
                }
                Err(e) => {
                    tracing::warn!(peer = %peer_addr, error = %e, "handshake failed");
                }
            }
        });
    }
}
