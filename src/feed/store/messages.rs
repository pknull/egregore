//! Message storage — insert, query, search, chain validation tracking.
//!
//! `chain_valid` flag: true means the immediate backward link to the predecessor
//! was verified at ingest time. False means the predecessor was missing (gap).
//! Backfill promotes the flag when the missing predecessor arrives.

use rusqlite::{params, OptionalExtension, TransactionBehavior};

use crate::error::{EgreError, Result};
use crate::feed::models::{FeedQuery, Message};
use crate::identity::PublicId;

use super::{content_type_name, FeedStore};

impl FeedStore {
    // ---- Message operations ----

    /// Atomically allocate a sequence number, build, and insert a message.
    ///
    /// Uses `TransactionBehavior::Immediate` to acquire a write lock before
    /// reading the current sequence, preventing race conditions where two
    /// concurrent publish calls could read the same sequence number.
    ///
    /// The `builder` closure receives the allocated sequence number and
    /// previous hash, and must return the fully constructed (hashed + signed)
    /// message.
    pub fn publish_message_atomic<F>(&self, author: &PublicId, builder: F) -> Result<Message>
    where
        F: FnOnce(u64, Option<String>) -> Result<Message>,
    {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        // Get latest sequence within transaction (locked)
        let latest_seq: u64 = tx
            .query_row(
                "SELECT COALESCE(MAX(sequence), 0) FROM messages WHERE author = ?1",
                params![author.0],
                |row| row.get(0),
            )
            .unwrap_or(0);

        let new_seq = latest_seq.checked_add(1).ok_or_else(|| EgreError::FeedIntegrity {
            reason: "sequence number overflow".into(),
        })?;

        // Get previous hash if needed
        let previous: Option<String> = if new_seq > 1 {
            tx.query_row(
                "SELECT hash FROM messages WHERE author = ?1 AND sequence = ?2",
                params![author.0, latest_seq],
                |row| row.get(0),
            )
            .optional()?
        } else {
            None
        };

        // Build the message (computes hash, signs)
        let message = builder(new_seq, previous)?;

        // Insert within same transaction
        let content_type = content_type_name(&message.content);
        let content_json = serde_json::to_string(&message.content)?;
        let raw_json = serde_json::to_string(&message)?;

        tx.execute(
            "INSERT INTO messages (hash, author, sequence, previous, timestamp, content_type, content_json, signature, raw_json, chain_valid, relates)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                message.hash,
                message.author.0,
                message.sequence,
                message.previous,
                message.timestamp.to_rfc3339(),
                content_type,
                content_json,
                message.signature,
                raw_json,
                1i32, // chain_valid = true for locally published messages
                message.relates,
            ],
        )
        .map_err(|e| match e {
            rusqlite::Error::SqliteFailure(err, _)
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                EgreError::DuplicateMessage {
                    author: message.author.0.clone(),
                    sequence: message.sequence,
                }
            }
            other => EgreError::Database(other),
        })?;

        // Insert tags
        for tag in &message.tags {
            tx.execute(
                "INSERT OR IGNORE INTO message_tags (message_hash, tag) VALUES (?1, ?2)",
                params![message.hash, tag],
            )?;
        }

        // Update feed tracking
        tx.execute(
            "INSERT INTO feeds (author, latest_sequence, last_seen)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(author) DO UPDATE SET
                latest_sequence = MAX(feeds.latest_sequence, excluded.latest_sequence),
                last_seen = excluded.last_seen",
            params![
                message.author.0,
                message.sequence,
                message.timestamp.to_rfc3339(),
            ],
        )?;

        tx.commit()?;
        Ok(message)
    }

    /// Insert a message. `chain_valid` indicates whether the hash chain
    /// linkage to the predecessor has been verified.
    pub fn insert_message(&self, msg: &Message, chain_valid: bool) -> Result<()> {
        let conn = self.conn();
        let content_type = content_type_name(&msg.content);
        let content_json = serde_json::to_string(&msg.content)?;
        let raw_json = serde_json::to_string(msg)?;

        conn.execute(
            "INSERT INTO messages (hash, author, sequence, previous, timestamp, content_type, content_json, signature, raw_json, chain_valid, relates)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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
                chain_valid as i32,
                msg.relates,
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

        // Insert tags
        for tag in &msg.tags {
            conn.execute(
                "INSERT OR IGNORE INTO message_tags (message_hash, tag) VALUES (?1, ?2)",
                params![msg.hash, tag],
            )?;
        }

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

    /// Update the chain_valid flag for a message identified by hash.
    pub fn set_chain_valid(&self, hash: &str, chain_valid: bool) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE messages SET chain_valid = ?1 WHERE hash = ?2",
            params![chain_valid as i32, hash],
        )?;
        Ok(())
    }

    /// Check if a message's chain is validated.
    pub fn is_chain_valid(&self, hash: &str) -> Result<bool> {
        let conn = self.conn();
        conn.query_row(
            "SELECT chain_valid FROM messages WHERE hash = ?1",
            params![hash],
            |row| {
                let valid: i32 = row.get(0)?;
                Ok(valid != 0)
            },
        )
        .optional()
        .map(|opt| opt.unwrap_or(false))
        .map_err(EgreError::from)
    }

    /// Get a message by its hash.
    pub fn get_message(&self, hash: &str) -> Result<Option<Message>> {
        let conn = self.conn();
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
        let conn = self.conn();
        let mut sql = String::from("SELECT DISTINCT m.raw_json FROM messages m");
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        // Join with message_tags if filtering by tag
        if query.tag.is_some() {
            sql.push_str(" INNER JOIN message_tags t ON m.hash = t.message_hash");
        }

        sql.push_str(" WHERE 1=1");

        if let Some(ref author) = query.author {
            sql.push_str(&format!(" AND m.author = ?{}", param_values.len() + 1));
            param_values.push(Box::new(author.0.clone()));
        }

        if let Some(ref exclude) = query.exclude_author {
            sql.push_str(&format!(" AND m.author != ?{}", param_values.len() + 1));
            param_values.push(Box::new(exclude.0.clone()));
        }

        if let Some(ref ct) = query.content_type {
            sql.push_str(&format!(" AND m.content_type = ?{}", param_values.len() + 1));
            param_values.push(Box::new(ct.clone()));
        }

        if let Some(ref relates) = query.relates {
            sql.push_str(&format!(" AND m.relates = ?{}", param_values.len() + 1));
            param_values.push(Box::new(relates.clone()));
        }

        if let Some(ref tag) = query.tag {
            sql.push_str(&format!(" AND t.tag = ?{}", param_values.len() + 1));
            param_values.push(Box::new(tag.clone()));
        }

        sql.push_str(" ORDER BY m.timestamp DESC");

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
    /// The query text is wrapped in double-quotes to force FTS5 phrase matching
    /// and prevent operator injection.
    pub fn search_messages(&self, query_text: &str, limit: u32) -> Result<Vec<Message>> {
        let trimmed = query_text.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        // Escape embedded double-quotes and wrap as FTS5 phrase literal
        let sanitized = format!("\"{}\"", trimmed.replace('"', "\"\""));

        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT m.raw_json FROM messages m
             INNER JOIN messages_fts f ON m.rowid = f.rowid
             WHERE messages_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2",
        )?;

        let rows = stmt
            .query_map(params![sanitized, limit], |row| {
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
        let conn = self.conn();
        conn.query_row(
            "SELECT latest_sequence FROM feeds WHERE author = ?1",
            params![author.0],
            |row| row.get(0),
        )
        .optional()
        .map(|opt| opt.unwrap_or(0))
        .map_err(EgreError::from)
    }

    /// Get the highest contiguous sequence starting from 1 for an author.
    /// If sequences are [1,2,3,5,6], returns 3 (gap at 4).
    /// If sequences are [2,3], returns 0 (missing sequence 1).
    /// Used by replication to detect gaps that need backfilling.
    ///
    /// Uses SQL gap detection (index-backed) instead of loading all sequences.
    pub fn get_latest_contiguous_sequence(&self, author: &PublicId) -> Result<u64> {
        let conn = self.conn();
        conn.query_row(
            "SELECT CASE
                WHEN NOT EXISTS (
                    SELECT 1 FROM messages WHERE author = ?1 AND sequence = 1
                ) THEN 0
                ELSE COALESCE(
                    (SELECT m1.sequence FROM messages m1
                     WHERE m1.author = ?1
                     AND NOT EXISTS (
                         SELECT 1 FROM messages m2
                         WHERE m2.author = ?1 AND m2.sequence = m1.sequence + 1
                     )
                     ORDER BY m1.sequence ASC
                     LIMIT 1),
                    0
                )
            END",
            params![author.0],
            |row| row.get(0),
        )
        .map_err(EgreError::from)
    }

    /// Get all known feeds with their latest sequence numbers.
    pub fn get_all_feeds(&self) -> Result<Vec<(PublicId, u64)>> {
        let conn = self.conn();
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

    /// Get a single message by author and sequence number.
    pub fn get_message_at_sequence(
        &self,
        author: &PublicId,
        sequence: u64,
    ) -> Result<Option<Message>> {
        let conn = self.conn();
        conn.query_row(
            "SELECT raw_json FROM messages WHERE author = ?1 AND sequence = ?2",
            params![author.0, sequence],
            |row| {
                let json: String = row.get(0)?;
                Ok(json)
            },
        )
        .optional()?
        .map(|json| serde_json::from_str(&json).map_err(EgreError::from))
        .transpose()
    }

    /// Get messages for a feed starting after a given sequence.
    pub fn get_messages_after(
        &self,
        author: &PublicId,
        after_seq: u64,
        limit: u32,
    ) -> Result<Vec<Message>> {
        let conn = self.conn();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::store::make_test_message;

    #[test]
    fn insert_and_retrieve() {
        let store = FeedStore::open_memory().unwrap();
        let msg = make_test_message("@alice.ed25519", 1, None);
        store.insert_message(&msg, true).unwrap();

        let retrieved = store.get_message("hash_@alice.ed25519_1").unwrap().unwrap();
        assert_eq!(retrieved.sequence, 1);
        assert_eq!(retrieved.author.0, "@alice.ed25519");
    }

    #[test]
    fn duplicate_rejected() {
        let store = FeedStore::open_memory().unwrap();
        let msg = make_test_message("@alice.ed25519", 1, None);
        store.insert_message(&msg, true).unwrap();

        let result = store.insert_message(&msg, true);
        assert!(matches!(result, Err(EgreError::DuplicateMessage { .. })));
    }

    #[test]
    fn feed_sequence_tracking() {
        let store = FeedStore::open_memory().unwrap();
        let m1 = make_test_message("@alice.ed25519", 1, None);
        let m2 = make_test_message("@alice.ed25519", 2, Some("hash_@alice.ed25519_1"));
        store.insert_message(&m1, true).unwrap();
        store.insert_message(&m2, true).unwrap();

        let seq = store
            .get_latest_sequence(&PublicId("@alice.ed25519".to_string()))
            .unwrap();
        assert_eq!(seq, 2);
    }

    #[test]
    fn query_by_author() {
        let store = FeedStore::open_memory().unwrap();
        store
            .insert_message(&make_test_message("@alice.ed25519", 1, None), true)
            .unwrap();
        store
            .insert_message(&make_test_message("@bob.ed25519", 1, None), true)
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
                .insert_message(
                    &make_test_message("@alice.ed25519", i, prev.as_deref()),
                    true,
                )
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
        msg.content = serde_json::json!({
            "type": "insight",
            "title": "Unique pattern",
            "observation": "Egregore decentralized replication",
            "confidence": 0.9,
            "tags": [],
        });
        store.insert_message(&msg, true).unwrap();

        let results = store.search_messages("decentralized", 10).unwrap();
        assert_eq!(results.len(), 1);
    }
}
