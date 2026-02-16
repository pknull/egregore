//! Relay-specific configuration — limits beyond what the shared Config provides.

/// Relay-specific configuration (beyond the shared Config).
#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// Maximum registered peers (0 = unlimited).
    pub max_peers: usize,
    /// Message TTL in days (0 = keep forever).
    pub ttl_days: u32,
}
