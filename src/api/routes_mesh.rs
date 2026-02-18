//! Mesh health endpoint — aggregate peer visibility across the network.

use std::collections::HashMap;

use axum::extract::State;
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use serde::Serialize;

use egregore::gossip::health::{PeerHealthStatus, DIRECT_OBSERVATION_MARKER};

use super::response;
use super::routes_peers::{build_status, StatusInfo};
use super::AppState;

/// Mesh-wide health response combining local status with peer observations.
#[derive(Debug, Serialize)]
pub struct MeshHealthResponse {
    pub local: StatusInfo,
    pub peers: Vec<MeshPeerInfo>,
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

    let health_records = result
        .unwrap_or_else(|_| Ok(Vec::new()))
        .unwrap_or_default();
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

    MeshHealthResponse {
        local: local_status,
        peers,
    }
}

pub async fn get_mesh(State(state): State<AppState>) -> impl IntoResponse {
    response::ok(build_mesh_health(&state).await).into_response()
}
