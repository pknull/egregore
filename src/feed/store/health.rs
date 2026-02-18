//! Peer health observation storage.
//!
//! Tracks direct observations (from this node's syncs) and transitive
//! observations (reported by other peers during gossip). Used to build
//! mesh-wide health visibility via the `/v1/mesh` endpoint.

use chrono::{DateTime, Utc};
use rusqlite::params;

use crate::error::{EgreError, Result};
use crate::gossip::health::{PeerObservation, DIRECT_OBSERVATION_MARKER};
use crate::identity::PublicId;

use super::{parse_rfc3339, FeedStore};

/// Parse RFC3339 timestamp with fallback to now, logging a warning on parse failure.
fn parse_rfc3339_or_now(s: &str, field: &str) -> DateTime<Utc> {
    parse_rfc3339(s).unwrap_or_else(|| {
        tracing::warn!(
            field = field,
            value = s,
            "invalid RFC3339 timestamp in peer_health, using current time"
        );
        Utc::now()
    })
}

/// Stored health record combining observation with metadata.
#[derive(Debug, Clone)]
pub struct PeerHealthRecord {
    pub peer_id: PublicId,
    pub last_seen_at: DateTime<Utc>,
    /// "self" for direct observation, or reporter's public_id for transitive.
    pub last_seen_by: String,
    pub last_seq: u64,
    pub generation: u32,
    pub updated_at: DateTime<Utc>,
}

impl FeedStore {
    /// Record a direct observation after successfully syncing with a peer.
    ///
    /// Only updates if the new generation is >= existing generation.
    /// Same generation prefers more recent timestamp.
    pub fn record_direct_observation(
        &self,
        peer_id: &PublicId,
        last_seq: u64,
        generation: u32,
    ) -> Result<()> {
        let conn = self.conn();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO peer_health (peer_id, last_seen_at, last_seen_by, last_seq, generation, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?2)
             ON CONFLICT(peer_id, last_seen_by) DO UPDATE SET
                last_seen_at = CASE
                    WHEN excluded.generation > peer_health.generation THEN excluded.last_seen_at
                    WHEN excluded.generation = peer_health.generation THEN excluded.last_seen_at
                    ELSE peer_health.last_seen_at
                END,
                last_seq = CASE
                    WHEN excluded.generation > peer_health.generation THEN excluded.last_seq
                    WHEN excluded.generation = peer_health.generation THEN excluded.last_seq
                    ELSE peer_health.last_seq
                END,
                generation = CASE
                    WHEN excluded.generation > peer_health.generation THEN excluded.generation
                    ELSE peer_health.generation
                END,
                updated_at = excluded.updated_at
             WHERE excluded.generation >= peer_health.generation",
            params![peer_id.0, now, DIRECT_OBSERVATION_MARKER, last_seq, generation],
        )?;
        Ok(())
    }

    /// Merge a transitive observation reported by another peer.
    ///
    /// Only accepts if the observation's generation is >= existing.
    /// Same generation prefers more recent last_seen_at.
    pub fn merge_transitive_observation(
        &self,
        observation: &PeerObservation,
        reported_by: &PublicId,
    ) -> Result<()> {
        let conn = self.conn();
        let now = Utc::now().to_rfc3339();
        let last_seen_str = observation.last_seen_at.to_rfc3339();

        conn.execute(
            "INSERT INTO peer_health (peer_id, last_seen_at, last_seen_by, last_seq, generation, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(peer_id, last_seen_by) DO UPDATE SET
                last_seen_at = CASE
                    WHEN excluded.generation > peer_health.generation THEN excluded.last_seen_at
                    WHEN excluded.generation = peer_health.generation
                         AND excluded.last_seen_at > peer_health.last_seen_at THEN excluded.last_seen_at
                    ELSE peer_health.last_seen_at
                END,
                last_seq = CASE
                    WHEN excluded.generation > peer_health.generation THEN excluded.last_seq
                    WHEN excluded.generation = peer_health.generation
                         AND excluded.last_seen_at > peer_health.last_seen_at THEN excluded.last_seq
                    ELSE peer_health.last_seq
                END,
                generation = CASE
                    WHEN excluded.generation > peer_health.generation THEN excluded.generation
                    ELSE peer_health.generation
                END,
                updated_at = ?6
             WHERE excluded.generation > peer_health.generation
                OR (excluded.generation = peer_health.generation
                    AND excluded.last_seen_at > peer_health.last_seen_at)",
            params![
                observation.peer_id.0,
                last_seen_str,
                reported_by.0,
                observation.last_seq,
                observation.generation,
                now,
            ],
        )?;
        Ok(())
    }

    /// Get all health observations for building the mesh view.
    pub fn get_all_peer_health(&self) -> Result<Vec<PeerHealthRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT peer_id, last_seen_at, last_seen_by, last_seq, generation, updated_at
             FROM peer_health ORDER BY peer_id, last_seen_at DESC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let peer_id: String = row.get(0)?;
                let last_seen_at_str: String = row.get(1)?;
                let last_seen_by: String = row.get(2)?;
                let last_seq: i64 = row.get(3)?;
                let generation: i64 = row.get(4)?;
                let updated_at_str: String = row.get(5)?;

                Ok(PeerHealthRecord {
                    peer_id: PublicId(peer_id),
                    last_seen_at: parse_rfc3339_or_now(&last_seen_at_str, "last_seen_at"),
                    last_seen_by,
                    last_seq: last_seq as u64,
                    generation: generation as u32,
                    updated_at: parse_rfc3339_or_now(&updated_at_str, "updated_at"),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Get direct observations to share with peers during gossip.
    pub fn get_direct_observations(&self) -> Result<Vec<PeerObservation>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT peer_id, last_seen_at, last_seq, generation
             FROM peer_health WHERE last_seen_by = ?1",
        )?;
        let rows = stmt
            .query_map(params![DIRECT_OBSERVATION_MARKER], |row| {
                let peer_id: String = row.get(0)?;
                let last_seen_at_str: String = row.get(1)?;
                let last_seq: i64 = row.get(2)?;
                let generation: i64 = row.get(3)?;

                Ok(PeerObservation {
                    peer_id: PublicId(peer_id),
                    last_seen_at: parse_rfc3339_or_now(&last_seen_at_str, "last_seen_at"),
                    last_seq: last_seq as u64,
                    generation: generation as u32,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Get the local node's generation counter.
    pub fn get_local_generation(&self) -> Result<u32> {
        let conn = self.conn();
        let result = conn.query_row(
            "SELECT value FROM local_state WHERE key = 'generation'",
            [],
            |row| {
                let value: String = row.get(0)?;
                Ok(value.parse::<u32>().unwrap_or(0))
            },
        );
        match result {
            Ok(gen) => Ok(gen),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(0),
            Err(e) => Err(EgreError::Database(e)),
        }
    }

    /// Increment the local generation counter (called on startup).
    /// Returns the new generation value. Saturates at u32::MAX to prevent overflow.
    pub fn increment_generation(&self) -> Result<u32> {
        let current = self.get_local_generation()?;
        let new_gen = current.saturating_add(1);
        let conn = self.conn();
        conn.execute(
            "INSERT INTO local_state (key, value) VALUES ('generation', ?1)
             ON CONFLICT(key) DO UPDATE SET value = ?1",
            params![new_gen.to_string()],
        )?;
        Ok(new_gen)
    }

    /// Evict stale health records older than the given cutoff.
    /// Returns the number of evicted records.
    pub fn evict_stale_health(&self, cutoff: &DateTime<Utc>) -> Result<u64> {
        let conn = self.conn();
        let cutoff_str = cutoff.to_rfc3339();
        let count = conn.execute(
            "DELETE FROM peer_health WHERE last_seen_at < ?1",
            params![cutoff_str],
        )? as u64;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_observation_recorded() {
        let store = FeedStore::open_memory().unwrap();
        let peer = PublicId("@test.ed25519".to_string());

        store.record_direct_observation(&peer, 10, 1).unwrap();
        let observations = store.get_direct_observations().unwrap();

        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].peer_id, peer);
        assert_eq!(observations[0].last_seq, 10);
        assert_eq!(observations[0].generation, 1);
    }

    #[test]
    fn generation_prevents_stale_update() {
        let store = FeedStore::open_memory().unwrap();
        let peer = PublicId("@test.ed25519".to_string());

        // Record with generation 2
        store.record_direct_observation(&peer, 10, 2).unwrap();

        // Try to update with older generation 1
        store.record_direct_observation(&peer, 20, 1).unwrap();

        let observations = store.get_direct_observations().unwrap();
        assert_eq!(observations[0].last_seq, 10); // Old seq preserved
        assert_eq!(observations[0].generation, 2); // Old gen preserved
    }

    #[test]
    fn same_generation_updates() {
        let store = FeedStore::open_memory().unwrap();
        let peer = PublicId("@test.ed25519".to_string());

        // Record with generation 1
        store.record_direct_observation(&peer, 10, 1).unwrap();

        // Update with same generation
        store.record_direct_observation(&peer, 20, 1).unwrap();

        let observations = store.get_direct_observations().unwrap();
        assert_eq!(observations[0].last_seq, 20); // Updated
        assert_eq!(observations[0].generation, 1);
    }

    #[test]
    fn transitive_observation_merged() {
        let store = FeedStore::open_memory().unwrap();
        let peer = PublicId("@peer.ed25519".to_string());
        let reporter = PublicId("@reporter.ed25519".to_string());

        let obs = PeerObservation {
            peer_id: peer.clone(),
            last_seen_at: Utc::now(),
            last_seq: 5,
            generation: 1,
        };

        store.merge_transitive_observation(&obs, &reporter).unwrap();
        let all = store.get_all_peer_health().unwrap();

        assert_eq!(all.len(), 1);
        assert_eq!(all[0].last_seen_by, reporter.0);
    }

    #[test]
    fn generation_counter() {
        let store = FeedStore::open_memory().unwrap();

        assert_eq!(store.get_local_generation().unwrap(), 0);

        let gen1 = store.increment_generation().unwrap();
        assert_eq!(gen1, 1);
        assert_eq!(store.get_local_generation().unwrap(), 1);

        let gen2 = store.increment_generation().unwrap();
        assert_eq!(gen2, 2);
        assert_eq!(store.get_local_generation().unwrap(), 2);
    }

    #[test]
    fn evict_stale_health() {
        let store = FeedStore::open_memory().unwrap();
        let peer = PublicId("@old.ed25519".to_string());

        // Record an observation
        store.record_direct_observation(&peer, 10, 1).unwrap();

        // Evict with future cutoff (nothing should be evicted since we just recorded)
        let cutoff = Utc::now() - chrono::Duration::hours(1);
        let evicted = store.evict_stale_health(&cutoff).unwrap();
        assert_eq!(evicted, 0);

        // Evict with past cutoff (our recent observation stays)
        let future_cutoff = Utc::now() + chrono::Duration::hours(1);
        let evicted = store.evict_stale_health(&future_cutoff).unwrap();
        assert_eq!(evicted, 1);

        assert_eq!(store.get_all_peer_health().unwrap().len(), 0);
    }
}
