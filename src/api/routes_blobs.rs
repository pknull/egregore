//! Blob store REST endpoints.
//!
//! - `POST /v1/blobs` — Upload a blob (application/octet-stream body)
//! - `GET /v1/blobs/:hash` — Download a blob
//! - `GET /v1/blobs/:hash/info` — Blob metadata

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use super::response::{self, ApiResponse};
use super::AppState;
use crate::blob::BlobInfo;

/// Maximum blob size: 100 MB.
const MAX_BLOB_SIZE: usize = 100 * 1024 * 1024;

/// Upload a blob. Body is raw binary (application/octet-stream).
pub async fn upload_blob(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Json<ApiResponse<BlobInfo>>, Response> {
    if body.is_empty() {
        return Err(response::err::<BlobInfo>(
            StatusCode::BAD_REQUEST,
            "EMPTY_BODY",
            "blob body must not be empty",
        )
        .into_response());
    }

    if body.len() > MAX_BLOB_SIZE {
        return Err(response::err::<BlobInfo>(
            StatusCode::PAYLOAD_TOO_LARGE,
            "PAYLOAD_TOO_LARGE",
            "blob exceeds 100 MB limit",
        )
        .into_response());
    }

    let blob_store = state.blob_store.clone();
    let data = body.to_vec();

    let result = tokio::task::spawn_blocking(move || blob_store.put(&data))
        .await
        .map_err(|_| {
            response::err::<BlobInfo>(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "blob storage task failed",
            )
            .into_response()
        })?
        .map_err(|e| {
            tracing::warn!(error = %e, "blob put failed");
            response::err::<BlobInfo>(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "failed to store blob",
            )
            .into_response()
        })?;

    Ok(response::ok(result))
}

/// Download a blob by hash. Returns raw binary with application/octet-stream.
pub async fn download_blob(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> Result<Response, Response> {
    let blob_store = state.blob_store.clone();
    let h = hash.clone();

    let data = tokio::task::spawn_blocking(move || blob_store.get(&h))
        .await
        .map_err(|_| {
            response::err::<()>(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "blob read task failed",
            )
            .into_response()
        })?
        .map_err(|e| {
            tracing::warn!(error = %e, "blob get failed");
            response::err::<()>(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "failed to read blob",
            )
            .into_response()
        })?;

    match data {
        Some(bytes) => Ok((
            [(header::CONTENT_TYPE, "application/octet-stream")],
            bytes,
        )
            .into_response()),
        None => Err(response::err::<()>(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            &format!("blob not found: {hash}"),
        )
        .into_response()),
    }
}

/// Get blob metadata without downloading content.
pub async fn blob_info(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> Result<Json<ApiResponse<BlobInfo>>, Response> {
    let blob_store = state.blob_store.clone();
    let h = hash.clone();

    let info = tokio::task::spawn_blocking(move || blob_store.info(&h))
        .await
        .map_err(|_| {
            response::err::<BlobInfo>(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "blob info task failed",
            )
            .into_response()
        })?
        .map_err(|e| {
            tracing::warn!(error = %e, "blob info failed");
            response::err::<BlobInfo>(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "failed to read blob info",
            )
            .into_response()
        })?;

    match info {
        Some(bi) => Ok(response::ok(bi)),
        None => Err(response::err::<BlobInfo>(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            &format!("blob not found: {hash}"),
        )
        .into_response()),
    }
}
