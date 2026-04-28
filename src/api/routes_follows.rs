use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Serialize;

use crate::api::response;
use crate::api::AppState;
use crate::identity::PublicId;

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

    if let Err(resp) = response::run_blocking(
        move || engine.store().add_follow(&author_id),
        "failed to add follow",
    )
    .await
    {
        return resp;
    }

    (StatusCode::CREATED, response::ok(FollowInfo { author })).into_response()
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

    if let Err(resp) = response::run_blocking(
        move || engine.store().remove_follow(&author_id),
        "failed to remove follow",
    )
    .await
    {
        return resp;
    }

    StatusCode::NO_CONTENT.into_response()
}

pub async fn get_follows(State(state): State<AppState>) -> impl IntoResponse {
    let engine = state.engine.clone();

    let follows = match response::run_blocking(
        move || engine.store().get_follows(),
        "failed to list follows",
    )
    .await
    {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let infos: Vec<FollowInfo> = follows
        .into_iter()
        .map(|pid| FollowInfo { author: pid.0 })
        .collect();
    response::ok(infos).into_response()
}
