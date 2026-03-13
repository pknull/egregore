use std::collections::{HashMap, HashSet};
use std::time::Instant;

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use crate::config::Config;
use crate::error::Result;
use crate::feed::engine::FeedEngine;
use crate::gossip::health::{PeerHealthStatus, DIRECT_OBSERVATION_MARKER};
use crate::gossip::registry::ConnectionRegistry;
use crate::identity::Identity;

#[derive(Debug, Clone, Serialize)]
pub struct NodeStatusMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub node: String,
    pub ts: DateTime<Utc>,
    pub version: String,
    pub uptime_secs: u64,
    pub peers: NodeStatusPeers,
    pub storage: NodeStatusStorage,
    pub throughput: NodeStatusThroughput,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeStatusPeers {
    pub connected: usize,
    pub known: u64,
    pub health: Vec<NodeStatusPeerHealth>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeStatusPeerHealth {
    pub peer: String,
    pub status: String,
    pub lag_msgs: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeStatusStorage {
    pub bytes: u64,
    pub message_count: u64,
    pub feed_count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeStatusThroughput {
    pub msgs_in_last_hour: u64,
    pub msgs_out_last_hour: u64,
}

pub fn build_node_status(
    identity: &Identity,
    config: &Config,
    engine: &FeedEngine,
    registry: Option<&ConnectionRegistry>,
    started_at: Instant,
) -> Result<NodeStatusMessage> {
    let store = engine.store();
    let now = Utc::now();
    let connected_peers: HashSet<String> = registry
        .map(|r| r.connected_peers().into_iter().map(|p| p.0).collect())
        .unwrap_or_default();

    let mut peer_health: HashMap<String, PeerHealthEntry> = HashMap::new();
    for record in store.get_all_peer_health()? {
        let peer_id = record.peer_id.0.clone();
        let age_secs = (now - record.last_seen_at).num_seconds().max(0) as u64;
        let status = if connected_peers.contains(&peer_id) {
            "connected".to_string()
        } else {
            peer_health_label(PeerHealthStatus::from_age_secs(
                age_secs,
                config.gossip_interval_secs,
            ))
            .to_string()
        };
        let lag_msgs = record
            .last_seq
            .saturating_sub(store.get_latest_contiguous_sequence(&record.peer_id)?);
        let candidate = PeerHealthEntry {
            peer: peer_id.clone(),
            status,
            lag_msgs,
            observed_at: record.last_seen_at,
            direct: record.last_seen_by == DIRECT_OBSERVATION_MARKER,
        };

        let replace = peer_health
            .get(&peer_id)
            .map(|existing| {
                (candidate.status == "connected" && existing.status != "connected")
                    || (candidate.direct && !existing.direct)
                    || candidate.observed_at > existing.observed_at
            })
            .unwrap_or(true);

        if replace {
            peer_health.insert(peer_id, candidate);
        }
    }

    for peer in &connected_peers {
        peer_health
            .entry(peer.clone())
            .or_insert_with(|| PeerHealthEntry {
                peer: peer.clone(),
                status: "connected".to_string(),
                lag_msgs: 0,
                observed_at: now,
                direct: true,
            });
    }

    let message_count = store.message_count()?;
    let feed_count = store.feed_count()?;
    let known_peers = [
        config.peers.len(),
        connected_peers.len(),
        peer_health.len(),
        store.peer_count()? as usize,
    ]
    .into_iter()
    .max()
    .unwrap_or(0) as u64;
    let db_bytes = std::fs::metadata(config.db_path())
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let (msgs_in_last_hour, msgs_out_last_hour) =
        store.message_flow_counts_since(&(now - Duration::hours(1)), &identity.public_id())?;

    let mut health: Vec<NodeStatusPeerHealth> = peer_health
        .into_values()
        .map(|entry| NodeStatusPeerHealth {
            peer: entry.peer,
            status: entry.status,
            lag_msgs: entry.lag_msgs,
        })
        .collect();
    health.sort_by(|a, b| a.peer.cmp(&b.peer));

    Ok(NodeStatusMessage {
        msg_type: "node_status".to_string(),
        node: identity.public_id().0,
        ts: now,
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_secs: started_at.elapsed().as_secs(),
        peers: NodeStatusPeers {
            connected: connected_peers.len(),
            known: known_peers,
            health,
        },
        storage: NodeStatusStorage {
            bytes: db_bytes,
            message_count,
            feed_count,
        },
        throughput: NodeStatusThroughput {
            msgs_in_last_hour,
            msgs_out_last_hour,
        },
    })
}

fn peer_health_label(status: PeerHealthStatus) -> &'static str {
    match status {
        PeerHealthStatus::Recent => "recent",
        PeerHealthStatus::Stale => "stale",
        PeerHealthStatus::Suspected => "suspected",
        PeerHealthStatus::Unknown => "unknown",
    }
}

struct PeerHealthEntry {
    peer: String,
    status: String,
    lag_msgs: u64,
    observed_at: DateTime<Utc>,
    direct: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::store::FeedStore;
    use crate::identity::Identity;

    #[test]
    fn build_node_status_includes_storage_and_throughput() {
        let identity = Identity::generate();
        let store = FeedStore::open_memory().unwrap();
        let engine = FeedEngine::new(store);
        engine
            .publish(
                &identity,
                serde_json::json!({"type":"message","text":"hello"}),
                None,
                vec![],
            )
            .unwrap();

        let status =
            build_node_status(&identity, &Config::default(), &engine, None, Instant::now())
                .unwrap();

        assert_eq!(status.msg_type, "node_status");
        assert_eq!(status.storage.message_count, 1);
        assert_eq!(status.storage.feed_count, 1);
        assert_eq!(status.throughput.msgs_out_last_hour, 1);
    }
}
