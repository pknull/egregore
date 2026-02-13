use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::api::response;
use crate::api::AppState;

#[derive(Serialize)]
pub struct PeerInfo {
    pub address: String,
    pub public_id: Option<String>,
}

#[derive(Deserialize)]
pub struct AddPeerRequest {
    pub address: String,
}

#[derive(Serialize)]
pub struct StatusInfo {
    pub version: String,
    pub identity: String,
    pub port: u16,
    pub gossip_port: u16,
    pub peer_count: usize,
}

pub async fn get_peers(State(state): State<AppState>) -> impl IntoResponse {
    let peers: Vec<PeerInfo> = state
        .config
        .peers
        .iter()
        .map(|addr| PeerInfo {
            address: addr.clone(),
            public_id: None,
        })
        .collect();

    response::ok(peers).into_response()
}

pub async fn add_peer(
    State(state): State<AppState>,
    Json(req): Json<AddPeerRequest>,
) -> impl IntoResponse {
    // For MVP, peers are in-memory only (config not persisted)
    // In later phases, this would update the peer store
    let _ = state; // acknowledge state
    (
        StatusCode::CREATED,
        response::ok(PeerInfo {
            address: req.address,
            public_id: None,
        }),
    )
        .into_response()
}

pub async fn get_status(State(state): State<AppState>) -> impl IntoResponse {
    response::ok(StatusInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        identity: state.identity.public_id().0,
        port: state.config.port,
        gossip_port: state.config.gossip_port,
        peer_count: state.config.peers.len(),
    })
    .into_response()
}
