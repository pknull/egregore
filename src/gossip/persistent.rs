//! Persistent connection task for push-based replication.
//!
//! Handles the lifecycle of a persistent connection after initial replication
//! completes. Runs as an async task, processing incoming Push messages,
//! flow control messages, and detecting connection failures.

use std::sync::Arc;

use crate::error::{EgreError, Result};
use crate::feed::engine::FeedEngine;
use crate::gossip::connection::SecureReader;
use crate::gossip::flow_control::DEFAULT_CREDIT_GRANT;
use crate::gossip::registry::ConnectionRegistry;
use crate::gossip::replication::GossipMessage;
use crate::identity::PublicId;

/// Task that handles incoming messages on a persistent connection.
///
/// Runs until the connection is closed or an error occurs. On completion,
/// automatically unregisters from the connection registry.
pub struct PersistentConnectionTask {
    reader: SecureReader,
    peer_id: PublicId,
    engine: Arc<FeedEngine>,
    registry: Arc<ConnectionRegistry>,
}

impl PersistentConnectionTask {
    /// Create a new persistent connection task.
    pub fn new(
        reader: SecureReader,
        peer_id: PublicId,
        engine: Arc<FeedEngine>,
        registry: Arc<ConnectionRegistry>,
    ) -> Self {
        Self {
            reader,
            peer_id,
            engine,
            registry,
        }
    }

    /// Run the task, processing incoming messages until connection closes.
    ///
    /// Returns `Ok(())` on graceful shutdown, `Err` on protocol or I/O errors.
    pub async fn run(mut self) -> Result<()> {
        tracing::debug!(peer = %self.peer_id.0, "persistent connection task started");

        let result = self.process_loop().await;

        // Always unregister on exit
        self.registry.unregister(&self.peer_id);

        match &result {
            Ok(()) => {
                tracing::info!(peer = %self.peer_id.0, "persistent connection closed gracefully");
            }
            Err(e) => {
                tracing::warn!(
                    peer = %self.peer_id.0,
                    error = %e,
                    "persistent connection ended with error"
                );
            }
        }

        result
    }

    /// Main processing loop for incoming messages.
    async fn process_loop(&mut self) -> Result<()> {
        loop {
            match self.reader.recv().await {
                Ok(Some(data)) => {
                    self.handle_message(&data).await?;
                }
                Ok(None) => {
                    // Graceful close (goodbye or EOF)
                    return Ok(());
                }
                Err(e) => {
                    // I/O or protocol error
                    return Err(e);
                }
            }
        }
    }

    /// Handle a single incoming message.
    async fn handle_message(&self, data: &[u8]) -> Result<()> {
        let msg: GossipMessage = serde_json::from_slice(data)?;

        match msg {
            GossipMessage::Push { message } => {
                tracing::debug!(
                    peer = %self.peer_id.0,
                    author = %message.author.0,
                    seq = message.sequence,
                    hash = %message.hash,
                    "received pushed message"
                );

                // Ingest on blocking task (SQLite)
                let eng = self.engine.clone();
                let msg_clone = message.clone();
                let result = tokio::task::spawn_blocking(move || eng.ingest(&msg_clone)).await;

                match result {
                    Ok(Ok(())) => {
                        tracing::debug!(hash = %message.hash, "ingested pushed message");
                    }
                    Ok(Err(EgreError::DuplicateMessage { .. })) => {
                        // Already have this message, not an error
                        tracing::trace!(hash = %message.hash, "pushed message is duplicate");
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(
                            hash = %message.hash,
                            error = %e,
                            "failed to ingest pushed message"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "spawn_blocking failed for ingest");
                    }
                }
            }
            GossipMessage::Have { .. }
            | GossipMessage::Want { .. }
            | GossipMessage::Messages { .. }
            | GossipMessage::Done => {
                // Replication messages shouldn't appear in persistent mode
                tracing::warn!(
                    peer = %self.peer_id.0,
                    msg_type = ?msg,
                    "unexpected replication message on persistent connection"
                );
            }
            GossipMessage::Subscribe { .. } | GossipMessage::SubscribeAck { .. } => {
                // Already negotiated, shouldn't receive these again
                tracing::warn!(
                    peer = %self.peer_id.0,
                    "received Subscribe/SubscribeAck on active persistent connection"
                );
            }
            GossipMessage::CreditGrant { amount } => {
                // Peer is granting us credits to send more messages
                tracing::debug!(
                    peer = %self.peer_id.0,
                    amount = amount,
                    "received credit grant"
                );
                self.registry.grant_credits(&self.peer_id, amount).await;
            }
            GossipMessage::CreditRequest { suggested } => {
                // Peer is requesting credits (they want to send us messages)
                // Grant credits based on our capacity
                let grant_amount = suggested.min(DEFAULT_CREDIT_GRANT);
                tracing::debug!(
                    peer = %self.peer_id.0,
                    requested = suggested,
                    granting = grant_amount,
                    "received credit request, granting credits"
                );
                if let Err(e) = self.registry.send_credit_grant(&self.peer_id, grant_amount).await {
                    tracing::warn!(
                        peer = %self.peer_id.0,
                        error = %e,
                        "failed to send credit grant"
                    );
                }
            }
            GossipMessage::FlowControlAck { supported, initial_credits } => {
                // Peer is acknowledging flow control capability
                tracing::debug!(
                    peer = %self.peer_id.0,
                    supported = supported,
                    initial_credits = initial_credits,
                    "received flow control ack"
                );
                self.registry.set_peer_credits_supported(&self.peer_id, supported);
                if supported && initial_credits > 0 {
                    self.registry.grant_credits(&self.peer_id, initial_credits).await;
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    // Integration tests would go here, but they require full setup
    // of SecureConnection pairs which is complex to mock
}
