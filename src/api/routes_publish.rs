use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::api::response;
use crate::api::AppState;

#[derive(Deserialize)]
pub struct PublishRequest {
    pub content: serde_json::Value,
}

pub async fn publish(
    State(state): State<AppState>,
    Json(req): Json<PublishRequest>,
) -> impl IntoResponse {
    let identity = state.identity.clone();
    let engine = state.engine.clone();

    let result = tokio::task::spawn_blocking(move || engine.publish(&identity, req.content))
        .await
        .map_err(|e| egregore::error::EgreError::Config {
            reason: format!("task join error: {e}"),
        });

    match result {
        Ok(Ok(msg)) => (StatusCode::CREATED, response::ok(msg)).into_response(),
        Ok(Err(e)) => response::from_error(e),
        Err(e) => response::from_error(e),
    }
}
