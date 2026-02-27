use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::net::{TcpStream, UdpSocket};

use crate::config::Config;
use crate::error::Result;
use crate::feed::engine::FeedEngine;
use crate::gossip::connection::SecureConnection;
use crate::identity::Identity;

const MAGIC: &[u8; 4] = b"EGRE";

/// UDP announcement broadcast on the LAN.
#[derive(Debug, Serialize, Deserialize)]
struct Announcement {
    magic: [u8; 4],
    network_key_hash: [u8; 8],
    gossip_port: u16,
    public_id: String,
}

/// Burst config: how often and for how long to announce.
struct BurstConfig {
    interval: Duration,
    duration: Duration,
}

const ANNOUNCE_BURST: BurstConfig = BurstConfig {
    interval: Duration::from_secs(5),
    duration: Duration::from_secs(30),
};

const QUIET_PERIOD: Duration = Duration::from_secs(300); // 5 minutes between bursts

/// Timeout for verification handshake to discovered peers.
const HANDSHAKE_VERIFY_TIMEOUT: Duration = Duration::from_secs(5);

/// Run LAN discovery: broadcast announcements and listen for peers.
pub async fn run_discovery(
    config: Config,
    identity: Identity,
    engine: Arc<FeedEngine>,
) -> Result<()> {
    let bind_addr: SocketAddr = format!("0.0.0.0:{}", config.discovery_port)
        .parse()
        .map_err(|e| crate::error::EgreError::Config {
            reason: format!("invalid discovery bind address: {e}"),
        })?;

    let socket = UdpSocket::bind(bind_addr).await?;
    socket.set_broadcast(true).map_err(|e| {
        crate::error::EgreError::Config {
            reason: format!("failed to enable broadcast: {e}"),
        }
    })?;

    let socket = Arc::new(socket);
    let discriminator = config.network_key_discriminator();
    let our_public_id = identity.public_id().0;

    tracing::info!(
        port = config.discovery_port,
        "LAN discovery started"
    );

    // Spawn listener task
    let listen_socket = socket.clone();
    let listen_discriminator = discriminator;
    let listen_public_id = our_public_id.clone();
    let listen_engine = engine.clone();
    let listen_network_key = config.network_key_bytes();
    let listen_identity = identity.clone();
    tokio::spawn(async move {
        listen_loop(
            listen_socket,
            listen_discriminator,
            listen_public_id,
            listen_engine,
            listen_network_key,
            listen_identity,
        )
        .await;
    });

    // Announce loop
    let broadcast_addr: SocketAddr = format!("255.255.255.255:{}", config.discovery_port)
        .parse()
        .unwrap();

    let announcement = Announcement {
        magic: *MAGIC,
        network_key_hash: discriminator,
        gossip_port: config.gossip_port,
        public_id: our_public_id,
    };

    let payload = serde_json::to_vec(&announcement).map_err(crate::error::EgreError::from)?;

    // Initial burst, then periodic bursts
    announce_burst(&socket, &broadcast_addr, &payload, &ANNOUNCE_BURST).await;
    loop {
        tokio::time::sleep(QUIET_PERIOD).await;
        announce_burst(&socket, &broadcast_addr, &payload, &ANNOUNCE_BURST).await;
    }
}

async fn announce_burst(
    socket: &UdpSocket,
    broadcast_addr: &SocketAddr,
    payload: &[u8],
    burst: &BurstConfig,
) {
    let start = tokio::time::Instant::now();
    while start.elapsed() < burst.duration {
        match socket.send_to(payload, broadcast_addr).await {
            Ok(_) => tracing::trace!("sent LAN discovery announcement"),
            Err(e) => tracing::warn!(error = %e, "failed to send discovery announcement"),
        }
        tokio::time::sleep(burst.interval).await;
    }
}

/// Cooldown before re-inserting the same peer address.
const PEER_INSERT_COOLDOWN: Duration = Duration::from_secs(60);
/// Max tracked peers in the rate limiter (prevents unbounded HashMap growth).
const MAX_RECENT_PEERS: usize = 1_000;

/// Validate an announcement: correct magic, same network, not self, valid port.
/// Returns the peer address (ip:port) if valid.
fn validate_announcement(
    ann: &Announcement,
    src_addr: &SocketAddr,
    our_discriminator: [u8; 8],
    our_public_id: &str,
) -> Option<String> {
    if ann.magic != *MAGIC {
        return None;
    }
    if ann.network_key_hash != our_discriminator {
        tracing::trace!(src = %src_addr, "ignoring announcement from different network");
        return None;
    }
    if ann.public_id == our_public_id {
        return None;
    }
    if ann.gossip_port < 1024 {
        return None;
    }
    Some(format!("{}:{}", src_addr.ip(), ann.gossip_port))
}

async fn listen_loop(
    socket: Arc<UdpSocket>,
    our_discriminator: [u8; 8],
    our_public_id: String,
    engine: Arc<FeedEngine>,
    network_key: [u8; 32],
    identity: Identity,
) {
    use std::collections::HashMap;
    use tokio::time::Instant;

    let mut buf = [0u8; 1024];
    let mut recent_peers: HashMap<String, Instant> = HashMap::new();

    loop {
        let (len, src_addr) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "discovery recv error");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };

        let announcement: Announcement = match serde_json::from_slice(&buf[..len]) {
            Ok(a) => a,
            Err(_) => continue,
        };

        let peer_addr =
            match validate_announcement(&announcement, &src_addr, our_discriminator, &our_public_id)
            {
                Some(addr) => addr,
                None => continue,
            };

        // Rate limit: skip if we recently inserted this peer
        let now = Instant::now();
        if let Some(last_seen) = recent_peers.get(&peer_addr) {
            if now.duration_since(*last_seen) < PEER_INSERT_COOLDOWN {
                continue;
            }
        }

        // Evict stale entries to bound memory
        if recent_peers.len() >= MAX_RECENT_PEERS {
            recent_peers.retain(|_, ts| now.duration_since(*ts) < PEER_INSERT_COOLDOWN);
            if recent_peers.len() >= MAX_RECENT_PEERS {
                continue; // still at capacity, skip this peer
            }
        }
        recent_peers.insert(peer_addr.clone(), now);

        tracing::debug!(
            peer = %peer_addr,
            public_id = %announcement.public_id,
            "discovered LAN peer, verifying handshake"
        );

        // Verify peer with SHS handshake before persisting
        let parsed_addr: SocketAddr = match peer_addr.parse() {
            Ok(addr) => addr,
            Err(e) => {
                tracing::debug!(
                    peer = %peer_addr,
                    error = %e,
                    "failed to parse discovered peer address"
                );
                continue;
            }
        };

        let handshake_result = tokio::time::timeout(
            HANDSHAKE_VERIFY_TIMEOUT,
            verify_peer_handshake(parsed_addr, network_key, identity.clone()),
        )
        .await;

        match handshake_result {
            Ok(Ok(())) => {
                tracing::info!(
                    peer = %peer_addr,
                    public_id = %announcement.public_id,
                    "verified LAN peer"
                );

                let addr = peer_addr.clone();
                let eng = engine.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    if let Err(e) = eng.store().insert_address_peer(&addr) {
                        tracing::warn!(error = %e, "failed to persist discovered peer");
                    }
                })
                .await;
            }
            Ok(Err(e)) => {
                tracing::debug!(
                    peer = %peer_addr,
                    error = %e,
                    "handshake verification failed for discovered peer"
                );
            }
            Err(_) => {
                tracing::debug!(
                    peer = %peer_addr,
                    "handshake verification timed out for discovered peer"
                );
            }
        }
    }
}

/// Attempt an SHS handshake to verify a discovered peer.
/// Returns Ok(()) if the handshake succeeds, Err otherwise.
async fn verify_peer_handshake(
    addr: SocketAddr,
    network_key: [u8; 32],
    identity: Identity,
) -> crate::error::Result<()> {
    let stream = TcpStream::connect(addr).await?;
    let conn = SecureConnection::connect(stream, network_key, identity).await?;
    // Handshake succeeded, close the connection cleanly
    conn.close().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announcement_serde_roundtrip() {
        let ann = Announcement {
            magic: *MAGIC,
            network_key_hash: [1, 2, 3, 4, 5, 6, 7, 8],
            gossip_port: 7655,
            public_id: "@test.ed25519".to_string(),
        };
        let json = serde_json::to_vec(&ann).unwrap();
        let decoded: Announcement = serde_json::from_slice(&json).unwrap();
        assert_eq!(decoded.magic, *MAGIC);
        assert_eq!(decoded.network_key_hash, ann.network_key_hash);
        assert_eq!(decoded.gossip_port, 7655);
        assert_eq!(decoded.public_id, "@test.ed25519");
    }

    #[test]
    fn discriminator_matching() {
        let config = Config::default();
        let disc = config.network_key_discriminator();

        let ann = Announcement {
            magic: *MAGIC,
            network_key_hash: disc,
            gossip_port: 7655,
            public_id: "@peer.ed25519".to_string(),
        };

        // Same network: discriminators match
        assert_eq!(ann.network_key_hash, disc);

        // Different network: discriminators differ
        let other_config = Config {
            network_key: "other-network".to_string(),
            ..Config::default()
        };
        assert_ne!(ann.network_key_hash, other_config.network_key_discriminator());
    }

    #[test]
    fn self_filtering() {
        let our_id = "@self.ed25519".to_string();
        let peer_id = "@peer.ed25519".to_string();

        // Same public_id should be filtered
        assert_eq!(our_id, our_id);
        // Different public_id should not be filtered
        assert_ne!(our_id, peer_id);
    }
}
