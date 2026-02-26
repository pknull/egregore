//! SQLite storage — messages, feeds, peers, follows, and FTS5 search.
//!
//! All access is synchronous (rusqlite). Async callers use `spawn_blocking`.
//!
//! Peer storage uses two tables:
//! - `peers`: address-keyed, from manual add or LAN discovery. Identity unknown
//!   until first successful SHS handshake backfills public_id.
//! - `known_peers`: public_id-keyed, from completed handshakes. Tracks
//!   authorization, privacy, timestamps.
//!
//! Both contribute to the sync loop's peer list via `list_all_syncable_addresses()`
//! (SQL UNION, deduplicated).

mod health;
mod messages;
mod peers;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::error::{EgreError, Result};
use crate::identity::PublicId;

/// Thread-safe wrapper around a SQLite connection.
/// All access is synchronous; use tokio::task::spawn_blocking from async code.
#[derive(Clone)]
pub struct FeedStore {
    conn: Arc<Mutex<Connection>>,
}

/// An address-only peer record (from manual add or LAN discovery).
/// Public ID is unknown until SHS handshake succeeds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddressPeer {
    pub address: String,
    pub public_id: Option<String>,
    pub last_connected: Option<DateTime<Utc>>,
    pub last_synced: Option<DateTime<Utc>>,
}

/// A known peer record for persistent peer tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerRecord {
    pub public_id: PublicId,
    pub address: Option<String>,
    pub nickname: Option<String>,
    pub private: bool,
    pub authorized: bool,
    pub first_seen: DateTime<Utc>,
    pub last_connected: Option<DateTime<Utc>>,
    pub last_synced: Option<DateTime<Utc>>,
}

const SCHEMA_DDL: &str = "
    CREATE TABLE IF NOT EXISTS feeds (
        author TEXT PRIMARY KEY,
        latest_sequence INTEGER NOT NULL DEFAULT 0,
        last_seen TEXT
    );
    CREATE TABLE IF NOT EXISTS messages (
        hash TEXT PRIMARY KEY,
        author TEXT NOT NULL,
        sequence INTEGER NOT NULL,
        previous TEXT,
        timestamp TEXT NOT NULL,
        content_type TEXT NOT NULL,
        content_json TEXT NOT NULL,
        signature TEXT NOT NULL,
        raw_json TEXT NOT NULL,
        chain_valid INTEGER NOT NULL DEFAULT 1,
        relates TEXT,
        UNIQUE(author, sequence)
    );
    CREATE INDEX IF NOT EXISTS idx_messages_author_seq ON messages(author, sequence);
    CREATE INDEX IF NOT EXISTS idx_messages_content_type ON messages(content_type);
    CREATE INDEX IF NOT EXISTS idx_messages_timestamp ON messages(timestamp);
    CREATE INDEX IF NOT EXISTS idx_messages_relates ON messages(relates);
    CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
        content_text, content='messages', content_rowid='rowid'
    );
    CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
        INSERT INTO messages_fts(rowid, content_text) VALUES (new.rowid, new.content_json);
    END;
    CREATE TRIGGER IF NOT EXISTS messages_bd BEFORE DELETE ON messages BEGIN
        INSERT INTO messages_fts(messages_fts, rowid, content_text)
        VALUES ('delete', old.rowid, old.content_json);
    END;
    CREATE TABLE IF NOT EXISTS message_tags (
        message_hash TEXT NOT NULL,
        tag TEXT NOT NULL,
        PRIMARY KEY (message_hash, tag),
        FOREIGN KEY (message_hash) REFERENCES messages(hash) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS idx_message_tags_tag ON message_tags(tag);
    CREATE TABLE IF NOT EXISTS peers (
        address TEXT PRIMARY KEY, public_id TEXT, last_connected TEXT, last_synced TEXT
    );
    CREATE TABLE IF NOT EXISTS known_peers (
        public_id TEXT PRIMARY KEY, address TEXT, nickname TEXT,
        private INTEGER NOT NULL DEFAULT 0, authorized INTEGER NOT NULL DEFAULT 1,
        first_seen TEXT NOT NULL, last_connected TEXT, last_synced TEXT
    );
    CREATE TABLE IF NOT EXISTS follows (author TEXT PRIMARY KEY);
    CREATE TABLE IF NOT EXISTS peer_health (
        peer_id TEXT NOT NULL,
        last_seen_at TEXT NOT NULL,
        last_seen_by TEXT NOT NULL,
        last_seq INTEGER NOT NULL DEFAULT 0,
        generation INTEGER NOT NULL DEFAULT 0,
        updated_at TEXT NOT NULL,
        PRIMARY KEY (peer_id, last_seen_by)
    );
    CREATE INDEX IF NOT EXISTS idx_peer_health_peer ON peer_health(peer_id);
    CREATE INDEX IF NOT EXISTS idx_peer_health_updated ON peer_health(updated_at);
    CREATE TABLE IF NOT EXISTS local_state (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
";

impl FeedStore {
    /// Open (or create) the database at the given path.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.init_schema()?;
        Ok(store)
    }

    /// Open an in-memory database (for tests).
    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.init_schema()?;
        Ok(store)
    }

    /// Acquire the database connection, recovering from mutex poisoning.
    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn init_schema(&self) -> Result<()> {
        let conn = self.conn();
        conn.execute_batch(SCHEMA_DDL)?;
        Self::run_migrations(&conn)?;
        Ok(())
    }

    fn run_migrations(conn: &Connection) -> Result<()> {
        // Migration: add chain_valid column
        let has_chain_valid: bool = conn
            .prepare("SELECT chain_valid FROM messages LIMIT 0")
            .is_ok();
        if !has_chain_valid {
            conn.execute_batch(
                "ALTER TABLE messages ADD COLUMN chain_valid INTEGER NOT NULL DEFAULT 1;",
            )?;
        }

        // Migration: add peer_health table
        let has_peer_health: bool = conn
            .prepare("SELECT 1 FROM peer_health LIMIT 0")
            .is_ok();
        if !has_peer_health {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS peer_health (
                    peer_id TEXT NOT NULL,
                    last_seen_at TEXT NOT NULL,
                    last_seen_by TEXT NOT NULL,
                    last_seq INTEGER NOT NULL DEFAULT 0,
                    generation INTEGER NOT NULL DEFAULT 0,
                    updated_at TEXT NOT NULL,
                    PRIMARY KEY (peer_id, last_seen_by)
                );
                CREATE INDEX IF NOT EXISTS idx_peer_health_peer ON peer_health(peer_id);
                CREATE INDEX IF NOT EXISTS idx_peer_health_updated ON peer_health(updated_at);",
            )?;
        }

        // Migration: add local_state table
        let has_local_state: bool = conn
            .prepare("SELECT 1 FROM local_state LIMIT 0")
            .is_ok();
        if !has_local_state {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS local_state (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );",
            )?;
        }

        // Migration: add relates column to messages
        let has_relates: bool = conn
            .prepare("SELECT relates FROM messages LIMIT 0")
            .is_ok();
        if !has_relates {
            conn.execute_batch(
                "ALTER TABLE messages ADD COLUMN relates TEXT;
                 CREATE INDEX IF NOT EXISTS idx_messages_relates ON messages(relates);",
            )?;
        }

        // Migration: add message_tags table
        let has_message_tags: bool = conn
            .prepare("SELECT 1 FROM message_tags LIMIT 0")
            .is_ok();
        if !has_message_tags {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS message_tags (
                    message_hash TEXT NOT NULL,
                    tag TEXT NOT NULL,
                    PRIMARY KEY (message_hash, tag),
                    FOREIGN KEY (message_hash) REFERENCES messages(hash) ON DELETE CASCADE
                );
                CREATE INDEX IF NOT EXISTS idx_message_tags_tag ON message_tags(tag);",
            )?;
        }

        Ok(())
    }

    // ---- Follow operations ----

    pub fn add_follow(&self, author: &PublicId) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT OR IGNORE INTO follows (author) VALUES (?1)",
            params![author.0],
        )?;
        Ok(())
    }

    pub fn remove_follow(&self, author: &PublicId) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "DELETE FROM follows WHERE author = ?1",
            params![author.0],
        )?;
        Ok(())
    }

    pub fn get_follows(&self) -> Result<Vec<PublicId>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT author FROM follows")?;
        let rows = stmt
            .query_map([], |row| {
                let author: String = row.get(0)?;
                Ok(PublicId(author))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn is_following(&self, author: &PublicId) -> Result<bool> {
        let conn = self.conn();
        conn.query_row(
            "SELECT COUNT(*) FROM follows WHERE author = ?1",
            params![author.0],
            |row| {
                let count: i64 = row.get(0)?;
                Ok(count > 0)
            },
        )
        .map_err(EgreError::from)
    }

    // ---- Metrics ----

    pub fn message_count(&self) -> Result<u64> {
        let conn = self.conn();
        conn.query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .map_err(EgreError::from)
    }

    pub fn feed_count(&self) -> Result<u64> {
        let conn = self.conn();
        conn.query_row("SELECT COUNT(*) FROM feeds", [], |row| row.get(0))
            .map_err(EgreError::from)
    }

    pub fn peer_count(&self) -> Result<u64> {
        let conn = self.conn();
        conn.query_row(
            "SELECT COUNT(*) FROM (
                SELECT address FROM peers WHERE address IS NOT NULL
                UNION
                SELECT address FROM known_peers WHERE authorized = 1 AND address IS NOT NULL
            )",
            [],
            |row| row.get(0),
        )
        .map_err(EgreError::from)
    }

    pub fn follow_count(&self) -> Result<u64> {
        let conn = self.conn();
        conn.query_row("SELECT COUNT(*) FROM follows", [], |row| row.get(0))
            .map_err(EgreError::from)
    }

    // ---- Eviction ----

    /// Delete messages older than `cutoff`. Returns the number of evicted messages.
    /// Runs atomically in a transaction to prevent inconsistent state on crash.
    pub fn evict_before(&self, cutoff: &DateTime<Utc>) -> Result<u64> {
        let mut conn = self.conn();
        let cutoff_str = cutoff.to_rfc3339();

        let tx = conn.transaction()?;

        let count = tx.execute(
            "DELETE FROM messages WHERE timestamp < ?1",
            params![cutoff_str],
        )? as u64;

        if count > 0 {
            // Mark the earliest remaining message per author as chain_valid=false
            // if it has a previous hash pointing to a now-deleted message
            tx.execute(
                "UPDATE messages SET chain_valid = 0
                 WHERE previous IS NOT NULL
                 AND (author, sequence) IN (
                     SELECT author, MIN(sequence) FROM messages GROUP BY author
                 )",
                [],
            )?;

            // Update latest_sequence to match actual remaining messages
            tx.execute(
                "UPDATE feeds SET latest_sequence = (
                    SELECT COALESCE(MAX(sequence), 0) FROM messages WHERE messages.author = feeds.author
                )",
                [],
            )?;
            // Clean up orphaned feed entries (feeds with no remaining messages)
            tx.execute(
                "DELETE FROM feeds WHERE author NOT IN (SELECT DISTINCT author FROM messages)",
                [],
            )?;
        }

        tx.commit()?;
        Ok(count)
    }
}

pub(crate) fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .ok()
}

fn content_type_name(content: &serde_json::Value) -> String {
    content
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string()
}

fn parse_peer_row(row: &rusqlite::Row) -> Result<PeerRecord> {
    let public_id: String = row.get(0).map_err(EgreError::Database)?;
    let address: Option<String> = row.get(1).map_err(EgreError::Database)?;
    let nickname: Option<String> = row.get(2).map_err(EgreError::Database)?;
    let private: i32 = row.get(3).map_err(EgreError::Database)?;
    let authorized: i32 = row.get(4).map_err(EgreError::Database)?;
    let first_seen_str: String = row.get(5).map_err(EgreError::Database)?;
    let last_connected_str: Option<String> = row.get(6).map_err(EgreError::Database)?;
    let last_synced_str: Option<String> = row.get(7).map_err(EgreError::Database)?;

    let first_seen = parse_rfc3339(&first_seen_str).ok_or_else(|| EgreError::Config {
        reason: format!("invalid first_seen timestamp '{}'", first_seen_str),
    })?;

    let last_connected = last_connected_str.and_then(|s| parse_rfc3339(&s));

    let last_synced = last_synced_str.and_then(|s| parse_rfc3339(&s));

    Ok(PeerRecord {
        public_id: PublicId(public_id),
        address,
        nickname,
        private: private != 0,
        authorized: authorized != 0,
        first_seen,
        last_connected,
        last_synced,
    })
}

#[cfg(test)]
pub(crate) fn make_test_message(
    author: &str,
    seq: u64,
    prev: Option<&str>,
) -> crate::feed::models::Message {
    use crate::feed::models::Message;
    Message {
        author: PublicId(author.to_string()),
        sequence: seq,
        previous: prev.map(|s| s.to_string()),
        timestamp: Utc::now(),
        content: serde_json::json!({
            "type": "insight",
            "title": format!("Test insight {seq}"),
            "observation": "Test observation",
            "confidence": 0.9,
        }),
        relates: None,
        tags: vec![],
        hash: format!("hash_{author}_{seq}"),
        signature: "sig".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follow_crud() {
        let store = FeedStore::open_memory().unwrap();
        let author = PublicId("@alice.ed25519".to_string());

        assert!(!store.is_following(&author).unwrap());
        assert_eq!(store.follow_count().unwrap(), 0);

        store.add_follow(&author).unwrap();
        assert!(store.is_following(&author).unwrap());
        assert_eq!(store.follow_count().unwrap(), 1);

        let follows = store.get_follows().unwrap();
        assert_eq!(follows.len(), 1);
        assert_eq!(follows[0], author);

        store.remove_follow(&author).unwrap();
        assert!(!store.is_following(&author).unwrap());
        assert_eq!(store.follow_count().unwrap(), 0);
    }

    #[test]
    fn follow_idempotent() {
        let store = FeedStore::open_memory().unwrap();
        let author = PublicId("@alice.ed25519".to_string());

        store.add_follow(&author).unwrap();
        store.add_follow(&author).unwrap(); // no error
        assert_eq!(store.follow_count().unwrap(), 1);
    }

    #[test]
    fn metrics() {
        let store = FeedStore::open_memory().unwrap();
        assert_eq!(store.message_count().unwrap(), 0);
        assert_eq!(store.feed_count().unwrap(), 0);
        assert_eq!(store.peer_count().unwrap(), 0);

        store
            .insert_message(&make_test_message("@alice.ed25519", 1, None), true)
            .unwrap();
        assert_eq!(store.message_count().unwrap(), 1);
        assert_eq!(store.feed_count().unwrap(), 1);
    }

    #[test]
    fn evict_old_messages() {
        let store = FeedStore::open_memory().unwrap();

        // Insert a message with old timestamp
        let mut old_msg = make_test_message("@old.ed25519", 1, None);
        old_msg.timestamp = Utc::now() - chrono::Duration::days(60);
        store.insert_message(&old_msg, true).unwrap();

        // Insert a recent message
        store
            .insert_message(&make_test_message("@new.ed25519", 1, None), true)
            .unwrap();

        assert_eq!(store.message_count().unwrap(), 2);

        let cutoff = Utc::now() - chrono::Duration::days(30);
        let evicted = store.evict_before(&cutoff).unwrap();
        assert_eq!(evicted, 1);
        assert_eq!(store.message_count().unwrap(), 1);
        assert_eq!(store.feed_count().unwrap(), 1);
    }
}
