//! POST /v1/publish — sign and append a message to the local feed.
//!
//! Accepts JSON content, delegates to FeedEngine::publish which signs with
//! the node's Ed25519 key, chains to the previous message, and stores it.
//! The message propagates to peers on the next gossip cycle.

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::api::response;
use crate::api::AppState;
use crate::telemetry;

#[derive(Deserialize)]
pub struct PublishRequest {
    pub content: serde_json::Value,
    /// Schema identifier (e.g., "insight/v1"). If not provided, inferred from content type.
    #[serde(default)]
    pub schema_id: Option<String>,
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
    payload: Result<Json<PublishRequest>, JsonRejection>,
) -> impl IntoResponse {
    let Json(req) = match payload {
        Ok(req) => req,
        Err(rejection) => return response::json_rejection::<()>(rejection).into_response(),
    };

    // Generate trace context if not provided by caller
    let trace_id = req.trace_id.clone().unwrap_or_else(telemetry::generate_trace_id);
    let span_id = req.span_id.clone().unwrap_or_else(telemetry::generate_span_id);

    let identity = state.identity.clone();
    let engine = state.engine.clone();

    let content_type = req
        .content
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    tracing::info!(
        trace_id = %trace_id,
        span_id = %span_id,
        content_type = %content_type,
        "publishing message"
    );

    let result = tokio::task::spawn_blocking(move || {
        engine.publish_full(
            &identity,
            req.content,
            req.schema_id,
            req.relates,
            req.tags,
            Some(trace_id),
            Some(span_id),
        )
    })
    .await
    .map_err(|e| crate::error::EgreError::Config {
        reason: format!("task join error: {e}"),
    });

    match result {
        Ok(Ok(msg)) => {
            tracing::debug!(
                hash = %msg.hash,
                sequence = msg.sequence,
                "message published successfully"
            );
            (StatusCode::CREATED, response::ok(msg)).into_response()
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "publish failed");
            response::from_error(e)
        }
        Err(e) => {
            tracing::warn!(error = %e, "publish task failed");
            response::from_error(e)
        }
    }
}
