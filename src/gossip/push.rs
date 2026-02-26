//! Push manager for broadcasting messages to connected peers.
//!
//! Subscribes to FeedEngine events and broadcasts Push messages to all
//! registered persistent connections via the ConnectionRegistry.

use std::sync::Arc;

use tokio::sync::broadcast;

use crate::feed::engine::FeedEngine;
use crate::feed::models::Message;
use crate::gossip::registry::ConnectionRegistry;

/// Manages push-based message broadcasting to connected peers.
///
/// Listens for new messages from the FeedEngine and broadcasts them
/// to all active persistent connections.
pub struct PushManager {
    registry: Arc<ConnectionRegistry>,
    engine: Arc<FeedEngine>,
}

impl PushManager {
    /// Create a new push manager.
    pub fn new(registry: Arc<ConnectionRegistry>, engine: Arc<FeedEngine>) -> Self {
        Self { registry, engine }
    }

    /// Start the push manager loop.
    ///
    /// Subscribes to the FeedEngine's broadcast channel and pushes each
    /// new message to all connected peers. Runs until the channel closes.
    pub async fn run(self) {
        let mut rx = self.engine.subscribe();

        tracing::info!("push manager started");

        loop {
            match rx.recv().await {
                Ok(msg) => {
                    self.handle_message(&msg).await;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        skipped = n,
                        "push manager lagged, some messages not broadcast"
                    );
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::info!("push manager stopping (channel closed)");
                    break;
                }
            }
        }
    }

    /// Handle a new message by broadcasting to all connected peers.
    async fn handle_message(&self, msg: &Message) {
        let conn_count = self.registry.connection_count();
        if conn_count == 0 {
            // No persistent connections, nothing to broadcast
            return;
        }

        tracing::debug!(
            hash = %msg.hash,
            author = %msg.author.0,
            seq = msg.sequence,
            connections = conn_count,
            "broadcasting message to persistent connections"
        );

        self.registry.broadcast(msg).await;
    }

    /// Get a reference to the connection registry.
    pub fn registry(&self) -> &Arc<ConnectionRegistry> {
        &self.registry
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::store::FeedStore;

    #[tokio::test]
    async fn push_manager_handles_no_connections() {
        let store = FeedStore::open_memory().unwrap();
        let engine = Arc::new(FeedEngine::new(store));
        let registry = Arc::new(ConnectionRegistry::new(32));

        let manager = PushManager::new(registry.clone(), engine.clone());

        // With no connections, broadcast should be a no-op
        assert_eq!(manager.registry().connection_count(), 0);
    }
}
