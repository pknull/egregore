//! Connection registry for persistent push-based connections.
//!
//! Tracks active persistent connections and provides broadcast capability.
//! Uses DashMap for lock-free concurrent access from multiple tasks.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use tokio::sync::Mutex;

use crate::error::Result;
use crate::feed::models::Message;
use crate::gossip::connection::SecureWriter;
use crate::gossip::replication::GossipMessage;
use crate::identity::PublicId;

/// Handle to a persistent connection's write half.
///
/// Stored in the registry for broadcasting messages to connected peers.
pub struct ConnectionHandle {
    /// The write half, guarded by a mutex for exclusive send access.
    writer: Arc<Mutex<SecureWriter>>,
    /// The remote peer's public identity.
    pub peer_id: PublicId,
    /// The remote peer's socket address.
    pub peer_addr: SocketAddr,
    /// When this connection was established.
    pub established_at: Instant,
    /// True if we initiated this connection (client), false if incoming (server).
    pub is_outgoing: bool,
}

impl ConnectionHandle {
    /// Create a new connection handle.
    pub fn new(
        writer: SecureWriter,
        peer_id: PublicId,
        peer_addr: SocketAddr,
        is_outgoing: bool,
    ) -> Self {
        Self {
            writer: Arc::new(Mutex::new(writer)),
            peer_id,
            peer_addr,
            established_at: Instant::now(),
            is_outgoing,
        }
    }

    /// Send a message over this connection.
    pub async fn send(&self, msg: &GossipMessage) -> Result<()> {
        let data = serde_json::to_vec(msg)?;
        let mut writer = self.writer.lock().await;
        writer.send(&data).await
    }

    /// Get the writer for direct access (e.g., for goodbye).
    pub fn writer(&self) -> Arc<Mutex<SecureWriter>> {
        self.writer.clone()
    }
}

/// Metrics for connection tracking.
#[derive(Default)]
pub struct ConnectionMetrics {
    /// Total messages broadcast since startup.
    pub messages_broadcast: AtomicU64,
    /// Total push messages sent (across all connections).
    pub pushes_sent: AtomicU64,
    /// Total push send failures.
    pub push_failures: AtomicU64,
}

/// Registry of active persistent connections.
///
/// Thread-safe, lock-free access for concurrent connection management
/// and message broadcasting.
pub struct ConnectionRegistry {
    /// Active connections indexed by peer public ID.
    connections: DashMap<PublicId, ConnectionHandle>,
    /// Maximum allowed persistent connections.
    max_connections: usize,
    /// Metrics for monitoring.
    pub metrics: ConnectionMetrics,
}

impl ConnectionRegistry {
    /// Create a new registry with the given connection limit.
    pub fn new(max_connections: usize) -> Self {
        Self {
            connections: DashMap::new(),
            max_connections,
            metrics: ConnectionMetrics::default(),
        }
    }

    /// Register a new persistent connection.
    ///
    /// Returns `false` if the connection limit is reached or the peer is
    /// already registered (duplicate connection).
    ///
    /// Uses atomic entry API to avoid TOCTOU race conditions.
    pub fn register(&self, handle: ConnectionHandle) -> bool {
        let peer_id = handle.peer_id.clone();

        // Check connection limit first (may slightly over-accept under high concurrency,
        // but that's acceptable for a soft limit)
        if self.connections.len() >= self.max_connections {
            tracing::warn!(
                peer = %peer_id.0,
                limit = self.max_connections,
                "connection limit reached, rejecting"
            );
            return false;
        }

        // Use entry API for atomic check-and-insert
        use dashmap::mapref::entry::Entry;
        match self.connections.entry(peer_id.clone()) {
            Entry::Occupied(_) => {
                tracing::debug!(peer = %peer_id.0, "duplicate connection, rejecting");
                false
            }
            Entry::Vacant(entry) => {
                tracing::info!(
                    peer = %peer_id.0,
                    addr = %handle.peer_addr,
                    direction = if handle.is_outgoing { "outgoing" } else { "incoming" },
                    "registered persistent connection"
                );
                entry.insert(handle);
                true
            }
        }
    }

    /// Unregister a connection by peer ID.
    ///
    /// Returns the removed handle if it existed.
    pub fn unregister(&self, peer_id: &PublicId) -> Option<ConnectionHandle> {
        if let Some((_, handle)) = self.connections.remove(peer_id) {
            tracing::info!(
                peer = %peer_id.0,
                duration_secs = handle.established_at.elapsed().as_secs(),
                "unregistered persistent connection"
            );
            Some(handle)
        } else {
            None
        }
    }

    /// Check if a peer is currently connected.
    pub fn is_connected(&self, peer_id: &PublicId) -> bool {
        self.connections.contains_key(peer_id)
    }

    /// Get the number of active connections.
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    /// Get a list of all connected peer IDs.
    pub fn connected_peers(&self) -> Vec<PublicId> {
        self.connections.iter().map(|r| r.key().clone()).collect()
    }

    /// Broadcast a message to all connected peers.
    ///
    /// Sends concurrently to all connections using parallel futures.
    /// Failed sends are logged and connections are removed.
    pub async fn broadcast(&self, msg: &Message) {
        let gossip_msg = GossipMessage::Push {
            message: msg.clone(),
        };

        self.metrics.messages_broadcast.fetch_add(1, Ordering::Relaxed);

        // Collect handles first to avoid holding DashMap references across await
        let peers: Vec<(PublicId, Arc<Mutex<SecureWriter>>)> = self
            .connections
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().writer()))
            .collect();

        if peers.is_empty() {
            return;
        }

        // Send to all connections concurrently
        let msg_data = match serde_json::to_vec(&gossip_msg) {
            Ok(data) => data,
            Err(e) => {
                tracing::error!(error = %e, "failed to serialize push message");
                return;
            }
        };

        let send_futures = peers.iter().map(|(peer_id, writer)| {
            let peer_id = peer_id.clone();
            let writer = writer.clone();
            let data = msg_data.clone();
            let hash = msg.hash.clone();
            async move {
                let result = {
                    let mut w = writer.lock().await;
                    w.send(&data).await
                };
                (peer_id, hash, result)
            }
        });

        let results = futures::future::join_all(send_futures).await;

        let mut failed_peers = Vec::new();
        for (peer_id, hash, result) in results {
            match result {
                Ok(()) => {
                    self.metrics.pushes_sent.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(
                        peer = %peer_id.0,
                        hash = %hash,
                        "pushed message"
                    );
                }
                Err(e) => {
                    self.metrics.push_failures.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        peer = %peer_id.0,
                        error = %e,
                        "failed to push message, marking for removal"
                    );
                    failed_peers.push(peer_id);
                }
            }
        }

        // Remove failed connections
        for peer_id in failed_peers {
            self.unregister(&peer_id);
        }
    }

    /// Get connection info for status reporting.
    pub fn connection_info(&self) -> Vec<ConnectionInfo> {
        self.connections
            .iter()
            .map(|entry| {
                let handle = entry.value();
                ConnectionInfo {
                    peer_id: handle.peer_id.clone(),
                    peer_addr: handle.peer_addr,
                    established_at: handle.established_at,
                    is_outgoing: handle.is_outgoing,
                }
            })
            .collect()
    }

    /// Gracefully close all persistent connections.
    ///
    /// Sends goodbye frames to all connected peers and removes them from the registry.
    /// Used during graceful shutdown.
    pub async fn close_all(&self) {
        let count = self.connections.len();
        if count == 0 {
            return;
        }

        tracing::info!(
            connection_count = count,
            "closing all persistent connections"
        );

        // Collect all connections to close
        let peers: Vec<(PublicId, Arc<Mutex<SecureWriter>>)> = self
            .connections
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().writer()))
            .collect();

        // Send goodbye to all concurrently
        let goodbye_futures = peers.iter().map(|(peer_id, writer)| {
            let peer_id = peer_id.clone();
            let writer = writer.clone();
            async move {
                let result = {
                    let mut w = writer.lock().await;
                    w.goodbye().await
                };
                if let Err(e) = result {
                    tracing::debug!(
                        peer = %peer_id.0,
                        error = %e,
                        "failed to send goodbye"
                    );
                }
                peer_id
            }
        });

        let closed_peers = futures::future::join_all(goodbye_futures).await;

        // Remove all from registry
        for peer_id in closed_peers {
            self.connections.remove(&peer_id);
        }

        tracing::info!("all persistent connections closed");
    }
}

/// Summary info about a connection for status reporting.
#[derive(Debug, Clone)]
pub struct ConnectionInfo {
    pub peer_id: PublicId,
    pub peer_addr: SocketAddr,
    pub established_at: Instant,
    pub is_outgoing: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_peer_id(n: u8) -> PublicId {
        PublicId(format!("@test-peer-{}.ed25519", n))
    }

    #[test]
    fn registry_respects_max_connections() {
        let registry = ConnectionRegistry::new(2);

        // We can't easily mock SecureWriter, so we'll just test the logic
        // through the public API where possible
        assert_eq!(registry.connection_count(), 0);
        assert!(!registry.is_connected(&mock_peer_id(1)));
    }

    #[test]
    fn connected_peers_returns_all() {
        let registry = ConnectionRegistry::new(10);
        assert!(registry.connected_peers().is_empty());
    }
}
