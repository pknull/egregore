//! Consumer groups — Kafka-style group membership and offset tracking.
//!
//! Consumer groups enable coordinated consumption of feeds. Multiple consumers
//! join a group, and feeds are assigned to members via round-robin. Group offsets
//! (cursors) are stored per-group rather than per-consumer, enabling exactly-once
//! semantics across group members.
//!
//! Membership changes trigger rebalancing: feeds are redistributed among active
//! members. A generation counter ensures stale assignments are rejected.

use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

use crate::error::{EgreError, Result};
use crate::identity::PublicId;

use super::FeedStore;

/// Maximum number of results for unbounded queries.
/// Prevents memory exhaustion from large groups.
const MAX_QUERY_RESULTS: usize = 10_000;

/// Parse RFC3339 timestamp with fallback to now, logging a warning on parse failure.
fn parse_rfc3339_or_now(s: &str, field: &str) -> DateTime<Utc> {
    super::parse_rfc3339(s).unwrap_or_else(|| {
        tracing::warn!(
            field = field,
            value = s,
            "invalid RFC3339 timestamp in consumer_groups, using current time"
        );
        Utc::now()
    })
}

/// A consumer group with members and their feed assignments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsumerGroup {
    pub group_id: String,
    /// Monotonically increasing on each rebalance.
    pub generation: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A member of a consumer group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupMember {
    pub group_id: String,
    pub member_id: PublicId,
    pub joined_at: DateTime<Utc>,
    /// Last heartbeat timestamp. Members must heartbeat periodically or be evicted.
    pub last_heartbeat: DateTime<Utc>,
    /// Feeds assigned to this member (author public IDs).
    pub assigned_feeds: Vec<PublicId>,
    /// The generation when this assignment was made.
    pub assignment_generation: u64,
}

/// Default heartbeat timeout in seconds. Members not heartbeating within this
/// period are considered dead and eligible for eviction.
/// Used by callers of `evict_stale_members`.
#[allow(dead_code)] // Used by API callers and tests
pub const DEFAULT_HEARTBEAT_TIMEOUT_SECS: i64 = 30;

/// A group offset (cursor) for a specific feed within a group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupOffset {
    pub group_id: String,
    pub author: PublicId,
    /// The last committed sequence number (inclusive).
    pub committed_sequence: u64,
    pub committed_at: DateTime<Utc>,
    /// Member who committed this offset.
    pub committed_by: PublicId,
}

/// Request to join a consumer group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinGroupRequest {
    pub group_id: String,
    pub member_id: PublicId,
}

/// Result of joining a consumer group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinGroupResult {
    pub group_id: String,
    pub member_id: PublicId,
    pub generation: u64,
    pub assigned_feeds: Vec<PublicId>,
    pub is_leader: bool,
}

/// Assignment strategy for distributing feeds among group members.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentStrategy {
    /// Simple round-robin assignment by feed author.
    RoundRobin,
}

impl Default for AssignmentStrategy {
    fn default() -> Self {
        Self::RoundRobin
    }
}

impl FeedStore {
    // ---- Consumer Group DDL (called during schema init) ----

    /// Additional schema for consumer groups. Called by run_post_schema_migrations.
    pub(crate) fn ensure_groups_schema(conn: &rusqlite::Connection) -> Result<()> {
        // Check if consumer_groups table exists
        let has_groups: bool = conn
            .prepare("SELECT 1 FROM consumer_groups LIMIT 0")
            .is_ok();

        if !has_groups {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS consumer_groups (
                    group_id TEXT PRIMARY KEY,
                    generation INTEGER NOT NULL DEFAULT 1,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS group_members (
                    group_id TEXT NOT NULL,
                    member_id TEXT NOT NULL,
                    joined_at TEXT NOT NULL,
                    last_heartbeat TEXT NOT NULL,
                    assignment_generation INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (group_id, member_id),
                    FOREIGN KEY (group_id) REFERENCES consumer_groups(group_id) ON DELETE CASCADE
                );
                CREATE INDEX IF NOT EXISTS idx_group_members_group ON group_members(group_id);

                CREATE TABLE IF NOT EXISTS group_assignments (
                    group_id TEXT NOT NULL,
                    member_id TEXT NOT NULL,
                    author TEXT NOT NULL,
                    generation INTEGER NOT NULL,
                    PRIMARY KEY (group_id, author),
                    FOREIGN KEY (group_id) REFERENCES consumer_groups(group_id) ON DELETE CASCADE
                );
                CREATE INDEX IF NOT EXISTS idx_group_assignments_member ON group_assignments(group_id, member_id);

                CREATE TABLE IF NOT EXISTS group_offsets (
                    group_id TEXT NOT NULL,
                    author TEXT NOT NULL,
                    committed_sequence INTEGER NOT NULL DEFAULT 0,
                    committed_at TEXT NOT NULL,
                    committed_by TEXT NOT NULL,
                    PRIMARY KEY (group_id, author),
                    FOREIGN KEY (group_id) REFERENCES consumer_groups(group_id) ON DELETE CASCADE
                );
                CREATE INDEX IF NOT EXISTS idx_group_offsets_group ON group_offsets(group_id);",
            )?;
        } else {
            // Migration: add last_heartbeat column if missing
            let has_heartbeat: bool = conn
                .prepare("SELECT last_heartbeat FROM group_members LIMIT 0")
                .is_ok();

            if !has_heartbeat {
                let now_str = Utc::now().to_rfc3339();
                conn.execute(
                    &format!(
                        "ALTER TABLE group_members ADD COLUMN last_heartbeat TEXT NOT NULL DEFAULT '{}'",
                        now_str
                    ),
                    [],
                )?;
            }
        }

        Ok(())
    }

    // ---- Consumer Group Operations ----

    /// Create a new consumer group. Returns the created group.
    pub fn create_group(&self, group_id: &str) -> Result<ConsumerGroup> {
        let conn = self.conn();
        let now = Utc::now();
        let now_str = now.to_rfc3339();

        conn.execute(
            "INSERT INTO consumer_groups (group_id, generation, created_at, updated_at)
             VALUES (?1, 1, ?2, ?2)",
            params![group_id, now_str],
        )
        .map_err(|e| match e {
            rusqlite::Error::SqliteFailure(err, _)
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                EgreError::FeedIntegrity {
                    reason: format!("consumer group '{}' already exists", group_id),
                }
            }
            other => EgreError::Database(other),
        })?;

        Ok(ConsumerGroup {
            group_id: group_id.to_string(),
            generation: 1,
            created_at: now,
            updated_at: now,
        })
    }

    /// Get a consumer group by ID.
    pub fn get_group(&self, group_id: &str) -> Result<Option<ConsumerGroup>> {
        let conn = self.conn();
        conn.query_row(
            "SELECT group_id, generation, created_at, updated_at FROM consumer_groups WHERE group_id = ?1",
            params![group_id],
            |row| {
                let group_id: String = row.get(0)?;
                let generation: u64 = row.get(1)?;
                let created_at_str: String = row.get(2)?;
                let updated_at_str: String = row.get(3)?;
                Ok((group_id, generation, created_at_str, updated_at_str))
            },
        )
        .optional()?
        .map(|(group_id, generation, created_at_str, updated_at_str)| {
            let created_at = parse_rfc3339_or_now(&created_at_str, "created_at");
            let updated_at = parse_rfc3339_or_now(&updated_at_str, "updated_at");
            Ok(ConsumerGroup {
                group_id,
                generation,
                created_at,
                updated_at,
            })
        })
        .transpose()
    }

    /// List all consumer groups.
    pub fn list_groups(&self) -> Result<Vec<ConsumerGroup>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT group_id, generation, created_at, updated_at FROM consumer_groups ORDER BY created_at DESC",
        )?;

        let rows = stmt
            .query_map([], |row| {
                let group_id: String = row.get(0)?;
                let generation: u64 = row.get(1)?;
                let created_at_str: String = row.get(2)?;
                let updated_at_str: String = row.get(3)?;
                Ok((group_id, generation, created_at_str, updated_at_str))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        rows.into_iter()
            .take(MAX_QUERY_RESULTS)
            .map(|(group_id, generation, created_at_str, updated_at_str)| {
                let created_at = parse_rfc3339_or_now(&created_at_str, "created_at");
                let updated_at = parse_rfc3339_or_now(&updated_at_str, "updated_at");
                Ok(ConsumerGroup {
                    group_id,
                    generation,
                    created_at,
                    updated_at,
                })
            })
            .collect()
    }

    /// Delete a consumer group and all its members/assignments/offsets.
    pub fn delete_group(&self, group_id: &str) -> Result<bool> {
        let conn = self.conn();
        let rows = conn.execute(
            "DELETE FROM consumer_groups WHERE group_id = ?1",
            params![group_id],
        )?;
        Ok(rows > 0)
    }

    /// Join a consumer group. Creates the group if it doesn't exist.
    /// Returns the join result with assigned feeds after rebalancing.
    pub fn join_group(&self, group_id: &str, member_id: &PublicId) -> Result<JoinGroupResult> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = Utc::now();
        let now_str = now.to_rfc3339();

        // Create group if it doesn't exist
        tx.execute(
            "INSERT OR IGNORE INTO consumer_groups (group_id, generation, created_at, updated_at)
             VALUES (?1, 1, ?2, ?2)",
            params![group_id, now_str],
        )?;

        // Add member (or update heartbeat if already present)
        tx.execute(
            "INSERT INTO group_members (group_id, member_id, joined_at, last_heartbeat, assignment_generation)
             VALUES (?1, ?2, ?3, ?3, 0)
             ON CONFLICT(group_id, member_id) DO UPDATE SET last_heartbeat = excluded.last_heartbeat",
            params![group_id, member_id.0, now_str],
        )?;

        // Increment generation for rebalance
        tx.execute(
            "UPDATE consumer_groups SET generation = generation + 1, updated_at = ?1 WHERE group_id = ?2",
            params![now_str, group_id],
        )?;

        // Get new generation
        let generation: u64 = tx.query_row(
            "SELECT generation FROM consumer_groups WHERE group_id = ?1",
            params![group_id],
            |row| row.get(0),
        )?;

        // Get all members
        let members: Vec<PublicId> = {
            let mut stmt = tx.prepare(
                "SELECT member_id FROM group_members WHERE group_id = ?1 ORDER BY joined_at ASC",
            )?;
            let rows = stmt.query_map(params![group_id], |row| {
                let id: String = row.get(0)?;
                Ok(PublicId(id))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };

        // Get all feeds (authors) we know about
        let feeds: Vec<PublicId> = {
            let mut stmt = tx.prepare("SELECT author FROM feeds ORDER BY author ASC")?;
            let rows = stmt.query_map([], |row| {
                let author: String = row.get(0)?;
                Ok(PublicId(author))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };

        // Perform round-robin assignment
        let assignments = round_robin_assign(&members, &feeds);

        // Clear old assignments for this group
        tx.execute(
            "DELETE FROM group_assignments WHERE group_id = ?1",
            params![group_id],
        )?;

        // Insert new assignments
        for (assigned_member, assigned_feeds) in &assignments {
            for feed_author in assigned_feeds {
                tx.execute(
                    "INSERT INTO group_assignments (group_id, member_id, author, generation)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![group_id, assigned_member.0, feed_author.0, generation],
                )?;
            }
            // Update member's assignment generation
            tx.execute(
                "UPDATE group_members SET assignment_generation = ?1 WHERE group_id = ?2 AND member_id = ?3",
                params![generation, group_id, assigned_member.0],
            )?;
        }

        tx.commit()?;

        // Find this member's assignments
        let my_feeds = assignments
            .iter()
            .find(|(m, _)| m == member_id)
            .map(|(_, feeds)| feeds.clone())
            .unwrap_or_default();

        // First member to join is the leader
        let is_leader = members.first().map(|m| m == member_id).unwrap_or(false);

        Ok(JoinGroupResult {
            group_id: group_id.to_string(),
            member_id: member_id.clone(),
            generation,
            assigned_feeds: my_feeds,
            is_leader,
        })
    }

    /// Leave a consumer group. Triggers rebalance for remaining members.
    pub fn leave_group(&self, group_id: &str, member_id: &PublicId) -> Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now_str = Utc::now().to_rfc3339();

        // Remove member
        let rows = tx.execute(
            "DELETE FROM group_members WHERE group_id = ?1 AND member_id = ?2",
            params![group_id, member_id.0],
        )?;

        if rows == 0 {
            // Note: rollback is not needed here, just drop the transaction
            return Ok(false);
        }

        // Remove this member's assignments
        tx.execute(
            "DELETE FROM group_assignments WHERE group_id = ?1 AND member_id = ?2",
            params![group_id, member_id.0],
        )?;

        // Increment generation for rebalance
        tx.execute(
            "UPDATE consumer_groups SET generation = generation + 1, updated_at = ?1 WHERE group_id = ?2",
            params![now_str, group_id],
        )?;

        // Get new generation
        let generation: u64 = tx.query_row(
            "SELECT generation FROM consumer_groups WHERE group_id = ?1",
            params![group_id],
            |row| row.get(0),
        )?;

        // Get remaining members
        let members: Vec<PublicId> = {
            let mut stmt = tx.prepare(
                "SELECT member_id FROM group_members WHERE group_id = ?1 ORDER BY joined_at ASC",
            )?;
            let rows = stmt.query_map(params![group_id], |row| {
                let id: String = row.get(0)?;
                Ok(PublicId(id))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };

        if members.is_empty() {
            // No members left, delete the group
            tx.execute(
                "DELETE FROM consumer_groups WHERE group_id = ?1",
                params![group_id],
            )?;
            tx.commit()?;
            return Ok(true);
        }

        // Get all feeds
        let feeds: Vec<PublicId> = {
            let mut stmt = tx.prepare("SELECT author FROM feeds ORDER BY author ASC")?;
            let rows = stmt.query_map([], |row| {
                let author: String = row.get(0)?;
                Ok(PublicId(author))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };

        // Reassign
        let assignments = round_robin_assign(&members, &feeds);

        // Clear old assignments
        tx.execute(
            "DELETE FROM group_assignments WHERE group_id = ?1",
            params![group_id],
        )?;

        // Insert new assignments
        for (assigned_member, assigned_feeds) in &assignments {
            for feed_author in assigned_feeds {
                tx.execute(
                    "INSERT INTO group_assignments (group_id, member_id, author, generation)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![group_id, assigned_member.0, feed_author.0, generation],
                )?;
            }
            tx.execute(
                "UPDATE group_members SET assignment_generation = ?1 WHERE group_id = ?2 AND member_id = ?3",
                params![generation, group_id, assigned_member.0],
            )?;
        }

        tx.commit()?;
        Ok(true)
    }

    /// Get all members of a consumer group.
    pub fn get_group_members(&self, group_id: &str) -> Result<Vec<GroupMember>> {
        let conn = self.conn();

        // Get group generation
        let generation: u64 = conn
            .query_row(
                "SELECT generation FROM consumer_groups WHERE group_id = ?1",
                params![group_id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);

        // Get all members
        let members: Vec<(String, String, String, u64)> = {
            let mut stmt = conn.prepare(
                "SELECT member_id, joined_at, last_heartbeat, assignment_generation FROM group_members WHERE group_id = ?1 ORDER BY joined_at ASC",
            )?;
            let rows = stmt
                .query_map(params![group_id], |row| {
                    let member_id: String = row.get(0)?;
                    let joined_at: String = row.get(1)?;
                    let last_heartbeat: String = row.get(2)?;
                    let assignment_generation: u64 = row.get(3)?;
                    Ok((member_id, joined_at, last_heartbeat, assignment_generation))
                })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };

        // Get all assignments for this group (single query instead of per-member)
        let all_assignments: Vec<(String, String)> = {
            let mut stmt = conn.prepare(
                "SELECT member_id, author FROM group_assignments WHERE group_id = ?1 ORDER BY member_id ASC, author ASC",
            )?;
            let rows = stmt.query_map(params![group_id], |row| {
                let member_id: String = row.get(0)?;
                let author: String = row.get(1)?;
                Ok((member_id, author))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };

        // Build map of member -> assigned feeds
        let mut assignments_map: std::collections::HashMap<String, Vec<PublicId>> =
            std::collections::HashMap::new();
        for (member_id, author) in all_assignments {
            assignments_map
                .entry(member_id)
                .or_default()
                .push(PublicId(author));
        }

        // Drop conn so we don't hold the lock
        drop(conn);

        members
            .into_iter()
            .take(MAX_QUERY_RESULTS)
            .map(|(member_id, joined_at_str, last_heartbeat_str, assignment_generation)| {
                let joined_at = parse_rfc3339_or_now(&joined_at_str, "joined_at");
                let last_heartbeat = parse_rfc3339_or_now(&last_heartbeat_str, "last_heartbeat");
                let assigned_feeds = assignments_map.remove(&member_id).unwrap_or_default();

                Ok(GroupMember {
                    group_id: group_id.to_string(),
                    member_id: PublicId(member_id),
                    joined_at,
                    last_heartbeat,
                    assigned_feeds,
                    assignment_generation: if assignment_generation == generation {
                        assignment_generation
                    } else {
                        generation
                    },
                })
            })
            .collect()
    }

    /// Get feed assignments for a specific member.
    pub fn get_member_assignments(&self, group_id: &str, member_id: &PublicId) -> Result<Vec<PublicId>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT author FROM group_assignments WHERE group_id = ?1 AND member_id = ?2 ORDER BY author ASC",
        )?;

        let feeds = stmt
            .query_map(params![group_id, member_id.0], |row| {
                let author: String = row.get(0)?;
                Ok(PublicId(author))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(feeds)
    }

    /// Update heartbeat timestamp for a member. Call periodically to prevent eviction.
    ///
    /// Returns `true` if the member exists and was updated, `false` if not found.
    pub fn heartbeat(&self, group_id: &str, member_id: &PublicId) -> Result<bool> {
        let conn = self.conn();
        let now_str = Utc::now().to_rfc3339();

        let rows = conn.execute(
            "UPDATE group_members SET last_heartbeat = ?1 WHERE group_id = ?2 AND member_id = ?3",
            params![now_str, group_id, member_id.0],
        )?;

        Ok(rows > 0)
    }

    /// Evict members who haven't sent a heartbeat within the timeout.
    ///
    /// Returns the number of members evicted. Triggers rebalance for affected groups.
    pub fn evict_stale_members(&self, timeout_secs: i64) -> Result<u64> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let cutoff = (Utc::now() - chrono::Duration::seconds(timeout_secs)).to_rfc3339();

        // Find stale members
        let stale: Vec<(String, String)> = {
            let mut stmt = tx.prepare(
                "SELECT group_id, member_id FROM group_members WHERE last_heartbeat < ?1",
            )?;
            let rows = stmt.query_map(params![cutoff], |row| {
                let group_id: String = row.get(0)?;
                let member_id: String = row.get(1)?;
                Ok((group_id, member_id))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };

        let evicted = stale.len() as u64;

        if evicted == 0 {
            return Ok(0);
        }

        // Delete stale members and their assignments
        tx.execute(
            "DELETE FROM group_members WHERE last_heartbeat < ?1",
            params![cutoff],
        )?;

        // Get affected groups that still have members (need rebalance)
        let affected_groups: Vec<String> = {
            // Just get groups that had stale members (simpler than complex query)

            let unique_groups: std::collections::HashSet<_> =
                stale.iter().map(|(g, _)| g.clone()).collect();
            unique_groups.into_iter().collect()
        };

        // Rebalance each affected group
        let now_str = Utc::now().to_rfc3339();
        for group_id in &affected_groups {
            // Increment generation
            tx.execute(
                "UPDATE consumer_groups SET generation = generation + 1, updated_at = ?1 WHERE group_id = ?2",
                params![now_str, group_id],
            )?;

            // Get new generation
            let generation: u64 = tx
                .query_row(
                    "SELECT generation FROM consumer_groups WHERE group_id = ?1",
                    params![group_id],
                    |row| row.get(0),
                )
                .optional()?
                .unwrap_or(1);

            // Get remaining members
            let members: Vec<PublicId> = {
                let mut stmt = tx.prepare(
                    "SELECT member_id FROM group_members WHERE group_id = ?1 ORDER BY joined_at ASC",
                )?;
                let rows = stmt.query_map(params![group_id], |row| {
                    let id: String = row.get(0)?;
                    Ok(PublicId(id))
                })?;
                rows.collect::<std::result::Result<Vec<_>, _>>()?
            };

            if members.is_empty() {
                // No members left, delete the group
                tx.execute(
                    "DELETE FROM consumer_groups WHERE group_id = ?1",
                    params![group_id],
                )?;
                continue;
            }

            // Get all feeds
            let feeds: Vec<PublicId> = {
                let mut stmt = tx.prepare("SELECT author FROM feeds ORDER BY author ASC")?;
                let rows = stmt.query_map([], |row| {
                    let author: String = row.get(0)?;
                    Ok(PublicId(author))
                })?;
                rows.collect::<std::result::Result<Vec<_>, _>>()?
            };

            // Clear old assignments and reassign
            tx.execute(
                "DELETE FROM group_assignments WHERE group_id = ?1",
                params![group_id],
            )?;

            let assignments = round_robin_assign(&members, &feeds);
            for (assigned_member, assigned_feeds) in &assignments {
                for feed_author in assigned_feeds {
                    tx.execute(
                        "INSERT INTO group_assignments (group_id, member_id, author, generation)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![group_id, assigned_member.0, feed_author.0, generation],
                    )?;
                }
                tx.execute(
                    "UPDATE group_members SET assignment_generation = ?1 WHERE group_id = ?2 AND member_id = ?3",
                    params![generation, group_id, assigned_member.0],
                )?;
            }
        }

        tx.commit()?;
        Ok(evicted)
    }

    /// Commit an offset for a feed within a consumer group.
    ///
    /// Uses a transaction to atomically verify assignment and commit offset,
    /// preventing race conditions with concurrent leave_group calls.
    pub fn commit_offset(
        &self,
        group_id: &str,
        author: &PublicId,
        sequence: u64,
        committed_by: &PublicId,
    ) -> Result<GroupOffset> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = Utc::now();
        let now_str = now.to_rfc3339();

        // Verify the committer is assigned this feed (inside transaction)
        let is_assigned: bool = tx
            .query_row(
                "SELECT 1 FROM group_assignments WHERE group_id = ?1 AND member_id = ?2 AND author = ?3",
                params![group_id, committed_by.0, author.0],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);

        if !is_assigned {
            // Transaction will be rolled back on drop
            return Err(EgreError::FeedIntegrity {
                reason: format!(
                    "member {} is not assigned feed {} in group {}",
                    committed_by.0, author.0, group_id
                ),
            });
        }

        tx.execute(
            "INSERT INTO group_offsets (group_id, author, committed_sequence, committed_at, committed_by)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(group_id, author) DO UPDATE SET
                committed_sequence = MAX(group_offsets.committed_sequence, excluded.committed_sequence),
                committed_at = excluded.committed_at,
                committed_by = excluded.committed_by",
            params![group_id, author.0, sequence, now_str, committed_by.0],
        )?;

        tx.commit()?;

        Ok(GroupOffset {
            group_id: group_id.to_string(),
            author: author.clone(),
            committed_sequence: sequence,
            committed_at: now,
            committed_by: committed_by.clone(),
        })
    }

    /// Get committed offset for a feed within a consumer group.
    pub fn get_offset(&self, group_id: &str, author: &PublicId) -> Result<Option<GroupOffset>> {
        let conn = self.conn();
        conn.query_row(
            "SELECT committed_sequence, committed_at, committed_by FROM group_offsets
             WHERE group_id = ?1 AND author = ?2",
            params![group_id, author.0],
            |row| {
                let seq: u64 = row.get(0)?;
                let committed_at_str: String = row.get(1)?;
                let committed_by: String = row.get(2)?;
                Ok((seq, committed_at_str, committed_by))
            },
        )
        .optional()?
        .map(|(seq, committed_at_str, committed_by)| {
            let committed_at = parse_rfc3339_or_now(&committed_at_str, "committed_at");
            Ok(GroupOffset {
                group_id: group_id.to_string(),
                author: author.clone(),
                committed_sequence: seq,
                committed_at,
                committed_by: PublicId(committed_by),
            })
        })
        .transpose()
    }

    /// Get all committed offsets for a consumer group.
    pub fn get_group_offsets(&self, group_id: &str) -> Result<Vec<GroupOffset>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT author, committed_sequence, committed_at, committed_by FROM group_offsets
             WHERE group_id = ?1 ORDER BY author ASC",
        )?;

        let rows = stmt
            .query_map(params![group_id], |row| {
                let author: String = row.get(0)?;
                let seq: u64 = row.get(1)?;
                let committed_at_str: String = row.get(2)?;
                let committed_by: String = row.get(3)?;
                Ok((author, seq, committed_at_str, committed_by))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        rows.into_iter()
            .take(MAX_QUERY_RESULTS)
            .map(|(author, seq, committed_at_str, committed_by)| {
                let committed_at = parse_rfc3339_or_now(&committed_at_str, "committed_at");
                Ok(GroupOffset {
                    group_id: group_id.to_string(),
                    author: PublicId(author),
                    committed_sequence: seq,
                    committed_at,
                    committed_by: PublicId(committed_by),
                })
            })
            .collect()
    }

    /// Get the current generation of a consumer group.
    pub fn get_group_generation(&self, group_id: &str) -> Result<Option<u64>> {
        let conn = self.conn();
        conn.query_row(
            "SELECT generation FROM consumer_groups WHERE group_id = ?1",
            params![group_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(EgreError::from)
    }

    /// Count members in a consumer group.
    pub fn group_member_count(&self, group_id: &str) -> Result<u64> {
        let conn = self.conn();
        conn.query_row(
            "SELECT COUNT(*) FROM group_members WHERE group_id = ?1",
            params![group_id],
            |row| row.get(0),
        )
        .map_err(EgreError::from)
    }
}

/// Perform round-robin assignment of feeds to members.
/// Returns a list of (member, assigned_feeds) tuples.
fn round_robin_assign(
    members: &[PublicId],
    feeds: &[PublicId],
) -> Vec<(PublicId, Vec<PublicId>)> {
    if members.is_empty() {
        return Vec::new();
    }

    let mut assignments: Vec<(PublicId, Vec<PublicId>)> = members
        .iter()
        .map(|m| (m.clone(), Vec::new()))
        .collect();

    for (i, feed) in feeds.iter().enumerate() {
        let member_idx = i % members.len();
        assignments[member_idx].1.push(feed.clone());
    }

    assignments
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::store::make_test_message;

    fn setup_store_with_feeds() -> FeedStore {
        let store = FeedStore::open_memory().unwrap();
        // Add some feeds
        store
            .insert_message(&make_test_message("@alice.ed25519", 1, None), true)
            .unwrap();
        store
            .insert_message(&make_test_message("@bob.ed25519", 1, None), true)
            .unwrap();
        store
            .insert_message(&make_test_message("@carol.ed25519", 1, None), true)
            .unwrap();
        store
    }

    #[test]
    fn create_and_get_group() {
        let store = FeedStore::open_memory().unwrap();
        let group = store.create_group("test-group").unwrap();

        assert_eq!(group.group_id, "test-group");
        assert_eq!(group.generation, 1);

        let retrieved = store.get_group("test-group").unwrap().unwrap();
        assert_eq!(retrieved.group_id, "test-group");
    }

    #[test]
    fn create_duplicate_group_fails() {
        let store = FeedStore::open_memory().unwrap();
        store.create_group("test-group").unwrap();

        let result = store.create_group("test-group");
        assert!(result.is_err());
    }

    #[test]
    fn join_group_creates_if_not_exists() {
        let store = setup_store_with_feeds();
        let member = PublicId("@member1.ed25519".to_string());

        let result = store.join_group("new-group", &member).unwrap();

        assert_eq!(result.group_id, "new-group");
        assert_eq!(result.member_id, member);
        assert!(result.is_leader);
        assert_eq!(result.generation, 2); // Created at 1, then incremented on join
    }

    #[test]
    fn join_group_round_robin_assignment() {
        let store = setup_store_with_feeds();

        let m1 = PublicId("@m1.ed25519".to_string());
        let m2 = PublicId("@m2.ed25519".to_string());

        let r1 = store.join_group("rr-group", &m1).unwrap();
        assert!(r1.is_leader);
        // First member gets all feeds initially
        assert_eq!(r1.assigned_feeds.len(), 3);

        let r2 = store.join_group("rr-group", &m2).unwrap();
        assert!(!r2.is_leader);

        // After second join, feeds should be redistributed
        // With 3 feeds and 2 members: m1 gets 2, m2 gets 1 (or similar)
        let m1_feeds = store.get_member_assignments("rr-group", &m1).unwrap();
        let m2_feeds = store.get_member_assignments("rr-group", &m2).unwrap();

        assert_eq!(m1_feeds.len() + m2_feeds.len(), 3);
        // Round-robin: indices 0,2 -> m1, index 1 -> m2
        assert_eq!(m1_feeds.len(), 2);
        assert_eq!(m2_feeds.len(), 1);
    }

    #[test]
    fn leave_group_rebalances() {
        let store = setup_store_with_feeds();

        let m1 = PublicId("@m1.ed25519".to_string());
        let m2 = PublicId("@m2.ed25519".to_string());

        store.join_group("leave-test", &m1).unwrap();
        store.join_group("leave-test", &m2).unwrap();

        // m2 leaves
        assert!(store.leave_group("leave-test", &m2).unwrap());

        // m1 should now have all feeds
        let m1_feeds = store.get_member_assignments("leave-test", &m1).unwrap();
        assert_eq!(m1_feeds.len(), 3);

        // Group still exists
        assert!(store.get_group("leave-test").unwrap().is_some());
    }

    #[test]
    fn leave_group_deletes_when_empty() {
        let store = FeedStore::open_memory().unwrap();
        let member = PublicId("@member.ed25519".to_string());

        store.join_group("solo-group", &member).unwrap();
        assert!(store.leave_group("solo-group", &member).unwrap());

        // Group should be deleted
        assert!(store.get_group("solo-group").unwrap().is_none());
    }

    #[test]
    fn commit_and_get_offset() {
        let store = setup_store_with_feeds();
        let member = PublicId("@member.ed25519".to_string());
        let author = PublicId("@alice.ed25519".to_string());

        store.join_group("offset-test", &member).unwrap();

        let offset = store
            .commit_offset("offset-test", &author, 5, &member)
            .unwrap();

        assert_eq!(offset.committed_sequence, 5);
        assert_eq!(offset.committed_by, member);

        let retrieved = store.get_offset("offset-test", &author).unwrap().unwrap();
        assert_eq!(retrieved.committed_sequence, 5);
    }

    #[test]
    fn commit_offset_rejects_unassigned() {
        let store = setup_store_with_feeds();
        let m1 = PublicId("@m1.ed25519".to_string());
        let m2 = PublicId("@m2.ed25519".to_string());

        store.join_group("reject-test", &m1).unwrap();
        store.join_group("reject-test", &m2).unwrap();

        // Find a feed assigned to m1
        let m1_feeds = store.get_member_assignments("reject-test", &m1).unwrap();
        assert!(!m1_feeds.is_empty());

        // m2 tries to commit offset for m1's feed
        let result = store.commit_offset("reject-test", &m1_feeds[0], 10, &m2);
        assert!(result.is_err());
    }

    #[test]
    fn offset_only_increases() {
        let store = setup_store_with_feeds();
        let member = PublicId("@member.ed25519".to_string());
        let author = PublicId("@alice.ed25519".to_string());

        store.join_group("inc-test", &member).unwrap();

        store.commit_offset("inc-test", &author, 10, &member).unwrap();
        store.commit_offset("inc-test", &author, 5, &member).unwrap(); // Lower value

        let offset = store.get_offset("inc-test", &author).unwrap().unwrap();
        // Should still be 10 (MAX behavior)
        assert_eq!(offset.committed_sequence, 10);
    }

    #[test]
    fn get_group_members() {
        let store = setup_store_with_feeds();
        let m1 = PublicId("@m1.ed25519".to_string());
        let m2 = PublicId("@m2.ed25519".to_string());

        store.join_group("members-test", &m1).unwrap();
        store.join_group("members-test", &m2).unwrap();

        let members = store.get_group_members("members-test").unwrap();
        assert_eq!(members.len(), 2);
        assert_eq!(members[0].member_id, m1);
        assert_eq!(members[1].member_id, m2);
    }

    #[test]
    fn list_groups() {
        let store = FeedStore::open_memory().unwrap();
        store.create_group("group-a").unwrap();
        store.create_group("group-b").unwrap();

        let groups = store.list_groups().unwrap();
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn delete_group() {
        let store = FeedStore::open_memory().unwrap();
        let member = PublicId("@member.ed25519".to_string());

        store.join_group("delete-me", &member).unwrap();
        assert!(store.get_group("delete-me").unwrap().is_some());

        assert!(store.delete_group("delete-me").unwrap());
        assert!(store.get_group("delete-me").unwrap().is_none());

        // Members should also be gone (cascading delete)
        let members = store.get_group_members("delete-me").unwrap();
        assert!(members.is_empty());
    }

    #[test]
    fn round_robin_assignment_function() {
        let members = vec![
            PublicId("@a.ed25519".to_string()),
            PublicId("@b.ed25519".to_string()),
            PublicId("@c.ed25519".to_string()),
        ];
        let feeds = vec![
            PublicId("@f1.ed25519".to_string()),
            PublicId("@f2.ed25519".to_string()),
            PublicId("@f3.ed25519".to_string()),
            PublicId("@f4.ed25519".to_string()),
            PublicId("@f5.ed25519".to_string()),
        ];

        let assignments = round_robin_assign(&members, &feeds);

        // 5 feeds, 3 members: a gets [f1, f4], b gets [f2, f5], c gets [f3]
        assert_eq!(assignments.len(), 3);
        assert_eq!(assignments[0].1.len(), 2); // a: f1, f4
        assert_eq!(assignments[1].1.len(), 2); // b: f2, f5
        assert_eq!(assignments[2].1.len(), 1); // c: f3
    }

    #[test]
    fn round_robin_empty_members() {
        let members: Vec<PublicId> = vec![];
        let feeds = vec![PublicId("@f1.ed25519".to_string())];

        let assignments = round_robin_assign(&members, &feeds);
        assert!(assignments.is_empty());
    }

    #[test]
    fn round_robin_empty_feeds() {
        let members = vec![PublicId("@a.ed25519".to_string())];
        let feeds: Vec<PublicId> = vec![];

        let assignments = round_robin_assign(&members, &feeds);
        assert_eq!(assignments.len(), 1);
        assert!(assignments[0].1.is_empty());
    }

    #[test]
    fn heartbeat_updates_timestamp() {
        let store = FeedStore::open_memory().unwrap();
        let member = PublicId("@member.ed25519".to_string());

        store.join_group("hb-test", &member).unwrap();

        // Get initial heartbeat
        let members = store.get_group_members("hb-test").unwrap();
        let initial_hb = members[0].last_heartbeat;

        // Small sleep to ensure time difference
        std::thread::sleep(std::time::Duration::from_millis(10));

        // Send heartbeat
        assert!(store.heartbeat("hb-test", &member).unwrap());

        // Verify heartbeat updated
        let members = store.get_group_members("hb-test").unwrap();
        assert!(members[0].last_heartbeat > initial_hb);
    }

    #[test]
    fn heartbeat_returns_false_for_unknown_member() {
        let store = FeedStore::open_memory().unwrap();
        store.create_group("hb-test").unwrap();

        let unknown = PublicId("@unknown.ed25519".to_string());
        assert!(!store.heartbeat("hb-test", &unknown).unwrap());
    }

    #[test]
    fn evict_stale_members_removes_old() {
        let store = setup_store_with_feeds();
        let m1 = PublicId("@m1.ed25519".to_string());
        let m2 = PublicId("@m2.ed25519".to_string());

        store.join_group("evict-test", &m1).unwrap();
        store.join_group("evict-test", &m2).unwrap();

        // Both members should exist
        assert_eq!(store.get_group_members("evict-test").unwrap().len(), 2);

        // Manually set heartbeat to old time to simulate stale member
        {
            let conn = store.conn();
            let old_time = (chrono::Utc::now() - chrono::Duration::seconds(120)).to_rfc3339();
            conn.execute(
                "UPDATE group_members SET last_heartbeat = ?1 WHERE group_id = 'evict-test'",
                rusqlite::params![old_time],
            )
            .unwrap();
        }

        // Evict with 60 second timeout - both should be evicted (last heartbeat was 120s ago)
        let evicted = store.evict_stale_members(60).unwrap();
        assert_eq!(evicted, 2);

        // Group should be deleted (no members left)
        assert!(store.get_group("evict-test").unwrap().is_none());
    }

    #[test]
    fn evict_stale_preserves_active_members() {
        let store = setup_store_with_feeds();
        let m1 = PublicId("@m1.ed25519".to_string());

        store.join_group("evict-active", &m1).unwrap();

        // Heartbeat to keep alive
        store.heartbeat("evict-active", &m1).unwrap();

        // Evict with reasonable timeout - member should survive
        let evicted = store.evict_stale_members(super::DEFAULT_HEARTBEAT_TIMEOUT_SECS).unwrap();
        assert_eq!(evicted, 0);

        // Member still exists
        assert_eq!(store.get_group_members("evict-active").unwrap().len(), 1);
    }
}
