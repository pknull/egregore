use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpStream;

use crate::feed::engine::FeedEngine;
use crate::gossip::connection::SecureConnection;
use crate::gossip::replication;
use crate::identity::Identity;

/// Periodically sync with known peers.
pub async fn run_sync_loop(
    peers: Vec<String>,
    network_key: [u8; 32],
    identity: Identity,
    engine: Arc<FeedEngine>,
    interval: Duration,
) {
    if peers.is_empty() {
        tracing::info!("no peers configured, gossip sync disabled");
        return;
    }

    tracing::info!(
        peer_count = peers.len(),
        interval_secs = interval.as_secs(),
        "gossip sync loop started"
    );

    loop {
        for peer_addr in &peers {
            tracing::debug!(peer = %peer_addr, "initiating gossip sync");
            match sync_with_peer(peer_addr, network_key, identity.clone(), &engine).await {
                Ok(()) => tracing::info!(peer = %peer_addr, "gossip sync complete"),
                Err(e) => tracing::warn!(peer = %peer_addr, error = %e, "gossip sync failed"),
            }
        }
        tokio::time::sleep(interval).await;
    }
}

async fn sync_with_peer(
    addr: &str,
    network_key: [u8; 32],
    identity: Identity,
    engine: &Arc<FeedEngine>,
) -> crate::error::Result<()> {
    let stream = TcpStream::connect(addr).await?;
    let mut conn = SecureConnection::connect(stream, network_key, identity).await?;
    replication::replicate_as_client(&mut conn, engine).await?;
    conn.close().await?;
    Ok(())
}
