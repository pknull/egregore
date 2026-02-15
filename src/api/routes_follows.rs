use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Serialize;

use crate::api::response;
use crate::api::AppState;
use egregore::identity::PublicId;

#[derive(Serialize)]
pub struct FollowInfo {
    pub author: String,
}

pub async fn add_follow(
    State(state): State<AppState>,
    Path(author): Path<String>,
) -> impl IntoResponse {
    if !PublicId::is_valid_format(&author) {
        return response::err::<FollowInfo>(
            StatusCode::BAD_REQUEST,
            "INVALID_AUTHOR",
            "author must be in @<base64>.ed25519 format",
        )
        .into_response();
    }
    let engine = state.engine.clone();
    let author_id = PublicId(author.clone());

    let result = tokio::task::spawn_blocking(move || engine.store().add_follow(&author_id)).await;

    match result {
        Ok(Ok(())) => (StatusCode::CREATED, response::ok(FollowInfo { author })).into_response(),
        Ok(Err(e)) => response::from_error(e),
        Err(_) => response::err::<()>(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "failed to add follow",
        )
        .into_response(),
    }
}

pub async fn remove_follow(
    State(state): State<AppState>,
    Path(author): Path<String>,
) -> impl IntoResponse {
    if !PublicId::is_valid_format(&author) {
        return response::err::<()>(
            StatusCode::BAD_REQUEST,
            "INVALID_AUTHOR",
            "author must be in @<base64>.ed25519 format",
        )
        .into_response();
    }
    let engine = state.engine.clone();
    let author_id = PublicId(author);

    let result =
        tokio::task::spawn_blocking(move || engine.store().remove_follow(&author_id)).await;

    match result {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(e)) => response::from_error(e),
        Err(_) => response::err::<()>(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "failed to remove follow",
        )
        .into_response(),
    }
}

pub async fn get_follows(State(state): State<AppState>) -> impl IntoResponse {
    let engine = state.engine.clone();

    let result = tokio::task::spawn_blocking(move || engine.store().get_follows()).await;

    match result {
        Ok(Ok(follows)) => {
            let infos: Vec<FollowInfo> = follows
                .into_iter()
                .map(|pid| FollowInfo { author: pid.0 })
                .collect();
            response::ok(infos).into_response()
        }
        Ok(Err(e)) => response::from_error(e),
        Err(_) => response::err::<()>(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "failed to list follows",
        )
        .into_response(),
    }
}
