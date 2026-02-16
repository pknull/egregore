//! Gossip peer address type for API serialization.

use serde::{Deserialize, Serialize};

/// A peer's gossip address as returned by the REST API.
/// `public_id` is populated after a successful SHS handshake reveals their identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerEntry {
    pub address: String,
    pub public_id: Option<String>,
}
