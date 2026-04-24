//! CRUD for `pending_forwarding`, `bus_author_seq_index`, and
//! `bus_stream_identity` — Phase 2 Wave 1 Step 7.
//!
//! Three independent tables are grouped here because their CRUD is used
//! by the same downstream clients (the bus adapter in Wave 2, the retry
//! scheduler in Step 8, the restart-time stream-reset detection in
//! Wave 2 Step 9, and the chain-gap / author-silence metric in Wave 4).
//!
//! Idempotent enqueue (amendment §G.2): `pending_forwarding_enqueue`
//! uses `INSERT OR IGNORE` so `FeedEngine::publish_full`'s synchronous
//! pre-enqueue and `BusTransport::publish`'s enqueue-before-attempt
//! both land on the same row without a duplicate-key error. Callers
//! inspect the `EnqueueResult` when they need to distinguish first
//! insertion from a no-op.

use chrono::{DateTime, Utc};
use rusqlite::params;

use crate::error::{EgreError, Result};
use crate::feed::models::Message;

use super::{parse_rfc3339, FeedStore};

/// Outcome of a `pending_forwarding_enqueue` call (amendment §G.2).
///
/// `Inserted` means a fresh row was created; `AlreadyPresent` means the
/// `(transport_id, message_hash)` pair was already enqueued (either by
/// an earlier publish attempt, a retry loop, or the concurrent
/// dispatcher path). Both outcomes are Ok — the second caller's work
/// is a no-op from the CRUD layer's perspective.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueResult {
    Inserted,
    AlreadyPresent,
}

/// One row of the `pending_forwarding` table.
#[derive(Debug, Clone)]
pub struct PendingRow {
    pub transport_id: String,
    pub message_hash: String,
    pub author: String,
    pub sequence: u64,
    pub enqueued_at: DateTime<Utc>,
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub attempt_count: u32,
    pub last_error: Option<String>,
}

/// One row of the max-indexed-at-per-author projection of
/// `bus_author_seq_index`. Feeds the author-silence metric
/// (amendment §G.3) in Wave 4.
#[derive(Debug, Clone)]
pub struct AuthorActivityRow {
    pub author: String,
    pub last_indexed_at: DateTime<Utc>,
}

/// Per-author row including the author's highest indexed
/// `(author_seq, stream_seq)` alongside `last_indexed_at`. Feeds
/// scry's `/v1/transport/bus/authors` endpoint (amendment §C.14).
/// Distinct from `AuthorActivityRow` which is author-silence-only.
#[derive(Debug, Clone)]
pub struct BusAuthorSummaryRow {
    pub author: String,
    pub last_indexed_at: DateTime<Utc>,
    pub author_seq: u64,
    pub stream_seq: u64,
}

/// Stream identity marker (amendment §G.1). Persisted on first index
/// population; compared against live `stream.info()` on startup.
#[derive(Debug, Clone)]
pub struct StreamIdentity {
    pub stream_name: String,
    pub first_ts: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub recorded_at: DateTime<Utc>,
}

impl FeedStore {
    // ---- pending_forwarding ----

    /// Idempotent enqueue (amendment §G.2). Returns `Inserted` if this
    /// is a fresh row; `AlreadyPresent` if the `(transport_id, hash)`
    /// pair already exists.
    pub fn pending_forwarding_enqueue(
        &self,
        transport_id: &str,
        msg: &Message,
    ) -> Result<EnqueueResult> {
        let conn = self.conn();
        let changed = conn.execute(
            "INSERT OR IGNORE INTO pending_forwarding
                (transport_id, message_hash, author, sequence, enqueued_at, attempt_count)
             VALUES (?1, ?2, ?3, ?4, ?5, 0)",
            params![
                transport_id,
                &msg.hash,
                &msg.author.0,
                msg.sequence as i64,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(if changed == 1 {
            EnqueueResult::Inserted
        } else {
            EnqueueResult::AlreadyPresent
        })
    }

    /// Remove the row after successful forwarding.
    pub fn pending_forwarding_complete(
        &self,
        transport_id: &str,
        message_hash: &str,
    ) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "DELETE FROM pending_forwarding
             WHERE transport_id = ?1 AND message_hash = ?2",
            params![transport_id, message_hash],
        )?;
        Ok(())
    }

    /// List up to `limit` pending rows for a transport, oldest first.
    pub fn pending_forwarding_list(
        &self,
        transport_id: &str,
        limit: u32,
    ) -> Result<Vec<PendingRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT transport_id, message_hash, author, sequence, enqueued_at,
                    last_attempt_at, attempt_count, last_error
             FROM pending_forwarding
             WHERE transport_id = ?1
             ORDER BY enqueued_at ASC
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![transport_id, limit as i64], |row| {
                let transport_id: String = row.get(0)?;
                let message_hash: String = row.get(1)?;
                let author: String = row.get(2)?;
                let sequence_i: i64 = row.get(3)?;
                let enqueued_at_s: String = row.get(4)?;
                let last_attempt_at_s: Option<String> = row.get(5)?;
                let attempt_count_i: i64 = row.get(6)?;
                let last_error: Option<String> = row.get(7)?;
                Ok((
                    transport_id,
                    message_hash,
                    author,
                    sequence_i,
                    enqueued_at_s,
                    last_attempt_at_s,
                    attempt_count_i,
                    last_error,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        rows.into_iter()
            .map(
                |(tid, hash, author, seq, enqueued, last_attempt, attempts, err)| {
                    let enqueued_at =
                        parse_rfc3339(&enqueued).ok_or_else(|| EgreError::Config {
                            reason: format!("invalid pending_forwarding.enqueued_at: {enqueued}"),
                        })?;
                    let last_attempt_at = last_attempt.as_deref().and_then(parse_rfc3339);
                    Ok(PendingRow {
                        transport_id: tid,
                        message_hash: hash,
                        author,
                        sequence: seq as u64,
                        enqueued_at,
                        last_attempt_at,
                        attempt_count: attempts as u32,
                        last_error: err,
                    })
                },
            )
            .collect()
    }

    /// Record a failed attempt: bumps `attempt_count`, sets
    /// `last_attempt_at = now`, and stores a PII-scrubbed error code.
    /// Caller is responsible for scrubbing (auditor A2 guidance).
    pub fn pending_forwarding_record_failure(
        &self,
        transport_id: &str,
        message_hash: &str,
        error: &str,
    ) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE pending_forwarding
             SET attempt_count = attempt_count + 1,
                 last_attempt_at = ?3,
                 last_error = ?4
             WHERE transport_id = ?1 AND message_hash = ?2",
            params![transport_id, message_hash, Utc::now().to_rfc3339(), error,],
        )?;
        Ok(())
    }

    /// Count pending rows for a transport (feeds `unreplicated_count`
    /// on `TransportHealth` in Wave 2).
    pub fn pending_forwarding_count(&self, transport_id: &str) -> Result<u64> {
        let conn = self.conn();
        conn.query_row(
            "SELECT COUNT(*) FROM pending_forwarding WHERE transport_id = ?1",
            params![transport_id],
            |row| row.get::<_, i64>(0).map(|c| c as u64),
        )
        .map_err(EgreError::from)
    }

    // ---- bus_author_seq_index ----

    /// INSERT OR REPLACE mapping of `(author, author_seq) → stream_seq`.
    /// Callers (publish PubAck path + subscribe ingest path) both write
    /// the same tuple so REPLACE is safe — the row is always deterministic.
    pub fn bus_author_seq_index_insert(
        &self,
        author: &str,
        author_seq: u64,
        stream_seq: u64,
    ) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT OR REPLACE INTO bus_author_seq_index
                (author, author_seq, stream_seq, indexed_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                author,
                author_seq as i64,
                stream_seq as i64,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// `SELECT MIN(stream_seq) WHERE author=? AND author_seq > ?` —
    /// supports `BusTransport::request_from` (amendment §C.3).
    pub fn bus_author_seq_index_find_stream_seq(
        &self,
        author: &str,
        after_seq: u64,
    ) -> Result<Option<u64>> {
        let conn = self.conn();
        conn.query_row(
            "SELECT MIN(stream_seq) FROM bus_author_seq_index
             WHERE author = ?1 AND author_seq > ?2",
            params![author, after_seq as i64],
            |row| row.get::<_, Option<i64>>(0),
        )
        .map(|opt| opt.map(|i| i as u64))
        .map_err(EgreError::from)
    }

    /// `SELECT MIN(stream_seq)` across the whole index — used in the
    /// first-seq axis of stream-reset detection (amendment §C.6).
    pub fn bus_author_seq_index_min_stream_seq(&self) -> Result<Option<u64>> {
        let conn = self.conn();
        conn.query_row(
            "SELECT MIN(stream_seq) FROM bus_author_seq_index",
            [],
            |row| row.get::<_, Option<i64>>(0),
        )
        .map(|opt| opt.map(|i| i as u64))
        .map_err(EgreError::from)
    }

    /// `SELECT MAX(stream_seq)` across the whole index — used in the
    /// last-seq regression axis (amendment §G.1).
    pub fn bus_author_seq_index_max_stream_seq(&self) -> Result<Option<u64>> {
        let conn = self.conn();
        conn.query_row(
            "SELECT MAX(stream_seq) FROM bus_author_seq_index",
            [],
            |row| row.get::<_, Option<i64>>(0),
        )
        .map(|opt| opt.map(|i| i as u64))
        .map_err(EgreError::from)
    }

    /// Latest-indexed-at per author — feeds the author-silence metric
    /// (amendment §G.3). Returns one row per author, sorted by
    /// `last_indexed_at DESC` for operator-facing ordering.
    pub fn bus_author_seq_index_latest_author_activity(&self) -> Result<Vec<AuthorActivityRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT author, MAX(indexed_at) FROM bus_author_seq_index
             GROUP BY author
             ORDER BY MAX(indexed_at) DESC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let author: String = row.get(0)?;
                let last_indexed_s: String = row.get(1)?;
                Ok((author, last_indexed_s))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        rows.into_iter()
            .map(|(author, last_indexed_s)| {
                let last_indexed_at =
                    parse_rfc3339(&last_indexed_s).ok_or_else(|| EgreError::Config {
                        reason: format!(
                            "invalid bus_author_seq_index.indexed_at: {last_indexed_s}"
                        ),
                    })?;
                Ok(AuthorActivityRow {
                    author,
                    last_indexed_at,
                })
            })
            .collect()
    }

    /// Per-author summary projecting `(author_seq, stream_seq,
    /// last_indexed_at)` for the author's highest indexed seq pair.
    /// Feeds scry's `/v1/transport/bus/authors` endpoint (amendment
    /// §C.14). Ordered by `last_indexed_at DESC` so operators see the
    /// most recently active authors first.
    pub fn bus_author_seq_index_author_summary(&self) -> Result<Vec<BusAuthorSummaryRow>> {
        let conn = self.conn();
        // The subquery picks the row with the largest author_seq per
        // author; we then join with the outer to read stream_seq +
        // indexed_at from that same row. GROUP BY with MAX(..) +
        // correlated scan keeps things in a single statement.
        let mut stmt = conn.prepare(
            "SELECT i.author, i.author_seq, i.stream_seq, i.indexed_at
             FROM bus_author_seq_index i
             INNER JOIN (
                 SELECT author, MAX(author_seq) AS max_seq
                 FROM bus_author_seq_index
                 GROUP BY author
             ) m ON m.author = i.author AND m.max_seq = i.author_seq
             ORDER BY i.indexed_at DESC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let author: String = row.get(0)?;
                let author_seq_i: i64 = row.get(1)?;
                let stream_seq_i: i64 = row.get(2)?;
                let indexed_s: String = row.get(3)?;
                Ok((author, author_seq_i, stream_seq_i, indexed_s))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        rows.into_iter()
            .map(|(author, aseq, sseq, s)| {
                let last_indexed_at = parse_rfc3339(&s).ok_or_else(|| EgreError::Config {
                    reason: format!("invalid bus_author_seq_index.indexed_at: {s}"),
                })?;
                Ok(BusAuthorSummaryRow {
                    author,
                    last_indexed_at,
                    author_seq: aseq as u64,
                    stream_seq: sseq as u64,
                })
            })
            .collect()
    }

    /// Truncate the index — called on stream-reset detection (amendment
    /// §C.6, §G.1).
    pub fn bus_author_seq_index_clear(&self) -> Result<()> {
        let conn = self.conn();
        conn.execute("DELETE FROM bus_author_seq_index", [])?;
        Ok(())
    }

    // ---- bus_stream_identity ----

    /// Fetch the persisted identity marker (if any) for a stream name.
    pub fn bus_stream_identity_get(&self, stream_name: &str) -> Result<Option<StreamIdentity>> {
        let conn = self.conn();
        let row = conn
            .query_row(
                "SELECT stream_name, first_ts, created_at, recorded_at
                 FROM bus_stream_identity WHERE stream_name = ?1",
                params![stream_name],
                |row| {
                    let name: String = row.get(0)?;
                    let first: String = row.get(1)?;
                    let created: String = row.get(2)?;
                    let recorded: String = row.get(3)?;
                    Ok((name, first, created, recorded))
                },
            )
            .optional()
            .map_err(EgreError::from)?;

        match row {
            None => Ok(None),
            Some((name, first, created, recorded)) => {
                let first_ts = parse_rfc3339(&first).ok_or_else(|| EgreError::Config {
                    reason: format!("invalid bus_stream_identity.first_ts: {first}"),
                })?;
                let created_at = parse_rfc3339(&created).ok_or_else(|| EgreError::Config {
                    reason: format!("invalid bus_stream_identity.created_at: {created}"),
                })?;
                let recorded_at = parse_rfc3339(&recorded).ok_or_else(|| EgreError::Config {
                    reason: format!("invalid bus_stream_identity.recorded_at: {recorded}"),
                })?;
                Ok(Some(StreamIdentity {
                    stream_name: name,
                    first_ts,
                    created_at,
                    recorded_at,
                }))
            }
        }
    }

    /// INSERT OR REPLACE the identity marker. `recorded_at = now`.
    pub fn bus_stream_identity_set(
        &self,
        stream_name: &str,
        first_ts: &DateTime<Utc>,
        created_at: &DateTime<Utc>,
    ) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT OR REPLACE INTO bus_stream_identity
                (stream_name, first_ts, created_at, recorded_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                stream_name,
                first_ts.to_rfc3339(),
                created_at.to_rfc3339(),
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }
}

// rusqlite's `OptionalExtension` is needed for `.optional()` on
// `query_row`. Pull it into scope without re-exporting.
use rusqlite::OptionalExtension;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::store::make_test_message;

    fn store_with_msg() -> (FeedStore, Message) {
        let store = FeedStore::open_memory().unwrap();
        let msg = make_test_message("@alice.ed25519", 1, None);
        (store, msg)
    }

    #[test]
    fn pending_enqueue_is_idempotent() {
        // Amendment §G.2: second call on the same (transport_id, hash)
        // must return AlreadyPresent and leave row count unchanged.
        let (store, msg) = store_with_msg();
        let first = store.pending_forwarding_enqueue("bus", &msg).unwrap();
        let second = store.pending_forwarding_enqueue("bus", &msg).unwrap();
        assert_eq!(first, EnqueueResult::Inserted);
        assert_eq!(second, EnqueueResult::AlreadyPresent);
        assert_eq!(store.pending_forwarding_count("bus").unwrap(), 1);
    }

    #[test]
    fn pending_complete_removes_row() {
        let (store, msg) = store_with_msg();
        store.pending_forwarding_enqueue("bus", &msg).unwrap();
        assert_eq!(store.pending_forwarding_count("bus").unwrap(), 1);

        store.pending_forwarding_complete("bus", &msg.hash).unwrap();
        assert_eq!(store.pending_forwarding_count("bus").unwrap(), 0);
    }

    #[test]
    fn pending_list_paginates_and_orders_oldest_first() {
        let store = FeedStore::open_memory().unwrap();
        for i in 1..=5 {
            let msg = make_test_message("@alice.ed25519", i, None);
            store.pending_forwarding_enqueue("bus", &msg).unwrap();
        }
        let rows = store.pending_forwarding_list("bus", 3).unwrap();
        assert_eq!(rows.len(), 3, "limit must cap returned rows");
        // Oldest-first ordering: sequences 1, 2, 3.
        let seqs: Vec<_> = rows.iter().map(|r| r.sequence).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
    }

    #[test]
    fn pending_record_failure_updates_attempt_count_and_error() {
        let (store, msg) = store_with_msg();
        store.pending_forwarding_enqueue("bus", &msg).unwrap();

        store
            .pending_forwarding_record_failure("bus", &msg.hash, "ENET_UNREACHABLE")
            .unwrap();
        store
            .pending_forwarding_record_failure("bus", &msg.hash, "NATS_TIMEOUT")
            .unwrap();

        let rows = store.pending_forwarding_list("bus", 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].attempt_count, 2);
        assert_eq!(rows[0].last_error.as_deref(), Some("NATS_TIMEOUT"));
        assert!(rows[0].last_attempt_at.is_some());
    }

    #[test]
    fn pending_count_matches_row_count() {
        let store = FeedStore::open_memory().unwrap();
        assert_eq!(store.pending_forwarding_count("bus").unwrap(), 0);

        for i in 1..=7 {
            let msg = make_test_message("@alice.ed25519", i, None);
            store.pending_forwarding_enqueue("bus", &msg).unwrap();
        }
        assert_eq!(store.pending_forwarding_count("bus").unwrap(), 7);

        // Per-transport scoping: gossip count is 0.
        assert_eq!(store.pending_forwarding_count("gossip").unwrap(), 0);
    }

    #[test]
    fn bus_author_seq_index_find_by_author_seq() {
        let store = FeedStore::open_memory().unwrap();
        let author = "@alice.ed25519";

        // Interleave two authors — the index rows for `@alice` at author_seq 3
        // must map to stream_seq 15 regardless of `@bob`'s entries in between.
        store.bus_author_seq_index_insert(author, 1, 10).unwrap();
        store
            .bus_author_seq_index_insert("@bob.ed25519", 1, 11)
            .unwrap();
        store.bus_author_seq_index_insert(author, 2, 12).unwrap();
        store
            .bus_author_seq_index_insert("@bob.ed25519", 2, 13)
            .unwrap();
        store.bus_author_seq_index_insert(author, 3, 15).unwrap();

        // after_seq = 0 → first stream_seq is 10.
        let first = store
            .bus_author_seq_index_find_stream_seq(author, 0)
            .unwrap();
        assert_eq!(first, Some(10));

        // after_seq = 2 → next stream_seq is 15.
        let next = store
            .bus_author_seq_index_find_stream_seq(author, 2)
            .unwrap();
        assert_eq!(next, Some(15));

        // after_seq = 3 → no more → None.
        let head = store
            .bus_author_seq_index_find_stream_seq(author, 3)
            .unwrap();
        assert_eq!(head, None);
    }

    #[test]
    fn bus_author_seq_index_min_max_and_clear() {
        let store = FeedStore::open_memory().unwrap();
        assert_eq!(store.bus_author_seq_index_min_stream_seq().unwrap(), None);
        assert_eq!(store.bus_author_seq_index_max_stream_seq().unwrap(), None);

        store
            .bus_author_seq_index_insert("@a.ed25519", 1, 10)
            .unwrap();
        store
            .bus_author_seq_index_insert("@b.ed25519", 1, 50)
            .unwrap();
        store
            .bus_author_seq_index_insert("@a.ed25519", 2, 22)
            .unwrap();

        assert_eq!(
            store.bus_author_seq_index_min_stream_seq().unwrap(),
            Some(10)
        );
        assert_eq!(
            store.bus_author_seq_index_max_stream_seq().unwrap(),
            Some(50)
        );

        store.bus_author_seq_index_clear().unwrap();
        assert_eq!(store.bus_author_seq_index_min_stream_seq().unwrap(), None);
        assert_eq!(store.bus_author_seq_index_max_stream_seq().unwrap(), None);
    }

    #[test]
    fn bus_stream_identity_upsert_round_trip() {
        let store = FeedStore::open_memory().unwrap();
        let first_ts = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let created = DateTime::parse_from_rfc3339("2026-01-02T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        assert!(store
            .bus_stream_identity_get("egregore-feed")
            .unwrap()
            .is_none());

        store
            .bus_stream_identity_set("egregore-feed", &first_ts, &created)
            .unwrap();

        let got = store
            .bus_stream_identity_get("egregore-feed")
            .unwrap()
            .expect("identity must be persisted");
        assert_eq!(got.stream_name, "egregore-feed");
        assert_eq!(got.first_ts, first_ts);
        assert_eq!(got.created_at, created);

        // UPSERT: setting again with a different first_ts replaces the row.
        let new_first = DateTime::parse_from_rfc3339("2026-02-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        store
            .bus_stream_identity_set("egregore-feed", &new_first, &created)
            .unwrap();
        let got2 = store
            .bus_stream_identity_get("egregore-feed")
            .unwrap()
            .unwrap();
        assert_eq!(got2.first_ts, new_first);
    }
}
