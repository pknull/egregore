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

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::{Duration as ChronoDuration, Utc};
use tokio::net::TcpStream;

use crate::feed::engine::FeedEngine;
use crate::gossip::connection::SecureConnection;
use crate::gossip::health::HEALTH_EVICTION_HOURS;
use crate::gossip::replication::{self, ReplicationConfig};
use crate::identity::Identity;

/// Max time for a single peer sync (connect + handshake + replication).
const PEER_SYNC_TIMEOUT: Duration = Duration::from_secs(60);

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
    tracing::info!(
        static_peer_count = static_peers.len(),
        interval_secs = interval.as_secs(),
        "gossip sync loop started"
    );

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
        let mut peer_set: HashSet<String> = static_peers.iter().cloned().collect();

        let db_engine = engine.clone();
        match tokio::task::spawn_blocking(move || {
            db_engine.store().list_all_syncable_addresses()
        })
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
            tokio::time::sleep(interval).await;
            continue;
        }

        // Build replication config from current follows
        let config = build_replication_config(&engine).await;

        for peer_addr in &peer_set {
            sync_one_peer(peer_addr, network_key, &identity, &engine, &config).await;
        }
        tokio::time::sleep(interval).await;
    }
}

pub(crate) async fn build_replication_config(engine: &Arc<FeedEngine>) -> ReplicationConfig {
    let eng = engine.clone();
    let result = tokio::task::spawn_blocking(move || eng.store().get_follows()).await;
    match result {
        Ok(Ok(follows)) if !follows.is_empty() => ReplicationConfig {
            follows: Some(follows.into_iter().collect()),
        },
        Ok(Ok(_)) => ReplicationConfig::default(), // no follows = replicate all
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "failed to read follows, replicating all feeds");
            ReplicationConfig::default()
        }
        Err(e) => {
            tracing::warn!(error = %e, "follows task failed, replicating all feeds");
            ReplicationConfig::default()
        }
    }
}

/// Sync with a single peer, with timeout and result logging.
async fn sync_one_peer(
    peer_addr: &str,
    network_key: [u8; 32],
    identity: &Identity,
    engine: &Arc<FeedEngine>,
    config: &ReplicationConfig,
) {
    tracing::debug!(peer = %peer_addr, "initiating gossip sync");
    let sync_result = tokio::time::timeout(
        PEER_SYNC_TIMEOUT,
        sync_with_peer(peer_addr, network_key, identity.clone(), engine, config),
    )
    .await;
    match sync_result {
        Ok(Ok(remote_id)) => {
            tracing::info!(peer = %peer_addr, "gossip sync complete");
            if let Some(pub_id_str) = remote_id {
                let addr = peer_addr.to_string();
                let eng = engine.clone();
                let pub_id_for_health = pub_id_str.clone();
                if let Err(e) = tokio::task::spawn_blocking(move || {
                    let store = eng.store();
                    // Update address peer sync timestamp
                    if let Err(e) = store.update_address_peer_synced(&addr, &pub_id_str) {
                        tracing::warn!(error = %e, "failed to update peer sync timestamp");
                    }

                    // Record direct observation for mesh health
                    let pub_id = crate::identity::PublicId(pub_id_for_health);
                    let their_seq = store.get_latest_sequence(&pub_id).unwrap_or(0);
                    let our_gen = store.get_local_generation().unwrap_or(0);
                    if let Err(e) = store.record_direct_observation(&pub_id, their_seq, our_gen) {
                        tracing::warn!(error = %e, "failed to record direct observation");
                    }
                })
                .await
                {
                    tracing::warn!(error = %e, "peer sync update task failed");
                }
            }
        }
        Ok(Err(e)) => tracing::warn!(peer = %peer_addr, error = %e, "gossip sync failed"),
        Err(_) => tracing::warn!(peer = %peer_addr, "gossip sync timed out"),
    }
}

/// Returns the remote peer's public ID string on success.
async fn sync_with_peer(
    addr: &str,
    network_key: [u8; 32],
    identity: Identity,
    engine: &Arc<FeedEngine>,
    config: &ReplicationConfig,
) -> crate::error::Result<Option<String>> {
    let stream = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(addr))
        .await
        .map_err(|_| crate::error::EgreError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("TCP connect to {addr} timed out"),
        )))?
        ?;
    let mut conn = SecureConnection::connect(stream, network_key, identity).await?;
    let remote_pub_id = crate::identity::PublicId::from_verifying_key(&conn.remote_public_key);
    replication::replicate_as_client(&mut conn, engine, config).await?;
    conn.close().await?;
    Ok(Some(remote_pub_id.0))
}
