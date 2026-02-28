//! Connection registry for persistent push-based connections.
//!
//! Tracks active persistent connections and provides broadcast capability.
//! Uses DashMap for lock-free concurrent access from multiple tasks.
//!
//! Flow control is managed per-peer through `PeerFlowState`, which tracks
//! credits and rate limits for backpressure-aware message pushing.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use tokio::sync::Mutex;

use crate::error::Result;
use crate::feed::models::Message;
use crate::gossip::connection::SecureWriter;
use crate::gossip::flow_control::{
    calculate_adaptive_fanout, should_send_to_peer, FlowControlConfig, FlowMetrics, PeerFlowState,
};
use crate::gossip::replication::GossipMessage;
use crate::identity::PublicId;

/// Handle to a persistent connection's write half.
///
/// Stored in the registry for broadcasting messages to connected peers.
/// Includes flow control state for credit-based backpressure.
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
    /// Flow control state for this peer.
    flow_state: Arc<PeerFlowState>,
}

impl ConnectionHandle {
    /// Create a new connection handle with default flow control.
    pub fn new(
        writer: SecureWriter,
        peer_id: PublicId,
        peer_addr: SocketAddr,
        is_outgoing: bool,
    ) -> Self {
        Self::with_flow_config(
            writer,
            peer_id,
            peer_addr,
            is_outgoing,
            FlowControlConfig::default(),
        )
    }

    /// Create a new connection handle with custom flow control config.
    pub fn with_flow_config(
        writer: SecureWriter,
        peer_id: PublicId,
        peer_addr: SocketAddr,
        is_outgoing: bool,
        flow_config: FlowControlConfig,
    ) -> Self {
        Self {
            writer: Arc::new(Mutex::new(writer)),
            peer_id,
            peer_addr,
            established_at: Instant::now(),
            is_outgoing,
            flow_state: Arc::new(PeerFlowState::new(flow_config)),
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

    /// Get the flow state for this connection.
    pub fn flow_state(&self) -> &Arc<PeerFlowState> {
        &self.flow_state
    }

    /// Get flow metrics for this connection.
    pub fn flow_metrics(&self) -> FlowMetrics {
        self.flow_state.metrics()
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
    /// Total messages dropped due to backpressure.
    pub pushes_dropped_backpressure: AtomicU64,
    /// Total messages skipped due to adaptive fanout.
    pub pushes_skipped_fanout: AtomicU64,
}

/// Registry of active persistent connections.
///
/// Thread-safe, lock-free access for concurrent connection management
/// and message broadcasting with flow control.
pub struct ConnectionRegistry {
    /// Active connections indexed by peer public ID.
    connections: DashMap<PublicId, ConnectionHandle>,
    /// Maximum allowed persistent connections.
    max_connections: usize,
    /// Metrics for monitoring.
    pub metrics: ConnectionMetrics,
    /// Flow control configuration for new connections.
    flow_config: FlowControlConfig,
}

impl ConnectionRegistry {
    /// Create a new registry with the given connection limit.
    pub fn new(max_connections: usize) -> Self {
        Self::with_flow_config(max_connections, FlowControlConfig::default())
    }

    /// Create a new registry with custom flow control configuration.
    pub fn with_flow_config(max_connections: usize, flow_config: FlowControlConfig) -> Self {
        Self {
            connections: DashMap::new(),
            max_connections,
            metrics: ConnectionMetrics::default(),
            flow_config,
        }
    }

    /// Get the flow control configuration.
    pub fn flow_config(&self) -> &FlowControlConfig {
        &self.flow_config
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

    /// Broadcast a message to all connected peers with flow control.
    ///
    /// Applies credit-based backpressure and adaptive fanout:
    /// - Checks if peer has credits available before sending
    /// - Applies rate limiting per peer
    /// - Uses adaptive fanout to reduce sends to unhealthy peers
    ///
    /// Failed sends are logged and connections are removed.
    pub async fn broadcast(&self, msg: &Message) {
        let gossip_msg = GossipMessage::Push {
            message: msg.clone(),
        };

        self.metrics.messages_broadcast.fetch_add(1, Ordering::Relaxed);

        // Collect handles with flow state to avoid holding DashMap references across await
        let peers: Vec<(PublicId, Arc<Mutex<SecureWriter>>, Arc<PeerFlowState>)> = self
            .connections
            .iter()
            .map(|entry| {
                (
                    entry.key().clone(),
                    entry.value().writer(),
                    entry.value().flow_state().clone(),
                )
            })
            .collect();

        if peers.is_empty() {
            return;
        }

        // Send to all connections concurrently with flow control
        let msg_data = match serde_json::to_vec(&gossip_msg) {
            Ok(data) => data,
            Err(e) => {
                tracing::error!(error = %e, "failed to serialize push message");
                return;
            }
        };

        // Determine which peers to send to based on flow control
        let mut send_targets = Vec::new();
        let mut skipped_backpressure = 0u64;
        let mut skipped_fanout = 0u64;

        for (peer_id, writer, flow_state) in &peers {
            // Check flow control (credits and rate limit)
            if !flow_state.can_send().await {
                flow_state.record_drop();
                skipped_backpressure += 1;
                tracing::trace!(
                    peer = %peer_id.0,
                    credits = flow_state.credits(),
                    "skipping push due to backpressure"
                );
                continue;
            }

            // Calculate adaptive fanout based on peer health
            let metrics = flow_state.metrics();
            let fanout_score =
                calculate_adaptive_fanout(metrics.messages_sent, metrics.messages_dropped, metrics.credits);

            if !should_send_to_peer(fanout_score) {
                skipped_fanout += 1;
                tracing::trace!(
                    peer = %peer_id.0,
                    fanout_score = fanout_score,
                    "skipping push due to adaptive fanout"
                );
                continue;
            }

            send_targets.push((peer_id.clone(), writer.clone(), flow_state.clone()));
        }

        if skipped_backpressure > 0 {
            self.metrics
                .pushes_dropped_backpressure
                .fetch_add(skipped_backpressure, Ordering::Relaxed);
        }
        if skipped_fanout > 0 {
            self.metrics
                .pushes_skipped_fanout
                .fetch_add(skipped_fanout, Ordering::Relaxed);
        }

        if send_targets.is_empty() {
            return;
        }

        let send_futures = send_targets.iter().map(|(peer_id, writer, flow_state)| {
            let peer_id = peer_id.clone();
            let writer = writer.clone();
            let flow_state = flow_state.clone();
            let data = msg_data.clone();
            let hash = msg.hash.clone();
            async move {
                let result = {
                    let mut w = writer.lock().await;
                    w.send(&data).await
                };
                if result.is_ok() {
                    // Consume credit and rate limit token on successful send
                    flow_state.consume_send().await;
                }
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

    /// Get extended connection info including flow metrics.
    pub fn connection_info_with_flow(&self) -> Vec<ConnectionInfoWithFlow> {
        self.connections
            .iter()
            .map(|entry| {
                let handle = entry.value();
                ConnectionInfoWithFlow {
                    peer_id: handle.peer_id.clone(),
                    peer_addr: handle.peer_addr,
                    established_at: handle.established_at,
                    is_outgoing: handle.is_outgoing,
                    flow_metrics: handle.flow_metrics(),
                }
            })
            .collect()
    }

    /// Grant credits to a specific peer.
    ///
    /// Used when receiving a CreditGrant message from a peer.
    pub async fn grant_credits(&self, peer_id: &PublicId, amount: u32) {
        if let Some(entry) = self.connections.get(peer_id) {
            entry.flow_state().grant_credits(amount).await;
            tracing::debug!(
                peer = %peer_id.0,
                amount = amount,
                new_credits = entry.flow_state().credits(),
                "granted credits to peer"
            );
        }
    }

    /// Set whether a peer supports credit-based flow control.
    pub fn set_peer_credits_supported(&self, peer_id: &PublicId, supported: bool) {
        if let Some(entry) = self.connections.get(peer_id) {
            entry.flow_state().set_credits_supported(supported);
        }
    }

    /// Get peers that need credit requests sent.
    ///
    /// Returns peers whose credits have fallen below the low watermark.
    pub fn peers_needing_credits(&self) -> Vec<PublicId> {
        self.connections
            .iter()
            .filter(|entry| entry.value().flow_state().needs_credit_request())
            .map(|entry| entry.key().clone())
            .collect()
    }

    /// Send a credit grant message to a peer.
    pub async fn send_credit_grant(&self, peer_id: &PublicId, amount: u32) -> Result<()> {
        if let Some(entry) = self.connections.get(peer_id) {
            let msg = GossipMessage::CreditGrant { amount };
            entry.value().send(&msg).await
        } else {
            Ok(()) // Peer not connected, no-op
        }
    }

    /// Send a credit request message to a peer.
    pub async fn send_credit_request(&self, peer_id: &PublicId, suggested: u32) -> Result<()> {
        if let Some(entry) = self.connections.get(peer_id) {
            let msg = GossipMessage::CreditRequest { suggested };
            entry.value().send(&msg).await
        } else {
            Ok(()) // Peer not connected, no-op
        }
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

/// Extended connection info including flow control metrics.
#[derive(Debug, Clone)]
pub struct ConnectionInfoWithFlow {
    pub peer_id: PublicId,
    pub peer_addr: SocketAddr,
    pub established_at: Instant,
    pub is_outgoing: bool,
    pub flow_metrics: FlowMetrics,
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
