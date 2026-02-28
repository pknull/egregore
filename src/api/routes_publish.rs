//! POST /v1/publish — sign and append a message to the local feed.
//!
//! Accepts JSON content, delegates to FeedEngine::publish which signs with
//! the node's Ed25519 key, chains to the previous message, and stores it.
//! The message propagates to peers on the next gossip cycle.

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
    /// Hash of a related message (optional, for threading).
    #[serde(default)]
    pub relates: Option<String>,
    /// Categorization tags (optional).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Distributed tracing: trace identifier (optional).
    #[serde(default)]
    pub trace_id: Option<String>,
    /// Distributed tracing: span identifier (optional).
    #[serde(default)]
    pub span_id: Option<String>,
}

pub async fn publish(
    State(state): State<AppState>,
    Json(req): Json<PublishRequest>,
) -> impl IntoResponse {
    let identity = state.identity.clone();
    let engine = state.engine.clone();

    let result =
        tokio::task::spawn_blocking(move || {
            engine.publish_with_trace(
                &identity,
                req.content,
                req.relates,
                req.tags,
                req.trace_id,
                req.span_id,
            )
        })
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
