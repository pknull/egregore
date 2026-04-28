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
use crate::feed::models::FeedQuery;
use crate::feed::models::Message;
use crate::identity::PublicId;

#[derive(Deserialize, Default)]
pub struct FeedParams {
    pub limit: Option<u32>,
    pub offset: Option<u32>,
    pub content_type: Option<String>,
    /// Filter by distributed trace identifier.
    pub trace_id: Option<String>,
    /// Filter by tag.
    pub tag: Option<String>,
    /// Filter by related message hash (find replies/responses).
    pub relates: Option<String>,
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
    let identity = state.identity.clone();
    let include_self = params.include_self.unwrap_or(false);
    let self_id = state.identity.public_id();
    let limit = params.limit;
    let offset = params.offset;

    let msgs = match response::run_blocking(
        move || {
            engine.query(&FeedQuery {
                author: None,
                exclude_author: if include_self { None } else { Some(self_id) },
                content_type: params.content_type,
                trace_id: params.trace_id,
                tag: params.tag,
                relates: params.relates,
                limit,
                offset,
                ..Default::default()
            })
        },
        "failed to query feed",
    )
    .await
    {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let msgs: Vec<Message> = msgs
        .into_iter()
        .map(|msg| crate::feed::private_box::decrypt_or_passthrough(&identity, msg))
        .collect();
    let meta = response::ApiMetadata {
        total: None,
        limit,
        offset,
    };
    response::ok_with_metadata(msgs, meta).into_response()
}

pub async fn get_feed_by_author(
    State(state): State<AppState>,
    Path(author): Path<String>,
    Query(params): Query<FeedParams>,
) -> impl IntoResponse {
    let engine = state.engine.clone();
    let identity = state.identity.clone();

    let msgs = match response::run_blocking(
        move || {
            engine.query(&FeedQuery {
                author: Some(PublicId(author)),
                content_type: params.content_type,
                trace_id: params.trace_id,
                limit: params.limit,
                offset: params.offset,
                ..Default::default()
            })
        },
        "failed to query feed",
    )
    .await
    {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let msgs: Vec<Message> = msgs
        .into_iter()
        .map(|msg| crate::feed::private_box::decrypt_or_passthrough(&identity, msg))
        .collect();
    response::ok(msgs).into_response()
}

pub async fn get_insights(
    State(state): State<AppState>,
    Query(params): Query<FeedParams>,
) -> impl IntoResponse {
    let engine = state.engine.clone();
    let identity = state.identity.clone();

    let msgs = match response::run_blocking(
        move || {
            engine.query(&FeedQuery {
                content_type: Some("insight".to_string()),
                trace_id: params.trace_id,
                limit: params.limit,
                offset: params.offset,
                ..Default::default()
            })
        },
        "failed to query insights",
    )
    .await
    {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let msgs: Vec<Message> = msgs
        .into_iter()
        .map(|msg| crate::feed::private_box::decrypt_or_passthrough(&identity, msg))
        .collect();
    response::ok(msgs).into_response()
}

pub async fn search_insights(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> impl IntoResponse {
    let engine = state.engine.clone();
    let identity = state.identity.clone();
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

    let msgs = match response::run_blocking(
        move || engine.search(&query_text, limit),
        "failed to search insights",
    )
    .await
    {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let msgs: Vec<Message> = msgs
        .into_iter()
        .map(|msg| crate::feed::private_box::decrypt_or_passthrough(&identity, msg))
        .collect();
    response::ok(msgs).into_response()
}

pub async fn get_message_by_hash(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> impl IntoResponse {
    let engine = state.engine.clone();
    let identity = state.identity.clone();

    let msg = match response::run_blocking(
        move || engine.get_message(&hash),
        "failed to retrieve message",
    )
    .await
    {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    match msg {
        Some(msg) => response::ok(crate::feed::private_box::decrypt_or_passthrough(
            &identity, msg,
        ))
        .into_response(),
        None => response::err::<()>(
            axum::http::StatusCode::NOT_FOUND,
            "NOT_FOUND",
            "message not found",
        )
        .into_response(),
    }
}
