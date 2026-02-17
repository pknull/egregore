//! Feed query endpoints — read messages from the local SQLite store.
//!
//! Messages from remote authors appear here only after gossip replication
//! has synced them. All queries are local reads — no network calls.
//!
//! The `/v1/feed` endpoint excludes the local node's own messages by default,
//! returning only messages from other authors (useful for feed watchers).
//! Use `?include_self=true` to include own messages in the results.

use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use serde::Deserialize;

use crate::api::response;
use crate::api::AppState;
use egregore::feed::models::FeedQuery;
use egregore::identity::PublicId;

#[derive(Deserialize, Default)]
pub struct FeedParams {
    pub limit: Option<u32>,
    pub offset: Option<u32>,
    pub content_type: Option<String>,
    /// Include own messages in results (default: false).
    pub include_self: Option<bool>,
}

#[derive(Deserialize, Default)]
pub struct SearchParams {
    pub q: Option<String>,
    pub limit: Option<u32>,
}

pub async fn get_feed(
    State(state): State<AppState>,
    Query(params): Query<FeedParams>,
) -> impl IntoResponse {
    let engine = state.engine.clone();
    let include_self = params.include_self.unwrap_or(false);
    let self_id = state.identity.public_id();

    let result = tokio::task::spawn_blocking(move || {
        engine.query(&FeedQuery {
            author: None,
            exclude_author: if include_self { None } else { Some(self_id) },
            content_type: params.content_type,
            limit: params.limit,
            offset: params.offset,
            ..Default::default()
        })
    })
    .await;

    match result {
        Ok(Ok(msgs)) => {
            let meta = response::ApiMetadata {
                total: None,
                limit: params.limit,
                offset: params.offset,
            };
            response::ok_with_metadata(msgs, meta).into_response()
        }
        Ok(Err(e)) => response::from_error(e),
        Err(_) => response::err::<()>(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "failed to query feed",
        )
        .into_response(),
    }
}

pub async fn get_feed_by_author(
    State(state): State<AppState>,
    Path(author): Path<String>,
    Query(params): Query<FeedParams>,
) -> impl IntoResponse {
    let engine = state.engine.clone();

    let result = tokio::task::spawn_blocking(move || {
        engine.query(&FeedQuery {
            author: Some(PublicId(author)),
            content_type: params.content_type,
            limit: params.limit,
            offset: params.offset,
            ..Default::default()
        })
    })
    .await;

    match result {
        Ok(Ok(msgs)) => response::ok(msgs).into_response(),
        Ok(Err(e)) => response::from_error(e),
        Err(_) => response::err::<()>(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "failed to query feed",
        )
        .into_response(),
    }
}

pub async fn get_insights(
    State(state): State<AppState>,
    Query(params): Query<FeedParams>,
) -> impl IntoResponse {
    let engine = state.engine.clone();

    let result = tokio::task::spawn_blocking(move || {
        engine.query(&FeedQuery {
            content_type: Some("insight".to_string()),
            limit: params.limit,
            offset: params.offset,
            ..Default::default()
        })
    })
    .await;

    match result {
        Ok(Ok(msgs)) => response::ok(msgs).into_response(),
        Ok(Err(e)) => response::from_error(e),
        Err(_) => response::err::<()>(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "failed to query insights",
        )
        .into_response(),
    }
}

pub async fn search_insights(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> impl IntoResponse {
    let engine = state.engine.clone();
    let query_text = params.q.unwrap_or_default();
    let limit = params.limit.unwrap_or(20).min(200);

    if query_text.is_empty() {
        return response::err::<()>(
            axum::http::StatusCode::BAD_REQUEST,
            "MISSING_QUERY",
            "query parameter 'q' is required",
        )
        .into_response();
    }

    let result =
        tokio::task::spawn_blocking(move || engine.search(&query_text, limit)).await;

    match result {
        Ok(Ok(msgs)) => response::ok(msgs).into_response(),
        Ok(Err(e)) => response::from_error(e),
        Err(_) => response::err::<()>(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "failed to search insights",
        )
        .into_response(),
    }
}

pub async fn get_message_by_hash(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> impl IntoResponse {
    let engine = state.engine.clone();

    let result = tokio::task::spawn_blocking(move || engine.get_message(&hash)).await;

    match result {
        Ok(Ok(Some(msg))) => response::ok(msg).into_response(),
        Ok(Ok(None)) => response::err::<()>(
            axum::http::StatusCode::NOT_FOUND,
            "NOT_FOUND",
            "message not found",
        )
        .into_response(),
        Ok(Err(e)) => response::from_error(e),
        Err(_) => response::err::<()>(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "failed to retrieve message",
        )
        .into_response(),
    }
}
