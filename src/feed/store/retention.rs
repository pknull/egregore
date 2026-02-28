//! Retention policies and message expiration.
//!
//! Supports:
//! - Per-message TTL via `expires_at` field
//! - Policy-based retention (time, count, size)
//! - Compaction (keep latest per key)
//! - Tombstone tracking for deleted messages

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Scope for a retention policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionScope {
    /// Applies to all messages.
    Global,
    /// Applies to messages with a specific topic/tag.
    Topic(String),
    /// Applies to messages from a specific author.
    Author(String),
    /// Applies to messages with a specific content type.
    ContentType(String),
}

impl RetentionScope {
    fn scope_type(&self) -> &'static str {
        match self {
            RetentionScope::Global => "global",
            RetentionScope::Topic(_) => "topic",
            RetentionScope::Author(_) => "author",
            RetentionScope::ContentType(_) => "content_type",
        }
    }

    fn scope_value(&self) -> Option<&str> {
        match self {
            RetentionScope::Global => None,
            RetentionScope::Topic(v) => Some(v),
            RetentionScope::Author(v) => Some(v),
            RetentionScope::ContentType(v) => Some(v),
        }
    }
}

/// A retention policy defining when messages should be removed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionPolicy {
    /// Policy identifier (database-assigned).
    #[serde(default)]
    pub id: Option<i64>,
    /// What this policy applies to.
    pub scope: RetentionScope,
    /// Maximum age in seconds. Messages older than this are eligible for deletion.
    #[serde(default)]
    pub max_age_secs: Option<u64>,
    /// Maximum number of messages to keep (per scope).
    #[serde(default)]
    pub max_count: Option<u64>,
    /// Maximum total size in bytes (per scope).
    #[serde(default)]
    pub max_bytes: Option<u64>,
    /// If set, keep only the latest message per this JSON path value (compaction).
    /// E.g., "$.key" keeps only the latest message for each unique value of content.key.
    #[serde(default)]
    pub compact_key: Option<String>,
}

impl RetentionPolicy {
    /// Create a global time-based retention policy.
    pub fn global_max_age(secs: u64) -> Self {
        Self {
            id: None,
            scope: RetentionScope::Global,
            max_age_secs: Some(secs),
            max_count: None,
            max_bytes: None,
            compact_key: None,
        }
    }

    /// Create a topic-scoped retention policy.
    pub fn for_topic(topic: impl Into<String>) -> RetentionPolicyBuilder {
        RetentionPolicyBuilder {
            scope: RetentionScope::Topic(topic.into()),
            max_age_secs: None,
            max_count: None,
            max_bytes: None,
            compact_key: None,
        }
    }
}

/// Builder for retention policies.
pub struct RetentionPolicyBuilder {
    scope: RetentionScope,
    max_age_secs: Option<u64>,
    max_count: Option<u64>,
    max_bytes: Option<u64>,
    compact_key: Option<String>,
}

impl RetentionPolicyBuilder {
    pub fn max_age_secs(mut self, secs: u64) -> Self {
        self.max_age_secs = Some(secs);
        self
    }

    pub fn max_count(mut self, count: u64) -> Self {
        self.max_count = Some(count);
        self
    }

    pub fn max_bytes(mut self, bytes: u64) -> Self {
        self.max_bytes = Some(bytes);
        self
    }

    pub fn compact_key(mut self, key: impl Into<String>) -> Self {
        self.compact_key = Some(key.into());
        self
    }

    pub fn build(self) -> RetentionPolicy {
        RetentionPolicy {
            id: None,
            scope: self.scope,
            max_age_secs: self.max_age_secs,
            max_count: self.max_count,
            max_bytes: self.max_bytes,
            compact_key: self.compact_key,
        }
    }
}

/// Reason a message was deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletionReason {
    /// Message had an expires_at that passed.
    Expired,
    /// Message was removed by retention policy.
    Retention,
    /// Message was removed by compaction (newer value exists).
    Compacted,
}

impl DeletionReason {
    #[allow(dead_code)]
    fn as_str(&self) -> &'static str {
        match self {
            DeletionReason::Expired => "expired",
            DeletionReason::Retention => "retention",
            DeletionReason::Compacted => "compacted",
        }
    }
}

/// Result of a cleanup operation.
#[derive(Debug, Default)]
pub struct CleanupResult {
    /// Number of messages deleted due to expiration.
    pub expired: usize,
    /// Number of messages deleted due to retention policies.
    pub retention: usize,
    /// Number of messages deleted due to compaction.
    pub compacted: usize,
}

impl CleanupResult {
    pub fn total(&self) -> usize {
        self.expired + self.retention + self.compacted
    }
}

/// Retention operations on the database.
pub struct RetentionOps<'a> {
    conn: &'a Connection,
}

impl<'a> RetentionOps<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// Save a retention policy to the database.
    pub fn save_policy(&self, policy: &RetentionPolicy) -> Result<i64> {
        self.conn.execute(
            "INSERT OR REPLACE INTO retention_policies
             (scope_type, scope_value, max_age_secs, max_count, max_bytes, compact_key, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                policy.scope.scope_type(),
                policy.scope.scope_value(),
                policy.max_age_secs.map(|v| v as i64),
                policy.max_count.map(|v| v as i64),
                policy.max_bytes.map(|v| v as i64),
                policy.compact_key,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// List all retention policies.
    pub fn list_policies(&self) -> Result<Vec<RetentionPolicy>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, scope_type, scope_value, max_age_secs, max_count, max_bytes, compact_key
             FROM retention_policies",
        )?;

        let policies = stmt
            .query_map([], |row| {
                let scope_type: String = row.get(1)?;
                let scope_value: Option<String> = row.get(2)?;
                let scope = match scope_type.as_str() {
                    "global" => RetentionScope::Global,
                    "topic" => RetentionScope::Topic(scope_value.unwrap_or_default()),
                    "author" => RetentionScope::Author(scope_value.unwrap_or_default()),
                    "content_type" => RetentionScope::ContentType(scope_value.unwrap_or_default()),
                    _ => RetentionScope::Global,
                };

                Ok(RetentionPolicy {
                    id: Some(row.get(0)?),
                    scope,
                    max_age_secs: row.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                    max_count: row.get::<_, Option<i64>>(4)?.map(|v| v as u64),
                    max_bytes: row.get::<_, Option<i64>>(5)?.map(|v| v as u64),
                    compact_key: row.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(policies)
    }

    /// Delete a retention policy by ID.
    pub fn delete_policy(&self, id: i64) -> Result<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM retention_policies WHERE id = ?1",
            params![id],
        )?;
        Ok(deleted > 0)
    }

    /// Run cleanup: delete expired messages and apply retention policies.
    pub fn run_cleanup(&self) -> Result<CleanupResult> {
        let mut result = CleanupResult::default();
        let now = Utc::now();

        // Phase 1: Delete messages with passed expires_at
        result.expired = self.cleanup_expired(&now)?;

        // Phase 2: Apply retention policies
        let policies = self.list_policies()?;
        for policy in &policies {
            let (retention, compacted) = self.apply_policy(policy, &now)?;
            result.retention += retention;
            result.compacted += compacted;
        }

        Ok(result)
    }

    /// Delete messages where expires_at < now.
    fn cleanup_expired(&self, now: &DateTime<Utc>) -> Result<usize> {
        let now_str = now.to_rfc3339();

        // First, record tombstones for deleted messages
        self.conn.execute(
            "INSERT INTO tombstones (hash, author, sequence, deleted_at, reason)
             SELECT hash, author, sequence, ?1, 'expired'
             FROM messages
             WHERE expires_at IS NOT NULL AND expires_at < ?2",
            params![now_str, now_str],
        )?;

        // Then delete the messages
        let deleted = self.conn.execute(
            "DELETE FROM messages WHERE expires_at IS NOT NULL AND expires_at < ?1",
            params![now_str],
        )?;

        Ok(deleted)
    }

    /// Apply a single retention policy.
    /// Returns (retention_deleted, compacted_deleted).
    fn apply_policy(&self, policy: &RetentionPolicy, now: &DateTime<Utc>) -> Result<(usize, usize)> {
        let mut retention_deleted = 0;
        let mut compacted_deleted = 0;

        // Time-based retention (0 = disabled)
        if let Some(max_age_secs) = policy.max_age_secs {
            if max_age_secs == 0 {
                // Treat 0 as "disabled" to prevent accidental deletion of all messages
                tracing::debug!("skipping time-based retention: max_age_secs=0 (disabled)");
            } else {
                let cutoff = *now - chrono::Duration::seconds(max_age_secs as i64);
                let cutoff_str = cutoff.to_rfc3339();

                let where_clause = self.scope_where_clause(&policy.scope);
                let delete_sql = format!(
                    "DELETE FROM messages WHERE timestamp < ?1 {}",
                    where_clause
                );

                // Record tombstones first
                let tombstone_sql = format!(
                    "INSERT INTO tombstones (hash, author, sequence, deleted_at, reason)
                     SELECT hash, author, sequence, ?1, 'retention'
                     FROM messages
                     WHERE timestamp < ?2 {}",
                    where_clause
                );
                self.conn.execute(&tombstone_sql, params![now.to_rfc3339(), cutoff_str])?;

                retention_deleted += self.conn.execute(&delete_sql, params![cutoff_str])?;
            }
        }

        // Count-based retention (0 = disabled)
        if let Some(max_count) = policy.max_count {
            if max_count == 0 {
                // Treat 0 as "disabled" to prevent accidental deletion of all messages
                tracing::debug!("skipping count-based retention: max_count=0 (disabled)");
            } else {
                let deleted = self.apply_count_retention(&policy.scope, max_count as usize, now)?;
                retention_deleted += deleted;
            }
        }

        // Compaction
        if let Some(ref compact_key) = policy.compact_key {
            compacted_deleted += self.apply_compaction(&policy.scope, compact_key, now)?;
        }

        Ok((retention_deleted, compacted_deleted))
    }

    /// Generate WHERE clause fragment for a scope.
    fn scope_where_clause(&self, scope: &RetentionScope) -> String {
        match scope {
            RetentionScope::Global => String::new(),
            RetentionScope::Topic(topic) => {
                format!(
                    "AND hash IN (SELECT message_hash FROM message_tags WHERE tag = '{}')",
                    topic.replace('\'', "''")
                )
            }
            RetentionScope::Author(author) => {
                format!("AND author = '{}'", author.replace('\'', "''"))
            }
            RetentionScope::ContentType(ct) => {
                format!("AND content_type = '{}'", ct.replace('\'', "''"))
            }
        }
    }

    /// Keep only the most recent max_count messages per scope.
    fn apply_count_retention(
        &self,
        scope: &RetentionScope,
        max_count: usize,
        now: &DateTime<Utc>,
    ) -> Result<usize> {
        let where_clause = self.scope_where_clause(scope);

        // Find messages to delete (all but the most recent max_count)
        let tombstone_sql = format!(
            "INSERT INTO tombstones (hash, author, sequence, deleted_at, reason)
             SELECT hash, author, sequence, ?1, 'retention'
             FROM messages
             WHERE hash NOT IN (
                 SELECT hash FROM messages {} ORDER BY timestamp DESC LIMIT ?2
             ) {}",
            where_clause, where_clause
        );
        self.conn.execute(&tombstone_sql, params![now.to_rfc3339(), max_count as i64])?;

        let delete_sql = format!(
            "DELETE FROM messages
             WHERE hash NOT IN (
                 SELECT hash FROM messages {} ORDER BY timestamp DESC LIMIT ?1
             ) {}",
            where_clause, where_clause
        );

        let deleted = self.conn.execute(&delete_sql, params![max_count as i64])?;
        Ok(deleted)
    }

    /// Compaction: keep only the latest message per (author, compact_key value).
    ///
    /// Partitions by author to ensure compaction doesn't delete messages across
    /// different authors that happen to share the same compact key value.
    fn apply_compaction(
        &self,
        scope: &RetentionScope,
        compact_key: &str,
        now: &DateTime<Utc>,
    ) -> Result<usize> {
        let where_clause = self.scope_where_clause(scope);

        // Extract key value from JSON content using json_extract
        // For each unique (author, key_val) pair, keep only the most recent message
        let tombstone_sql = format!(
            "INSERT INTO tombstones (hash, author, sequence, deleted_at, reason)
             SELECT m.hash, m.author, m.sequence, ?1, 'compacted'
             FROM messages m
             WHERE m.hash NOT IN (
                 SELECT m2.hash
                 FROM messages m2
                 WHERE m2.hash IN (
                     SELECT hash FROM (
                         SELECT hash, author, json_extract(content_json, ?2) as key_val,
                                ROW_NUMBER() OVER (PARTITION BY author, json_extract(content_json, ?2) ORDER BY timestamp DESC) as rn
                         FROM messages
                         WHERE json_extract(content_json, ?2) IS NOT NULL {}
                     ) WHERE rn = 1
                 )
             )
             AND json_extract(m.content_json, ?2) IS NOT NULL {}",
            where_clause, where_clause
        );
        self.conn.execute(&tombstone_sql, params![now.to_rfc3339(), compact_key])?;

        let delete_sql = format!(
            "DELETE FROM messages
             WHERE hash NOT IN (
                 SELECT hash FROM (
                     SELECT hash, author, json_extract(content_json, ?1) as key_val,
                            ROW_NUMBER() OVER (PARTITION BY author, json_extract(content_json, ?1) ORDER BY timestamp DESC) as rn
                     FROM messages
                     WHERE json_extract(content_json, ?1) IS NOT NULL {}
                 ) WHERE rn = 1
             )
             AND json_extract(content_json, ?1) IS NOT NULL {}",
            where_clause, where_clause
        );

        let deleted = self.conn.execute(&delete_sql, params![compact_key])?;
        Ok(deleted)
    }

    /// Check if a message hash exists in tombstones.
    pub fn is_tombstoned(&self, hash: &str) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM tombstones WHERE hash = ?1",
            params![hash],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Get tombstone info for a message.
    pub fn get_tombstone(&self, hash: &str) -> Result<Option<(String, String, String)>> {
        let result = self.conn.query_row(
            "SELECT author, deleted_at, reason FROM tombstones WHERE hash = ?1",
            params![hash],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        );

        match result {
            Ok(row) => Ok(Some(row)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Clean up old tombstones (keep for 7 days by default).
    pub fn cleanup_tombstones(&self, max_age_secs: u64) -> Result<usize> {
        let cutoff = Utc::now() - chrono::Duration::seconds(max_age_secs as i64);
        let deleted = self.conn.execute(
            "DELETE FROM tombstones WHERE deleted_at < ?1",
            params![cutoff.to_rfc3339()],
        )?;
        Ok(deleted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (
                hash TEXT PRIMARY KEY,
                author TEXT NOT NULL,
                sequence INTEGER NOT NULL,
                timestamp TEXT NOT NULL,
                content_type TEXT NOT NULL,
                content_json TEXT NOT NULL,
                expires_at TEXT
            );
            CREATE TABLE message_tags (
                message_hash TEXT NOT NULL,
                tag TEXT NOT NULL
            );
            CREATE TABLE retention_policies (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                scope_type TEXT NOT NULL,
                scope_value TEXT,
                max_age_secs INTEGER,
                max_count INTEGER,
                max_bytes INTEGER,
                compact_key TEXT,
                created_at TEXT NOT NULL,
                UNIQUE(scope_type, scope_value)
            );
            CREATE TABLE tombstones (
                hash TEXT PRIMARY KEY,
                author TEXT NOT NULL,
                sequence INTEGER NOT NULL,
                deleted_at TEXT NOT NULL,
                reason TEXT NOT NULL
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn test_cleanup_expired_messages() {
        let conn = setup_db();
        let ops = RetentionOps::new(&conn);

        // Insert message that's already expired
        let past = (Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        conn.execute(
            "INSERT INTO messages (hash, author, sequence, timestamp, content_type, content_json, expires_at)
             VALUES ('hash1', 'author1', 1, ?1, 'test', '{}', ?1)",
            params![past],
        ).unwrap();

        // Insert message that hasn't expired
        let future = (Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        conn.execute(
            "INSERT INTO messages (hash, author, sequence, timestamp, content_type, content_json, expires_at)
             VALUES ('hash2', 'author1', 2, ?1, 'test', '{}', ?1)",
            params![future],
        ).unwrap();

        // Insert message with no expiration
        conn.execute(
            "INSERT INTO messages (hash, author, sequence, timestamp, content_type, content_json)
             VALUES ('hash3', 'author1', 3, ?1, 'test', '{}')",
            params![Utc::now().to_rfc3339()],
        ).unwrap();

        let result = ops.run_cleanup().unwrap();
        assert_eq!(result.expired, 1);

        // Verify tombstone was created
        assert!(ops.is_tombstoned("hash1").unwrap());
        assert!(!ops.is_tombstoned("hash2").unwrap());
        assert!(!ops.is_tombstoned("hash3").unwrap());
    }

    #[test]
    fn test_save_and_list_policies() {
        let conn = setup_db();
        let ops = RetentionOps::new(&conn);

        let policy = RetentionPolicy::global_max_age(3600);
        ops.save_policy(&policy).unwrap();

        let policies = ops.list_policies().unwrap();
        assert_eq!(policies.len(), 1);
        assert_eq!(policies[0].max_age_secs, Some(3600));
    }

    #[test]
    fn test_count_retention() {
        let conn = setup_db();
        let ops = RetentionOps::new(&conn);

        // Insert 5 messages
        for i in 1..=5 {
            let ts = Utc::now() - chrono::Duration::minutes(5 - i);
            conn.execute(
                "INSERT INTO messages (hash, author, sequence, timestamp, content_type, content_json)
                 VALUES (?1, 'author1', ?2, ?3, 'test', '{}')",
                params![format!("hash{}", i), i, ts.to_rfc3339()],
            ).unwrap();
        }

        // Create policy to keep only 3 messages
        let policy = RetentionPolicy {
            id: None,
            scope: RetentionScope::Global,
            max_age_secs: None,
            max_count: Some(3),
            max_bytes: None,
            compact_key: None,
        };
        ops.save_policy(&policy).unwrap();

        let result = ops.run_cleanup().unwrap();
        assert_eq!(result.retention, 2);

        // Verify only 3 messages remain
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 3);
    }
}
