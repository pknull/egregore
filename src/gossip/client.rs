//! Gossip sync loop — outgoing connections to known peers.
//!
//! Each cycle: merge CLI peers + DB peers → build replication config → sync each.
//! A single `ReplicationConfig` is built per cycle from the follows table:
//! - Empty follows → replicate ALL feeds (open replication, the default)
//! - Any follows added → only request those authors
//!
//! Per-peer sync: TCP connect → SHS handshake → bidirectional replication → close.
//! The replication protocol (Have/Want/Messages/Done) runs over Box Stream.
//! See gossip/replication.rs for the wire protocol.
//!
//! With push_enabled: After replication, negotiate persistent mode. If accepted,
//! split the connection and register for push-based message broadcasting.
//! Failed connections use exponential backoff to avoid overwhelming peers.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{Duration as ChronoDuration, Utc};
use tokio::net::TcpStream;
use tracing::Instrument;

use crate::feed::engine::FeedEngine;
use crate::gossip::backoff::ExponentialBackoff;
use crate::gossip::bloom::BloomConfig;
use crate::gossip::connection::SecureConnection;
use crate::gossip::health::HEALTH_EVICTION_HOURS;
use crate::gossip::persistent::PersistentConnectionTask;
use crate::gossip::registry::{ConnectionHandle, ConnectionRegistry};
use crate::gossip::replication::{self, ReplicationConfig};
use crate::identity::{Identity, PublicId};
use crate::metrics;
use crate::telemetry;

/// Max time for a single peer sync (connect + handshake + replication).
const PEER_SYNC_TIMEOUT: Duration = Duration::from_secs(60);

/// Configuration for the sync loop.
#[derive(Clone)]
pub struct SyncConfig {
    /// Static peers from CLI.
    pub static_peers: Vec<String>,
    /// Network key for SHS.
    pub network_key: [u8; 32],
    /// Local identity.
    pub identity: Identity,
    /// Sync interval.
    pub interval: Duration,
    /// Enable persistent push connections.
    pub push_enabled: bool,
    /// Initial backoff delay for failed connections.
    pub backoff_initial: Duration,
    /// Maximum backoff delay for failed connections.
    pub backoff_max: Duration,
}

impl SyncConfig {
    /// Create a new config with default backoff settings.
    pub fn new(
        static_peers: Vec<String>,
        network_key: [u8; 32],
        identity: Identity,
        interval: Duration,
        push_enabled: bool,
    ) -> Self {
        Self {
            static_peers,
            network_key,
            identity,
            interval,
            push_enabled,
            backoff_initial: Duration::from_secs(5),
            backoff_max: Duration::from_secs(300),
        }
    }
}

/// Tracks backoff state for a peer address.
struct PeerBackoff {
    backoff: ExponentialBackoff,
    /// When the current backoff expires (peer can be retried).
    retry_after: Option<Instant>,
}

/// Periodically sync with known peers.
/// `static_peers` are CLI-provided addresses that never change.
/// Additional peers are read from the database each iteration.
pub async fn run_sync_loop(
    static_peers: Vec<String>,
    network_key: [u8; 32],
    identity: Identity,
    engine: Arc<FeedEngine>,
    interval: Duration,
) {
    let config = SyncConfig::new(static_peers, network_key, identity, interval, false);
    run_sync_loop_with_push(config, engine, None).await;
}

/// Periodically sync with known peers, with optional push support.
pub async fn run_sync_loop_with_push(
    config: SyncConfig,
    engine: Arc<FeedEngine>,
    registry: Option<Arc<ConnectionRegistry>>,
) {
    tracing::info!(
        static_peer_count = config.static_peers.len(),
        interval_secs = config.interval.as_secs(),
        push_enabled = config.push_enabled,
        "gossip sync loop started"
    );

    // Track backoff state per peer address
    let mut peer_backoffs: HashMap<String, PeerBackoff> = HashMap::new();

    loop {
        // Evict stale health records (Consul-style 72-hour TTL)
        let evict_engine = engine.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || {
            let cutoff = Utc::now() - ChronoDuration::hours(HEALTH_EVICTION_HOURS);
            match evict_engine.store().evict_stale_health(&cutoff) {
                Ok(count) if count > 0 => {
                    tracing::debug!(evicted = count, "evicted stale health records");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to evict stale health records");
                }
                _ => {}
            }
        })
        .await
        {
            tracing::warn!(error = %e, "health eviction task failed");
        }

        // Build peer list: static CLI peers + DB peers
        let mut peer_set: HashSet<String> = config.static_peers.iter().cloned().collect();

        let db_engine = engine.clone();
        match tokio::task::spawn_blocking(move || db_engine.store().list_all_syncable_addresses())
            .await
        {
            Ok(Ok(db_peers)) => {
                peer_set.extend(db_peers);
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "failed to read peers from database");
            }
            Err(e) => {
                tracing::warn!(error = %e, "spawn_blocking failed for peer lookup");
            }
        }

        if peer_set.is_empty() {
            tracing::debug!("no peers available, waiting for next cycle");
            tokio::time::sleep(config.interval).await;
            continue;
        }

        // Clean up backoff entries for peers no longer in the peer set
        peer_backoffs.retain(|addr, _| peer_set.contains(addr));

        // Build replication config from current follows
        let repl_config = build_replication_config(&engine).await;

        for peer_addr in &peer_set {
            // Check if this peer is in backoff
            let now = Instant::now();
            if let Some(backoff) = peer_backoffs.get(peer_addr) {
                if let Some(retry_after) = backoff.retry_after {
                    if now < retry_after {
                        tracing::trace!(
                            peer = %peer_addr,
                            retry_in_secs = (retry_after - now).as_secs(),
                            "peer in backoff, skipping"
                        );
                        continue;
                    }
                }
            }

            let result =
                sync_one_peer(peer_addr, &config, &engine, &repl_config, registry.as_ref()).await;

            // Update backoff based on result
            match result {
                SyncResult::Success => {
                    // Reset backoff on success
                    if let Some(backoff) = peer_backoffs.get_mut(peer_addr) {
                        backoff.backoff.reset();
                        backoff.retry_after = None;
                    }
                }
                SyncResult::Failed => {
                    // Apply backoff on failure
                    let backoff =
                        peer_backoffs
                            .entry(peer_addr.clone())
                            .or_insert_with(|| PeerBackoff {
                                backoff: ExponentialBackoff::new(
                                    config.backoff_initial,
                                    config.backoff_max,
                                ),
                                retry_after: None,
                            });
                    let delay = backoff.backoff.next_delay();
                    backoff.retry_after = Some(now + delay);

                    if backoff.backoff.is_extended() {
                        // Deduplicate per peer to prevent flooding
                        crate::dedup_warn!(
                            "peer_extended_backoff",
                            peer_addr,
                            peer = %peer_addr,
                            attempt = backoff.backoff.attempt(),
                            next_retry_secs = delay.as_secs(),
                            "peer consistently failing, extended backoff"
                        );
                    } else {
                        tracing::debug!(
                            peer = %peer_addr,
                            next_retry_secs = delay.as_secs(),
                            "backing off from peer"
                        );
                    }
                }
            }
        }
        tokio::time::sleep(config.interval).await;
    }
}

/// Result of a sync attempt.
enum SyncResult {
    Success,
    Failed,
}

pub(crate) async fn build_replication_config(engine: &Arc<FeedEngine>) -> ReplicationConfig {
    let eng = engine.clone();
    let result = tokio::task::spawn_blocking(move || {
        let store = eng.store();
        let follows = store.get_follows();
        let topics = store.get_topic_subscriptions();
        (follows, topics)
    })
    .await;

    match result {
        Ok((Ok(follows), Ok(topics))) => {
            let follows_set = if follows.is_empty() {
                None
            } else {
                Some(follows.into_iter().collect())
            };
            let topics_set = if topics.is_empty() {
                None
            } else {
                Some(topics.into_iter().collect())
            };
            ReplicationConfig {
                follows: follows_set,
                topics: topics_set,
                bloom_config: BloomConfig::default(),
            }
        }
        Ok((Ok(follows), Err(e))) => {
            tracing::warn!(error = %e, "failed to read topics, using follows only");
            ReplicationConfig {
                follows: if follows.is_empty() {
                    None
                } else {
                    Some(follows.into_iter().collect())
                },
                topics: None,
                bloom_config: BloomConfig::default(),
            }
        }
        Ok((Err(e), _)) => {
            // FAIL-CLOSED: Don't widen scope on error. Use empty follows set.
            // This prevents unintended broad replication when storage fails.
            tracing::error!(
                error = %e,
                "failed to read follows, using fail-closed policy (no replication)"
            );
            ReplicationConfig {
                follows: Some(std::collections::HashSet::new()), // Empty = replicate no one
                topics: None,
                bloom_config: BloomConfig::default(),
            }
        }
        Err(e) => {
            // FAIL-CLOSED: Task panic should not expand scope.
            tracing::error!(
                error = %e,
                "replication config task failed, using fail-closed policy (no replication)"
            );
            ReplicationConfig {
                follows: Some(std::collections::HashSet::new()), // Empty = replicate no one
                topics: None,
                bloom_config: BloomConfig::default(),
            }
        }
    }
}

/// Sync with a single peer, with timeout and result logging.
/// Returns SyncResult for backoff tracking.
async fn sync_one_peer(
    peer_addr: &str,
    config: &SyncConfig,
    engine: &Arc<FeedEngine>,
    repl_config: &ReplicationConfig,
    registry: Option<&Arc<ConnectionRegistry>>,
) -> SyncResult {
    // Generate trace context for this sync cycle
    let trace_id = telemetry::generate_trace_id();
    let span_id = telemetry::generate_span_id();

    let span = tracing::info_span!(
        "gossip_sync",
        peer = %peer_addr,
        trace_id = %trace_id,
        span_id = %span_id,
    );

    async {
        tracing::debug!("initiating gossip sync");
        let sync_result = tokio::time::timeout(
            PEER_SYNC_TIMEOUT,
            sync_with_peer(peer_addr, config, engine, repl_config, registry),
        )
        .await;
        match sync_result {
            Ok(Ok(SyncOutcome::PullComplete(remote_id))) => {
                tracing::info!(remote_id = %remote_id.0, "gossip sync complete (pull mode)");
                update_peer_health(engine, peer_addr, &remote_id).await;
                SyncResult::Success
            }
            Ok(Ok(SyncOutcome::PersistentEstablished(remote_id))) => {
                tracing::info!(remote_id = %remote_id.0, "gossip sync complete (persistent mode)");
                update_peer_health(engine, peer_addr, &remote_id).await;
                SyncResult::Success
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "gossip sync failed");
                SyncResult::Failed
            }
            Err(_) => {
                tracing::warn!("gossip sync timed out");
                SyncResult::Failed
            }
        }
    }
    .instrument(span)
    .await
}

/// Outcome of a peer sync operation.
enum SyncOutcome {
    /// Pull-only replication completed and connection closed.
    PullComplete(PublicId),
    /// Persistent connection established (task spawned).
    PersistentEstablished(PublicId),
}

/// Sync with a peer and optionally establish persistent connection.
async fn sync_with_peer(
    addr: &str,
    config: &SyncConfig,
    engine: &Arc<FeedEngine>,
    repl_config: &ReplicationConfig,
    registry: Option<&Arc<ConnectionRegistry>>,
) -> crate::error::Result<SyncOutcome> {
    let stream = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(addr))
        .await
        .map_err(|_| {
            crate::error::EgreError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("TCP connect to {addr} timed out"),
            ))
        })??;

    let peer_addr = stream.peer_addr()?;
    let mut conn =
        SecureConnection::connect(stream, config.network_key, config.identity.clone()).await?;
    let remote_pub_id = PublicId::from_verifying_key(&conn.remote_public_key);

    // Run standard replication
    replication::replicate_as_client(&mut conn, engine, repl_config).await?;

    // Attempt persistent mode if enabled
    if config.push_enabled {
        if let Some(reg) = registry {
            // Check if we're already connected to this peer
            if reg.is_connected(&remote_pub_id) {
                tracing::debug!(
                    peer = %remote_pub_id.0,
                    "already have persistent connection, closing"
                );
                conn.close().await?;
                return Ok(SyncOutcome::PullComplete(remote_pub_id));
            }

            // Negotiate persistent mode
            if replication::negotiate_persistent_mode_client(&mut conn).await? {
                // Split connection for concurrent I/O
                let (reader, writer) = conn.into_split();

                // Register the writer half
                let handle = ConnectionHandle::new(writer, remote_pub_id.clone(), peer_addr, true);

                if reg.register(handle) {
                    // Update peer connection metrics
                    metrics::set_peers_connected(reg.connection_count());

                    // Spawn task to handle incoming Push messages
                    let reg_for_task = reg.clone();
                    let task = PersistentConnectionTask::new(
                        reader,
                        remote_pub_id.clone(),
                        engine.clone(),
                        reg.clone(),
                    );
                    tokio::spawn(async move {
                        if let Err(e) = task.run().await {
                            tracing::debug!(error = %e, "persistent connection task ended");
                        }
                        // Update metrics when connection ends
                        metrics::set_peers_connected(reg_for_task.connection_count());
                    });

                    return Ok(SyncOutcome::PersistentEstablished(remote_pub_id));
                } else {
                    // Registration failed (at capacity or duplicate)
                    // Reader/writer will be dropped, connection closes automatically
                    tracing::debug!(
                        peer = %remote_pub_id.0,
                        "failed to register persistent connection"
                    );
                    return Ok(SyncOutcome::PullComplete(remote_pub_id));
                }
            }
        }
    }

    // Fall back to pull-only mode
    conn.close().await?;
    Ok(SyncOutcome::PullComplete(remote_pub_id))
}

/// Update peer health records after successful sync.
async fn update_peer_health(engine: &Arc<FeedEngine>, peer_addr: &str, remote_id: &PublicId) {
    let addr = peer_addr.to_string();
    let eng = engine.clone();
    let pub_id_str = remote_id.0.clone();
    let pub_id_for_health = remote_id.clone();

    if let Err(e) = tokio::task::spawn_blocking(move || {
        let store = eng.store();
        // Update address peer sync timestamp
        if let Err(e) = store.update_address_peer_synced(&addr, &pub_id_str) {
            tracing::warn!(error = %e, "failed to update peer sync timestamp");
        }

        // Record direct observation for mesh health
        let their_seq = store.get_latest_sequence(&pub_id_for_health).unwrap_or(0);
        let our_gen = store.get_local_generation().unwrap_or(0);
        if let Err(e) = store.record_direct_observation(&pub_id_for_health, their_seq, our_gen) {
            tracing::warn!(error = %e, "failed to record direct observation");
        }
    })
    .await
    {
        tracing::warn!(error = %e, "peer sync update task failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::PublicId;

    /// Verify that a ReplicationConfig with empty follows set rejects all authors.
    /// This is the fail-closed behavior - replicate no one.
    #[test]
    fn fail_closed_config_rejects_all_authors() {
        let config = ReplicationConfig {
            follows: Some(std::collections::HashSet::new()), // Empty = no one
            topics: None,
            bloom_config: BloomConfig::default(),
        };

        // Should reject any author since follows is empty
        let test_author = PublicId("@TEST123.ed25519".to_string());
        assert!(
            !config.wants_author(&test_author),
            "fail-closed config should reject all authors"
        );
    }

    /// Verify that default config (None follows) accepts all authors.
    /// This is fail-open behavior we want to avoid on errors.
    #[test]
    fn default_config_accepts_all_authors() {
        let config = ReplicationConfig::default();

        // Should accept any author since follows is None
        let test_author = PublicId("@TEST123.ed25519".to_string());
        assert!(
            config.wants_author(&test_author),
            "default config should accept all authors"
        );
    }

    /// Verify distinction between None (all) and Some(empty) (none).
    #[test]
    fn none_vs_empty_follows_semantics() {
        let test_author = PublicId("@TEST123.ed25519".to_string());

        // None means "replicate all" (no filter)
        let all_config = ReplicationConfig {
            follows: None,
            topics: None,
            bloom_config: BloomConfig::default(),
        };
        assert!(all_config.wants_author(&test_author));

        // Some(empty) means "replicate none" (filter, but empty list)
        let none_config = ReplicationConfig {
            follows: Some(std::collections::HashSet::new()),
            topics: None,
            bloom_config: BloomConfig::default(),
        };
        assert!(!none_config.wants_author(&test_author));

        // Some with entries means "replicate only these"
        let mut follows = std::collections::HashSet::new();
        follows.insert(test_author.clone());
        let selective_config = ReplicationConfig {
            follows: Some(follows),
            topics: None,
            bloom_config: BloomConfig::default(),
        };
        assert!(selective_config.wants_author(&test_author));
        assert!(!selective_config.wants_author(&PublicId("@OTHER.ed25519".to_string())));
    }
}
