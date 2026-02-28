//! mDNS/Bonjour peer discovery using DNS-SD.
//!
//! Alternative to UDP broadcast discovery that works across tailnets
//! and other networks where broadcast doesn't propagate.
//!
//! Advertises the egregore service with TXT records containing:
//! - `id`: Public identity (truncated for display)
//! - `port`: Gossip TCP port
//! - `net`: Network key discriminator (8-byte hash prefix, hex-encoded)

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::net::TcpStream;
use tokio::time::Instant;

use crate::config::Config;
use crate::error::Result;
use crate::feed::engine::FeedEngine;
use crate::gossip::connection::SecureConnection;
use crate::identity::Identity;

/// Timeout for verification handshake to discovered peers.
const HANDSHAKE_VERIFY_TIMEOUT: Duration = Duration::from_secs(5);

/// Cooldown before re-verifying the same peer address.
const PEER_VERIFY_COOLDOWN: Duration = Duration::from_secs(60);

/// Max tracked peers in the rate limiter.
const MAX_RECENT_PEERS: usize = 1_000;

/// Run mDNS peer discovery: advertise our service and browse for peers.
pub async fn run_mdns_discovery(
    config: Config,
    identity: Identity,
    engine: Arc<FeedEngine>,
) -> Result<()> {
    let mdns = ServiceDaemon::new().map_err(|e| crate::error::EgreError::Config {
        reason: format!("failed to create mDNS daemon: {e}"),
    })?;

    let service_type = &config.mdns_service;
    let discriminator = config.network_key_discriminator();
    let discriminator_hex = hex::encode(discriminator);
    let our_public_id = identity.public_id().0.clone();
    let our_public_id_short = truncate_id(&our_public_id, 12);

    // Create service info for advertising
    let instance_name = format!("egregore-{}", &our_public_id_short);
    let host_name = format!("{}.local.", hostname::get()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "egregore".to_string()));

    let mut properties = HashMap::new();
    properties.insert("id".to_string(), our_public_id_short.clone());
    properties.insert("port".to_string(), config.gossip_port.to_string());
    properties.insert("net".to_string(), discriminator_hex.clone());

    let service_info = ServiceInfo::new(
        service_type,
        &instance_name,
        &host_name,
        "",  // Let mdns-sd determine our IP
        config.gossip_port,
        properties,
    ).map_err(|e| crate::error::EgreError::Config {
        reason: format!("failed to create mDNS service info: {e}"),
    })?;

    // Register our service
    mdns.register(service_info).map_err(|e| crate::error::EgreError::Config {
        reason: format!("failed to register mDNS service: {e}"),
    })?;

    tracing::info!(
        service_type = %service_type,
        instance = %instance_name,
        gossip_port = config.gossip_port,
        "mDNS service registered"
    );

    // Browse for peers
    let receiver = mdns.browse(service_type).map_err(|e| crate::error::EgreError::Config {
        reason: format!("failed to browse mDNS services: {e}"),
    })?;

    // Process discovery events
    let network_key = config.network_key_bytes();
    process_mdns_events(
        receiver,
        discriminator_hex,
        our_public_id,
        engine,
        network_key,
        identity,
    ).await;

    // Keep the daemon alive (it runs in background threads)
    // This function should not return under normal operation
    std::future::pending::<()>().await;

    Ok(())
}

/// Truncate a public ID for display/instance naming.
fn truncate_id(id: &str, max_len: usize) -> String {
    // Remove @ prefix and .ed25519 suffix, then truncate
    let clean = id
        .trim_start_matches('@')
        .trim_end_matches(".ed25519");
    if clean.len() <= max_len {
        clean.to_string()
    } else {
        clean[..max_len].to_string()
    }
}

/// Process mDNS service discovery events.
async fn process_mdns_events(
    receiver: mdns_sd::Receiver<ServiceEvent>,
    our_discriminator_hex: String,
    our_public_id: String,
    engine: Arc<FeedEngine>,
    network_key: [u8; 32],
    identity: Identity,
) {
    let mut recent_peers: HashMap<String, Instant> = HashMap::new();

    loop {
        match receiver.recv_async().await {
            Ok(event) => {
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        handle_resolved_service(
                            &info,
                            &our_discriminator_hex,
                            &our_public_id,
                            &engine,
                            &network_key,
                            &identity,
                            &mut recent_peers,
                        ).await;
                    }
                    ServiceEvent::ServiceRemoved(_, full_name) => {
                        tracing::debug!(service = %full_name, "mDNS peer removed");
                    }
                    ServiceEvent::SearchStarted(_) => {
                        tracing::debug!("mDNS search started");
                    }
                    ServiceEvent::SearchStopped(_) => {
                        tracing::debug!("mDNS search stopped");
                    }
                    _ => {}
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "mDNS receiver error");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

/// Parsed peer information from mDNS TXT records.
struct PeerInfo<'a> {
    peer_id: &'a str,
    port: u16,
    addresses: std::collections::HashSet<std::net::IpAddr>,
}

/// Parse and validate peer info from mDNS service TXT records.
///
/// Returns None if the peer should be filtered (wrong network, self, invalid data).
fn parse_peer_info<'a>(
    info: &'a ServiceInfo,
    our_discriminator_hex: &str,
    our_public_id: &str,
) -> Option<PeerInfo<'a>> {
    let properties = info.get_properties();

    let peer_net = properties.get_property_val_str("net").unwrap_or("");
    let peer_id = properties.get_property_val_str("id").unwrap_or("");
    let peer_port_str = properties.get_property_val_str("port").unwrap_or("");

    // Filter by network key
    if peer_net != our_discriminator_hex {
        tracing::trace!(
            service = %info.get_fullname(),
            peer_net = %peer_net,
            our_net = %our_discriminator_hex,
            "ignoring mDNS peer from different network"
        );
        return None;
    }

    // Filter self
    let our_id_short = truncate_id(our_public_id, 12);
    if peer_id == our_id_short {
        tracing::trace!(service = %info.get_fullname(), "ignoring self");
        return None;
    }

    // Parse port (0 is invalid, 1-1023 accepted with warning)
    let port: u16 = match peer_port_str.parse() {
        Ok(0) => {
            tracing::trace!(service = %info.get_fullname(), "mDNS peer has port 0 (invalid)");
            return None;
        }
        Ok(p) if p < 1024 => {
            tracing::debug!(service = %info.get_fullname(), port = p, "mDNS peer using privileged port");
            p
        }
        Ok(p) => p,
        Err(_) => {
            tracing::trace!(service = %info.get_fullname(), port = %peer_port_str, "invalid port (parse error)");
            return None;
        }
    };

    // Get addresses
    let addresses = info.get_addresses();
    if addresses.is_empty() {
        tracing::trace!(service = %info.get_fullname(), "mDNS peer has no addresses");
        return None;
    }

    Some(PeerInfo { peer_id, port, addresses: addresses.clone() })
}

/// Check if we should verify this peer address (rate limiting).
///
/// Returns true if verification should proceed, false to skip.
fn should_verify_peer(
    peer_addr: &str,
    recent_peers: &mut HashMap<String, Instant>,
) -> bool {
    let now = Instant::now();

    // Check cooldown
    if let Some(last_seen) = recent_peers.get(peer_addr) {
        if now.duration_since(*last_seen) < PEER_VERIFY_COOLDOWN {
            return false;
        }
    }

    // Evict stale entries if at capacity
    if recent_peers.len() >= MAX_RECENT_PEERS {
        recent_peers.retain(|_, ts| now.duration_since(*ts) < PEER_VERIFY_COOLDOWN);
        if recent_peers.len() >= MAX_RECENT_PEERS {
            tracing::warn!(
                peer = %peer_addr,
                limit = MAX_RECENT_PEERS,
                "mDNS peer rate limit reached, skipping peer"
            );
            return false;
        }
    }

    recent_peers.insert(peer_addr.to_string(), now);
    true
}

/// Handle a resolved mDNS service (potential peer).
async fn handle_resolved_service(
    info: &ServiceInfo,
    our_discriminator_hex: &str,
    our_public_id: &str,
    engine: &Arc<FeedEngine>,
    network_key: &[u8; 32],
    identity: &Identity,
    recent_peers: &mut HashMap<String, Instant>,
) {
    // Parse and validate peer info
    let peer_info = match parse_peer_info(info, our_discriminator_hex, our_public_id) {
        Some(info) => info,
        None => return,
    };

    // Try each address
    for addr in &peer_info.addresses {
        let peer_addr = format!("{}:{}", addr, peer_info.port);

        // Rate limit check
        if !should_verify_peer(&peer_addr, recent_peers) {
            continue;
        }

        tracing::debug!(
            peer = %peer_addr,
            peer_id = %peer_info.peer_id,
            "discovered mDNS peer, verifying handshake"
        );

        // Parse socket address
        let parsed_addr: SocketAddr = match peer_addr.parse() {
            Ok(a) => a,
            Err(e) => {
                tracing::debug!(peer = %peer_addr, error = %e, "invalid peer address");
                continue;
            }
        };

        // Verify with SHS handshake
        let handshake_result = tokio::time::timeout(
            HANDSHAKE_VERIFY_TIMEOUT,
            verify_peer_handshake(parsed_addr, *network_key, identity.clone()),
        ).await;

        match handshake_result {
            Ok(Ok(())) => {
                tracing::info!(
                    peer = %peer_addr,
                    peer_id = %peer_info.peer_id,
                    "verified mDNS peer"
                );

                let addr_clone = peer_addr.clone();
                let eng = engine.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    if let Err(e) = eng.store().insert_address_peer(&addr_clone) {
                        tracing::warn!(error = %e, "failed to persist mDNS peer");
                    }
                }).await;

                // Only need one working address per service
                break;
            }
            Ok(Err(e)) => {
                tracing::debug!(peer = %peer_addr, error = %e, "mDNS peer handshake failed");
            }
            Err(_) => {
                tracing::debug!(peer = %peer_addr, "mDNS peer handshake timed out");
            }
        }
    }
}

/// Attempt an SHS handshake to verify a discovered peer.
async fn verify_peer_handshake(
    addr: SocketAddr,
    network_key: [u8; 32],
    identity: Identity,
) -> crate::error::Result<()> {
    let stream = TcpStream::connect(addr).await?;
    let conn = SecureConnection::connect(stream, network_key, identity).await?;
    conn.close().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_id_removes_prefix_suffix() {
        let id = "@abc123def456.ed25519";
        assert_eq!(truncate_id(id, 12), "abc123def456");
    }

    #[test]
    fn truncate_id_respects_max_len() {
        let id = "@abc123def456ghi789.ed25519";
        assert_eq!(truncate_id(id, 6), "abc123");
    }

    #[test]
    fn truncate_id_handles_short() {
        let id = "@abc.ed25519";
        assert_eq!(truncate_id(id, 12), "abc");
    }
}
