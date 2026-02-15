use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use serde::Serialize;

use crate::api::RelayState;

#[derive(Serialize)]
pub struct DirectoryEntry {
    pub public_id: String,
    pub nickname: Option<String>,
    pub last_connected: Option<String>,
}

pub async fn get_directory(State(state): State<RelayState>) -> impl IntoResponse {
    let engine = state.engine.clone();

    let result = tokio::task::spawn_blocking(move || {
        // Only return non-private, authorized peers
        engine.store().list_peers(false)
    })
    .await;

    super::routes_register::from_blocking(result, |peers| {
        let entries: Vec<DirectoryEntry> = peers
            .into_iter()
            .map(|p| DirectoryEntry {
                public_id: p.public_id.0,
                nickname: p.nickname,
                last_connected: p.last_connected.map(|dt| dt.to_rfc3339()),
            })
            .collect();
        Json(serde_json::json!({
            "success": true,
            "data": entries,
        }))
        .into_response()
    })
}
