use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::error::{EgreError, Result};
use crate::feed::models::{FeedQuery, Message};
use crate::identity::PublicId;

/// Thread-safe wrapper around a SQLite connection.
/// All access is synchronous; use tokio::task::spawn_blocking from async code.
#[derive(Clone)]
pub struct FeedStore {
    conn: Arc<Mutex<Connection>>,
}

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

    fn init_schema(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "
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
                UNIQUE(author, sequence)
            );

            CREATE INDEX IF NOT EXISTS idx_messages_author_seq
                ON messages(author, sequence);

            CREATE INDEX IF NOT EXISTS idx_messages_content_type
                ON messages(content_type);

            CREATE INDEX IF NOT EXISTS idx_messages_timestamp
                ON messages(timestamp);

            CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
                content_text,
                content='messages',
                content_rowid='rowid'
            );

            CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
                INSERT INTO messages_fts(rowid, content_text)
                VALUES (new.rowid, new.content_json);
            END;

            CREATE TABLE IF NOT EXISTS peers (
                address TEXT PRIMARY KEY,
                public_id TEXT,
                last_connected TEXT,
                last_synced TEXT
            );
            ",
        )?;
        Ok(())
    }

    /// Insert a message. Returns error on duplicate or integrity violation.
    pub fn insert_message(&self, msg: &Message) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let content_type = content_type_name(&msg.content);
        let content_json = serde_json::to_string(&msg.content)?;
        let raw_json = serde_json::to_string(msg)?;

        conn.execute(
            "INSERT INTO messages (hash, author, sequence, previous, timestamp, content_type, content_json, signature, raw_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                msg.hash,
                msg.author.0,
                msg.sequence,
                msg.previous,
                msg.timestamp.to_rfc3339(),
                content_type,
                content_json,
                msg.signature,
                raw_json,
            ],
        )
        .map_err(|e| match e {
            rusqlite::Error::SqliteFailure(err, _)
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                EgreError::DuplicateMessage {
                    author: msg.author.0.clone(),
                    sequence: msg.sequence,
                }
            }
            other => EgreError::Database(other),
        })?;

        // Update feed tracking
        conn.execute(
            "INSERT INTO feeds (author, latest_sequence, last_seen)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(author) DO UPDATE SET
                latest_sequence = MAX(feeds.latest_sequence, excluded.latest_sequence),
                last_seen = excluded.last_seen",
            params![
                msg.author.0,
                msg.sequence,
                msg.timestamp.to_rfc3339(),
            ],
        )?;

        Ok(())
    }

    /// Get a message by its hash.
    pub fn get_message(&self, hash: &str) -> Result<Option<Message>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT raw_json FROM messages WHERE hash = ?1",
            params![hash],
            |row| {
                let json: String = row.get(0)?;
                Ok(json)
            },
        )
        .optional()?
        .map(|json| serde_json::from_str(&json).map_err(EgreError::from))
        .transpose()
    }

    /// Get feed messages with pagination.
    pub fn query_messages(&self, query: &FeedQuery) -> Result<Vec<Message>> {
        let conn = self.conn.lock().unwrap();
        let mut sql = String::from("SELECT raw_json FROM messages WHERE 1=1");
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(ref author) = query.author {
            sql.push_str(&format!(" AND author = ?{}", param_values.len() + 1));
            param_values.push(Box::new(author.0.clone()));
        }

        if let Some(ref ct) = query.content_type {
            sql.push_str(&format!(" AND content_type = ?{}", param_values.len() + 1));
            param_values.push(Box::new(ct.clone()));
        }

        sql.push_str(" ORDER BY timestamp DESC");

        let limit = query.limit.unwrap_or(50).min(200);
        let offset = query.offset.unwrap_or(0);
        sql.push_str(&format!(
            " LIMIT ?{} OFFSET ?{}",
            param_values.len() + 1,
            param_values.len() + 2
        ));
        param_values.push(Box::new(limit));
        param_values.push(Box::new(offset));

        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(|p| p.as_ref()).collect();

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_refs.as_slice(), |row| {
                let json: String = row.get(0)?;
                Ok(json)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        rows.into_iter()
            .map(|json| serde_json::from_str(&json).map_err(EgreError::from))
            .collect()
    }

    /// Full-text search across message content.
    pub fn search_messages(&self, query_text: &str, limit: u32) -> Result<Vec<Message>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT m.raw_json FROM messages m
             INNER JOIN messages_fts f ON m.rowid = f.rowid
             WHERE messages_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2",
        )?;

        let rows = stmt
            .query_map(params![query_text, limit], |row| {
                let json: String = row.get(0)?;
                Ok(json)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        rows.into_iter()
            .map(|json| serde_json::from_str(&json).map_err(EgreError::from))
            .collect()
    }

    /// Get the latest sequence number for a feed.
    pub fn get_latest_sequence(&self, author: &PublicId) -> Result<u64> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT latest_sequence FROM feeds WHERE author = ?1",
            params![author.0],
            |row| row.get(0),
        )
        .optional()
        .map(|opt| opt.unwrap_or(0))
        .map_err(EgreError::from)
    }

    /// Get all known feeds with their latest sequence numbers.
    pub fn get_all_feeds(&self) -> Result<Vec<(PublicId, u64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT author, latest_sequence FROM feeds")?;
        let rows = stmt
            .query_map([], |row| {
                let author: String = row.get(0)?;
                let seq: u64 = row.get(1)?;
                Ok((PublicId(author), seq))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Get messages for a feed starting after a given sequence.
    pub fn get_messages_after(
        &self,
        author: &PublicId,
        after_seq: u64,
        limit: u32,
    ) -> Result<Vec<Message>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT raw_json FROM messages
             WHERE author = ?1 AND sequence > ?2
             ORDER BY sequence ASC
             LIMIT ?3",
        )?;

        let rows = stmt
            .query_map(params![author.0, after_seq, limit], |row| {
                let json: String = row.get(0)?;
                Ok(json)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        rows.into_iter()
            .map(|json| serde_json::from_str(&json).map_err(EgreError::from))
            .collect()
    }
}

fn content_type_name(content: &crate::feed::models::Content) -> &'static str {
    use crate::feed::models::Content;
    match content {
        Content::Insight { .. } => "insight",
        Content::Endorsement { .. } => "endorsement",
        Content::Dispute { .. } => "dispute",
        Content::Query { .. } => "query",
        Content::Response { .. } => "response",
        Content::Profile { .. } => "profile",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::models::Content;
    use chrono::Utc;

    fn make_test_message(author: &str, seq: u64, prev: Option<&str>) -> Message {
        Message {
            author: PublicId(author.to_string()),
            sequence: seq,
            previous: prev.map(|s| s.to_string()),
            timestamp: Utc::now(),
            content: Content::Insight {
                title: format!("Test insight {seq}"),
                context: None,
                observation: "Test observation".to_string(),
                evidence: None,
                guidance: None,
                confidence: Some(0.9),
                tags: vec!["test".to_string()],
            },
            hash: format!("hash_{author}_{seq}"),
            signature: "sig".to_string(),
        }
    }

    #[test]
    fn insert_and_retrieve() {
        let store = FeedStore::open_memory().unwrap();
        let msg = make_test_message("@alice.ed25519", 1, None);
        store.insert_message(&msg).unwrap();

        let retrieved = store.get_message("hash_@alice.ed25519_1").unwrap().unwrap();
        assert_eq!(retrieved.sequence, 1);
        assert_eq!(retrieved.author.0, "@alice.ed25519");
    }

    #[test]
    fn duplicate_rejected() {
        let store = FeedStore::open_memory().unwrap();
        let msg = make_test_message("@alice.ed25519", 1, None);
        store.insert_message(&msg).unwrap();

        let result = store.insert_message(&msg);
        assert!(matches!(result, Err(EgreError::DuplicateMessage { .. })));
    }

    #[test]
    fn feed_sequence_tracking() {
        let store = FeedStore::open_memory().unwrap();
        let m1 = make_test_message("@alice.ed25519", 1, None);
        let m2 = make_test_message("@alice.ed25519", 2, Some("hash_@alice.ed25519_1"));
        store.insert_message(&m1).unwrap();
        store.insert_message(&m2).unwrap();

        let seq = store
            .get_latest_sequence(&PublicId("@alice.ed25519".to_string()))
            .unwrap();
        assert_eq!(seq, 2);
    }

    #[test]
    fn query_by_author() {
        let store = FeedStore::open_memory().unwrap();
        store
            .insert_message(&make_test_message("@alice.ed25519", 1, None))
            .unwrap();
        store
            .insert_message(&make_test_message("@bob.ed25519", 1, None))
            .unwrap();

        let query = FeedQuery {
            author: Some(PublicId("@alice.ed25519".to_string())),
            ..Default::default()
        };
        let results = store.query_messages(&query).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn get_messages_after_sequence() {
        let store = FeedStore::open_memory().unwrap();
        for i in 1..=5 {
            let prev = if i == 1 {
                None
            } else {
                Some(format!("hash_@alice.ed25519_{}", i - 1))
            };
            store
                .insert_message(&make_test_message(
                    "@alice.ed25519",
                    i,
                    prev.as_deref(),
                ))
                .unwrap();
        }

        let msgs = store
            .get_messages_after(&PublicId("@alice.ed25519".to_string()), 3, 10)
            .unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].sequence, 4);
        assert_eq!(msgs[1].sequence, 5);
    }

    #[test]
    fn full_text_search() {
        let store = FeedStore::open_memory().unwrap();
        let mut msg = make_test_message("@alice.ed25519", 1, None);
        msg.content = Content::Insight {
            title: "Unique pattern".to_string(),
            context: None,
            observation: "Egregore decentralized replication".to_string(),
            evidence: None,
            guidance: None,
            confidence: Some(0.9),
            tags: vec![],
        };
        store.insert_message(&msg).unwrap();

        let results = store.search_messages("decentralized", 10).unwrap();
        assert_eq!(results.len(), 1);
    }
}
