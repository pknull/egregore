use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use egregore::feed::models::FeedQuery;
use egregore::identity::PublicId;

use crate::api::RelayState;

#[derive(Deserialize, Default)]
pub struct FeedParams {
    pub page: Option<u32>,
    pub per_page: Option<u32>,
    pub since_sequence: Option<u64>,
}

pub async fn get_firehose(
    State(state): State<RelayState>,
    Query(params): Query<FeedParams>,
) -> impl IntoResponse {
    let engine = state.engine.clone();
    let per_page = params.per_page.unwrap_or(50).min(200);
    let page = params.page.unwrap_or(1).max(1);
    let offset = (page - 1).saturating_mul(per_page);

    let result = tokio::task::spawn_blocking(move || {
        engine.query(&FeedQuery {
            author: None,
            content_type: None,
            tag: None,
            limit: Some(per_page),
            offset: Some(offset),
            search: None,
        })
    })
    .await;

    super::routes_register::from_blocking(result, |msgs| {
        Json(serde_json::json!({
            "success": true,
            "data": msgs,
            "metadata": {
                "page": page,
                "per_page": per_page,
            },
        }))
        .into_response()
    })
}

pub async fn get_feed_by_author(
    State(state): State<RelayState>,
    Path(author): Path<String>,
    Query(params): Query<FeedParams>,
) -> impl IntoResponse {
    let engine = state.engine.clone();
    let since = params.since_sequence.unwrap_or(0);
    let limit = params.per_page.unwrap_or(50).min(200);
    let pub_id = PublicId(author);

    let result = tokio::task::spawn_blocking(move || {
        engine.store().get_messages_after(&pub_id, since, limit)
    })
    .await;

    super::routes_register::from_blocking(result, |msgs| {
        Json(serde_json::json!({
            "success": true,
            "data": msgs,
        }))
        .into_response()
    })
}
