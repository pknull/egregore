//! Phase 2 Wave 5 Step 26 (amendment §C.14) — transport-observability
//! endpoints consumed by scry's bridge panel.
//!
//! Two endpoints:
//!
//! - `GET /v1/transport/pending?transport_id=bus` — count + up to 100
//!   pending-forwarding rows for the named transport. Feeds scry's
//!   per-stream lag widget.
//! - `GET /v1/transport/bus/authors` — per-author summary of
//!   `bus_author_seq_index`, one row per author with
//!   `{author, last_indexed_at, author_seq, stream_seq}`. Feeds scry's
//!   last-ack-per-author widget.
//!
//! Both wrap SQLite access in `spawn_blocking` per the async-SQLite
//! convention documented in CLAUDE.md.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::api::response;
use crate::api::AppState;

// ----------------------------------------------------------------------
// GET /v1/transport/pending
// ----------------------------------------------------------------------

/// Query string: `?transport_id=bus`.
#[derive(Debug, Deserialize)]
pub struct PendingQuery {
    pub transport_id: String,
}

/// One row of pending-forwarding state, suitable for operator display.
/// `last_error` has already been scrubbed at the CRUD layer (auditor A2).
#[derive(Debug, Serialize)]
pub struct PendingRowDto {
    pub message_hash: String,
    pub author: String,
    pub sequence: u64,
    pub enqueued_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub attempt_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// Response envelope wrapped by `response::ok(..)`.
#[derive(Debug, Serialize)]
pub struct PendingSummary {
    pub transport_id: String,
    pub count: u64,
    /// Capped at 100 rows for operator display; use MCP/CLI for deep
    /// inspection.
    pub rows: Vec<PendingRowDto>,
}

/// UI row-cap to keep the response bounded.
const PENDING_LIST_LIMIT: u32 = 100;

pub async fn get_transport_pending(
    State(state): State<AppState>,
    Query(params): Query<PendingQuery>,
) -> impl IntoResponse {
    let engine = state.engine.clone();
    let transport_id = params.transport_id;

    let tid = transport_id.clone();
    let result = tokio::task::spawn_blocking(move || {
        let store = engine.store();
        let count = store.pending_forwarding_count(&tid)?;
        let rows = store.pending_forwarding_list(&tid, PENDING_LIST_LIMIT)?;
        crate::error::Result::Ok((count, rows))
    })
    .await;

    match result {
        Ok(Ok((count, rows))) => {
            let dtos: Vec<PendingRowDto> = rows
                .into_iter()
                .map(|r| PendingRowDto {
                    message_hash: r.message_hash,
                    author: r.author,
                    sequence: r.sequence,
                    enqueued_at: r.enqueued_at,
                    last_attempt_at: r.last_attempt_at,
                    attempt_count: r.attempt_count,
                    last_error: r.last_error,
                })
                .collect();
            response::ok(PendingSummary {
                transport_id,
                count,
                rows: dtos,
            })
            .into_response()
        }
        Ok(Err(e)) => response::from_error(e),
        Err(join_err) => {
            tracing::error!(error = %join_err, "transport/pending task failed");
            response::err::<PendingSummary>(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "failed to read pending-forwarding state",
            )
            .into_response()
        }
    }
}

// ----------------------------------------------------------------------
// GET /v1/transport/bus/authors
// ----------------------------------------------------------------------

/// One row per author from `bus_author_seq_index`, carrying the
/// author's highest indexed `(author_seq, stream_seq)` pair + the
/// timestamp of the last index write.
#[derive(Debug, Serialize)]
pub struct BusAuthorRowDto {
    pub author: String,
    pub last_indexed_at: DateTime<Utc>,
    pub author_seq: u64,
    pub stream_seq: u64,
}

#[derive(Debug, Serialize)]
pub struct BusAuthorsSummary {
    pub rows: Vec<BusAuthorRowDto>,
}

pub async fn get_bus_authors(State(state): State<AppState>) -> impl IntoResponse {
    let engine = state.engine.clone();
    let result = tokio::task::spawn_blocking(move || {
        engine.store().bus_author_seq_index_author_summary()
    })
    .await;

    match result {
        Ok(Ok(rows)) => {
            let dtos = rows
                .into_iter()
                .map(|r| BusAuthorRowDto {
                    author: r.author,
                    last_indexed_at: r.last_indexed_at,
                    author_seq: r.author_seq,
                    stream_seq: r.stream_seq,
                })
                .collect();
            response::ok(BusAuthorsSummary { rows: dtos }).into_response()
        }
        Ok(Err(e)) => response::from_error(e),
        Err(join_err) => {
            tracing::error!(error = %join_err, "transport/bus/authors task failed");
            response::err::<BusAuthorsSummary>(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "failed to read bus_author_seq_index",
            )
            .into_response()
        }
    }
}
