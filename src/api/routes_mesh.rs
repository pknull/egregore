//! Mesh health endpoint — aggregate peer visibility across the network.

use std::collections::HashMap;

use axum::extract::State;
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::gossip::health::{PeerHealthStatus, DIRECT_OBSERVATION_MARKER};

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

    let peers: Vec<MeshPeerInfo> = peer_map.into_values().collect();

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
