//! Mesh health endpoint — aggregate peer visibility across the network.

use std::collections::HashMap;

use axum::extract::State;
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::feed::profile_lifecycle::{peer_profile_validity, SystemClock};
use crate::gossip::health::{PeerHealthStatus, DIRECT_OBSERVATION_MARKER};
use crate::identity::PublicId;

use super::response;
use super::routes_peers::{build_status, HealthIndicators, StatusInfo, SubsystemHealth};
use super::AppState;

/// Mesh-wide health response combining local status with peer observations.
#[derive(Debug, Serialize)]
pub struct MeshHealthResponse {
    pub local: StatusInfo,
    pub peers: Vec<MeshPeerInfo>,
    /// Health indicators for mesh query. Absent means healthy (backward compat).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<HealthIndicators>,
}

/// Individual peer health info in mesh view.
#[derive(Debug, Serialize)]
pub struct MeshPeerInfo {
    pub peer_id: String,
    pub last_seen_at: Option<DateTime<Utc>>,
    /// "self" for direct observation, or reporter's peer_id for transitive.
    pub last_seen_by: String,
    pub observation_age_secs: Option<u64>,
    pub last_seq: u64,
    pub generation: u32,
    pub status: PeerHealthStatus,
    /// RFC 0001 §11.2 soft-filter flag: `true` when the peer's most-recent
    /// Profile has `valid_until < now`. The peer is NOT removed from the
    /// response — expiration is a trust/freshness signal, not a connectivity
    /// gate. `Absent` and `Undated` (pre-upgrade peer) both render as `false`.
    ///
    /// `#[serde(default)]` on the outgoing struct is a mild safety net for
    /// downstream deserializers that predate this field.
    #[serde(default)]
    pub profile_expired: bool,
}

/// Build mesh health info from current state. Shared by HTTP route and MCP tool.
pub async fn build_mesh_health(state: &AppState) -> MeshHealthResponse {
    let local_status = build_status(state).await;
    let sync_interval = state.config.gossip_interval_secs;

    let engine = state.engine.clone();
    let result = tokio::task::spawn_blocking(move || engine.store().get_all_peer_health()).await;

    // Track errors for health reporting instead of silently flattening
    let (health_records, store_health) = match result {
        Ok(Ok(records)) => (records, SubsystemHealth::Healthy),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "mesh health query failed: store error");
            (
                Vec::new(),
                SubsystemHealth::Degraded {
                    error: format!("peer_health: {}", e),
                },
            )
        }
        Err(e) => {
            tracing::warn!(error = %e, "mesh health query failed: task join error");
            (
                Vec::new(),
                SubsystemHealth::Degraded {
                    error: format!("task_join: {}", e),
                },
            )
        }
    };

    let now = Utc::now();

    // Group by peer_id, pick best observation per peer
    let mut peer_map: HashMap<String, MeshPeerInfo> = HashMap::new();

    for record in health_records {
        let age_secs = (now - record.last_seen_at).num_seconds().max(0) as u64;
        let status = PeerHealthStatus::from_age_secs(age_secs, sync_interval);

        let entry = peer_map
            .entry(record.peer_id.0.clone())
            .or_insert_with(|| MeshPeerInfo {
                peer_id: record.peer_id.0.clone(),
                last_seen_at: Some(record.last_seen_at),
                last_seen_by: record.last_seen_by.clone(),
                observation_age_secs: Some(age_secs),
                last_seq: record.last_seq,
                generation: record.generation,
                status,
                // Soft-filter flag populated below once all peers are known,
                // so we pay the lookup cost once per unique peer rather than
                // once per (peer, reporter) row.
                profile_expired: false,
            });

        // Prefer direct observations over transitive
        // Among same type, prefer more recent
        let entry_is_direct = entry.last_seen_by == DIRECT_OBSERVATION_MARKER;
        let record_is_direct = record.last_seen_by == DIRECT_OBSERVATION_MARKER;
        let prefer_new = (record_is_direct && !entry_is_direct)
            || (record_is_direct == entry_is_direct
                && record.last_seen_at > entry.last_seen_at.unwrap_or(DateTime::<Utc>::MIN_UTC));

        if prefer_new {
            entry.last_seen_at = Some(record.last_seen_at);
            entry.last_seen_by = record.last_seen_by;
            entry.observation_age_secs = Some(age_secs);
            entry.last_seq = record.last_seq;
            entry.generation = record.generation;
            entry.status = status;
        }
    }

    let mut peers: Vec<MeshPeerInfo> = peer_map.into_values().collect();

    // RFC 0001 §11.2 peer-side soft filter: surface `profile_expired` per peer.
    // The peer is NEVER removed from the response — expiration is a
    // trust/freshness signal, not a connectivity gate. Undated (pre-upgrade)
    // peers render as `profile_expired: false` (§6.1 deliberate asymmetry).
    //
    // Lookups run on the blocking pool because `peer_profile_validity` issues
    // a rusqlite query per peer; hopping back through the runtime per-peer
    // would thrash. We collect the peer IDs, batch-query on a single blocking
    // task, then merge back into the response.
    let peer_ids: Vec<PublicId> = peers.iter().map(|p| PublicId(p.peer_id.clone())).collect();
    let validity_engine = state.engine.clone();
    let validity_map: HashMap<String, bool> = tokio::task::spawn_blocking(move || {
        let clock = SystemClock;
        let mut map = HashMap::with_capacity(peer_ids.len());
        for pid in peer_ids {
            // On lookup error, default to `false` (don't falsely mark expired
            // because of a transient store error) and log for operators.
            match peer_profile_validity(&validity_engine, &pid, &clock) {
                Ok(v) => {
                    map.insert(pid.0, v.is_expired());
                }
                Err(e) => {
                    tracing::warn!(
                        peer_id = %pid.0,
                        error = %e,
                        "peer_profile_validity lookup failed; defaulting profile_expired=false"
                    );
                    map.insert(pid.0, false);
                }
            }
        }
        map
    })
    .await
    .unwrap_or_else(|e| {
        tracing::warn!(error = %e, "peer_profile_validity join failed; all peers default to profile_expired=false");
        HashMap::new()
    });

    for peer in peers.iter_mut() {
        if let Some(&expired) = validity_map.get(&peer.peer_id) {
            peer.profile_expired = expired;
        }
    }

    // Only include health field when degraded (backward compat)
    let health = if store_health.is_healthy() {
        None
    } else {
        Some(HealthIndicators {
            healthy: false,
            store: store_health,
        })
    };

    MeshHealthResponse {
        local: local_status,
        peers,
        health,
    }
}

pub async fn get_mesh(State(state): State<AppState>) -> impl IntoResponse {
    response::ok(build_mesh_health(&state).await).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthy_mesh_response_omits_health_field() {
        let response = MeshHealthResponse {
            local: StatusInfo {
                version: "1.0.0".to_string(),
                identity: "@test.ed25519".to_string(),
                port: 7654,
                gossip_port: 7655,
                peer_count: 0,
                message_count: 0,
                feed_count: 0,
                follow_count: 0,
                uptime_secs: 0,
                health: None,
                transport: None,
            },
            peers: vec![],
            health: None,
        };
        let json = serde_json::to_value(&response).unwrap();
        assert!(
            json.get("health").is_none(),
            "healthy mesh should omit health field"
        );
    }

    #[test]
    fn degraded_mesh_response_includes_health_field() {
        let response = MeshHealthResponse {
            local: StatusInfo {
                version: "1.0.0".to_string(),
                identity: "@test.ed25519".to_string(),
                port: 7654,
                gossip_port: 7655,
                peer_count: 0,
                message_count: 0,
                feed_count: 0,
                follow_count: 0,
                uptime_secs: 0,
                health: None,
                transport: None,
            },
            peers: vec![],
            health: Some(HealthIndicators {
                healthy: false,
                store: SubsystemHealth::Degraded {
                    error: "peer_health: connection refused".to_string(),
                },
            }),
        };
        let json = serde_json::to_value(&response).unwrap();
        let health = json
            .get("health")
            .expect("degraded mesh should include health field");
        assert_eq!(health["healthy"], false);
        assert_eq!(
            health["store"]["degraded"]["error"],
            "peer_health: connection refused"
        );
    }
}
