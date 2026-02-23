//! Peer health observation types for mesh-wide visibility.
//!
//! Each node tracks when it last saw each peer (via successful sync) and
//! shares these observations with other peers during gossip. After O(log N)
//! sync cycles, all nodes converge on transitive health visibility.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::identity::PublicId;

/// Marker for direct observations (this node observed the peer directly).
/// Used in `last_seen_by` field of peer_health table.
pub const DIRECT_OBSERVATION_MARKER: &str = "self";

/// Maximum number of peer observations accepted per gossip message.
/// Based on typical gossip protocols (Serf/SWIM use 5-10 entries per node).
/// Set generously to allow mesh-wide visibility while bounding memory usage.
pub const MAX_PEER_OBSERVATIONS: usize = 500;

/// Maximum age of observations to accept (72 hours, matching Consul's dead node reaping).
/// Observations older than this are rejected as stale.
pub const MAX_OBSERVATION_AGE_HOURS: i64 = 72;

/// Stale health record eviction threshold (72 hours).
/// Records older than this are removed during periodic eviction.
pub const HEALTH_EVICTION_HOURS: i64 = 72;

/// Observation of a peer's health, either direct (self-observed) or
/// transitive (reported by another peer during gossip).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerObservation {
    /// The peer being observed.
    pub peer_id: PublicId,
    /// When this peer was last seen (successful sync completion).
    pub last_seen_at: DateTime<Utc>,
    /// The peer's latest feed sequence number when observed.
    pub last_seq: u64,
    /// Restart counter — prevents stale observations from winning.
    /// Incremented each time a node restarts.
    pub generation: u32,
}

/// Clamp an observation timestamp to a valid range.
///
/// - Not in the future (clock skew protection)
/// - Not older than MAX_OBSERVATION_AGE_HOURS (stale data protection)
///
/// Returns None if the timestamp is too old to be useful.
pub fn clamp_observation_timestamp(ts: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let now = Utc::now();
    let max_age = Duration::hours(MAX_OBSERVATION_AGE_HOURS);
    let oldest_valid = now - max_age;

    if ts > now {
        // Future timestamp: clamp to now (clock skew)
        Some(now)
    } else if ts < oldest_valid {
        // Too old: reject
        None
    } else {
        Some(ts)
    }
}

/// Health status categories based on observation age relative to sync interval.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PeerHealthStatus {
    /// Seen within 2 sync intervals.
    Recent,
    /// Seen within 5 sync intervals.
    Stale,
    /// Seen within 10 sync intervals.
    Suspected,
    /// Never observed.
    Unknown,
}

impl PeerHealthStatus {
    /// Determine status from observation age and sync interval.
    ///
    /// # Arguments
    /// * `age_secs` - Seconds since the peer was last seen
    /// * `sync_interval_secs` - The gossip sync interval in seconds
    pub fn from_age_secs(age_secs: u64, sync_interval_secs: u64) -> Self {
        if sync_interval_secs == 0 {
            return Self::Unknown;
        }
        let intervals = age_secs / sync_interval_secs;
        match intervals {
            0..=1 => Self::Recent,
            2..=4 => Self::Stale,
            5..=9 => Self::Suspected,
            _ => Self::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_from_age() {
        let interval = 60; // 60 second sync interval

        // Within 2 intervals = recent
        assert_eq!(PeerHealthStatus::from_age_secs(0, interval), PeerHealthStatus::Recent);
        assert_eq!(PeerHealthStatus::from_age_secs(60, interval), PeerHealthStatus::Recent);
        assert_eq!(PeerHealthStatus::from_age_secs(119, interval), PeerHealthStatus::Recent);

        // 2-4 intervals = stale
        assert_eq!(PeerHealthStatus::from_age_secs(120, interval), PeerHealthStatus::Stale);
        assert_eq!(PeerHealthStatus::from_age_secs(299, interval), PeerHealthStatus::Stale);

        // 5-9 intervals = suspected
        assert_eq!(PeerHealthStatus::from_age_secs(300, interval), PeerHealthStatus::Suspected);
        assert_eq!(PeerHealthStatus::from_age_secs(599, interval), PeerHealthStatus::Suspected);

        // 10+ intervals = unknown
        assert_eq!(PeerHealthStatus::from_age_secs(600, interval), PeerHealthStatus::Unknown);
        assert_eq!(PeerHealthStatus::from_age_secs(3600, interval), PeerHealthStatus::Unknown);
    }

    #[test]
    fn zero_interval_returns_unknown() {
        assert_eq!(PeerHealthStatus::from_age_secs(100, 0), PeerHealthStatus::Unknown);
    }
}
