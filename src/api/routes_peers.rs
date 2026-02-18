use axum::extract::{Path, State};
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
    pub source: String,
}

#[derive(Deserialize)]
pub struct AddPeerRequest {
    pub address: String,
}

/// Validate a peer address is in host:port format.
pub fn is_valid_peer_address(address: &str) -> bool {
    let parts: Vec<&str> = address.rsplitn(2, ':').collect();
    parts.len() == 2 && parts[0].parse::<u16>().is_ok() && !parts[1].is_empty()
}

#[derive(Debug, Serialize)]
pub struct StatusInfo {
    pub version: String,
    pub identity: String,
    pub port: u16,
    pub gossip_port: u16,
    pub peer_count: usize,
    pub message_count: u64,
    pub feed_count: u64,
    pub follow_count: u64,
    pub uptime_secs: u64,
}

/// Aggregate peers from all sources (CLI, address table, known_peers) with deduplication.
pub async fn aggregate_peers(state: &AppState) -> Vec<PeerInfo> {
    let engine = state.engine.clone();
    let cli_peers = state.config.peers.clone();

    let result = tokio::task::spawn_blocking(move || {
        let store = engine.store();
        let address_peers = store.list_address_peers().unwrap_or_default();
        let known_peers = store.list_peers(true).unwrap_or_default();
        (address_peers, known_peers)
    })
    .await;

    let (address_peers, known_peers) = result.unwrap_or_default();

    let mut peers: Vec<PeerInfo> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for addr in &cli_peers {
        seen.insert(addr.clone());
        peers.push(PeerInfo {
            address: addr.clone(),
            public_id: None,
            source: "cli".to_string(),
        });
    }

    for ap in &address_peers {
        if seen.insert(ap.address.clone()) {
            peers.push(PeerInfo {
                address: ap.address.clone(),
                public_id: ap.public_id.clone(),
                source: "manual".to_string(),
            });
        }
    }

    for kp in &known_peers {
        if let Some(ref addr) = kp.address {
            if seen.insert(addr.clone()) {
                peers.push(PeerInfo {
                    address: addr.clone(),
                    public_id: Some(kp.public_id.0.clone()),
                    source: "known".to_string(),
                });
            }
        }
    }

    peers
}

pub async fn get_peers(State(state): State<AppState>) -> impl IntoResponse {
    let peers = aggregate_peers(&state).await;
    response::ok(peers).into_response()
}

pub async fn add_peer(
    State(state): State<AppState>,
    Json(req): Json<AddPeerRequest>,
) -> impl IntoResponse {
    if !is_valid_peer_address(&req.address) {
        return response::err::<PeerInfo>(
            StatusCode::BAD_REQUEST,
            "INVALID_ADDRESS",
            "address must be in host:port format",
        )
        .into_response();
    }

    let address = req.address.clone();
    let engine = state.engine.clone();

    let result = tokio::task::spawn_blocking(move || {
        engine.store().insert_address_peer(&address)
    })
    .await;

    match result {
        Ok(Ok(())) => (
            StatusCode::CREATED,
            response::ok(PeerInfo {
                address: req.address,
                public_id: None,
                source: "manual".to_string(),
            }),
        )
            .into_response(),
        Ok(Err(e)) => response::from_error(e),
        Err(_) => response::err::<PeerInfo>(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "failed to persist peer",
        )
        .into_response(),
    }
}

pub async fn delete_peer(
    State(state): State<AppState>,
    Path(address): Path<String>,
) -> impl IntoResponse {
    if !is_valid_peer_address(&address) {
        return response::err::<()>(
            StatusCode::BAD_REQUEST,
            "INVALID_ADDRESS",
            "address must be in host:port format",
        )
        .into_response();
    }

    let engine = state.engine.clone();

    let result = tokio::task::spawn_blocking(move || {
        engine.store().remove_address_peer(&address)
    })
    .await;

    match result {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(e)) => response::from_error(e),
        Err(_) => response::err::<()>(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "failed to remove peer",
        )
        .into_response(),
    }
}

/// Build status info from current state. Shared by HTTP route and MCP tool.
pub async fn build_status(state: &AppState) -> StatusInfo {
    let engine = state.engine.clone();
    let uptime = state.started_at.elapsed().as_secs();
    let peer_count = aggregate_peers(state).await.len();

    let result = tokio::task::spawn_blocking(move || {
        let store = engine.store();
        let message_count = store.message_count().unwrap_or(0);
        let feed_count = store.feed_count().unwrap_or(0);
        let follow_count = store.follow_count().unwrap_or(0);
        (message_count, feed_count, follow_count)
    })
    .await;

    let (message_count, feed_count, follow_count) = result.unwrap_or((0, 0, 0));

    StatusInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        identity: state.identity.public_id().0,
        port: state.config.port,
        gossip_port: state.config.gossip_port,
        peer_count,
        message_count,
        feed_count,
        follow_count,
        uptime_secs: uptime,
    }
}

pub async fn get_status(State(state): State<AppState>) -> impl IntoResponse {
    response::ok(build_status(&state).await).into_response()
}
