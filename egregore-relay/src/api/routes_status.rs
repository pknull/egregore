use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use serde::Serialize;

use crate::api::RelayState;

#[derive(Serialize)]
pub struct RelayStatus {
    pub version: String,
    pub identity: String,
    pub authorized_peers: u64,
    pub message_count: u64,
    pub feed_count: u64,
    pub ttl_days: u32,
    pub uptime_secs: u64,
}

pub async fn get_status(State(state): State<RelayState>) -> impl IntoResponse {
    let engine = state.engine.clone();
    let uptime = state.started_at.elapsed().as_secs();
    let ttl_days = state.relay_config.ttl_days;

    let result = tokio::task::spawn_blocking(move || {
        let store = engine.store();
        let message_count = store.message_count().unwrap_or(0);
        let feed_count = store.feed_count().unwrap_or(0);
        let peer_count = store.peer_count().unwrap_or(0);
        (message_count, feed_count, peer_count)
    })
    .await;

    let (message_count, feed_count, authorized_peers) = result.unwrap_or((0, 0, 0));

    Json(serde_json::json!({
        "success": true,
        "data": RelayStatus {
            version: env!("CARGO_PKG_VERSION").to_string(),
            identity: state.identity.public_id().0,
            authorized_peers,
            message_count,
            feed_count,
            ttl_days,
            uptime_secs: uptime,
        },
    }))
    .into_response()
}
