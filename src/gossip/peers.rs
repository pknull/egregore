use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerEntry {
    pub address: String,
    pub public_id: Option<String>,
}
