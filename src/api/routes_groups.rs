//! Consumer group API routes — create, join, leave, and manage groups.
//!
//! # Security Model
//!
//! Consumer groups are designed for **localhost-only coordination** between
//! processes on the same machine. The API does NOT cryptographically verify
//! the identity of callers — member_id values are accepted on trust.
//!
//! **Do NOT expose these endpoints over a network** without:
//! 1. Adding authentication (API keys, mTLS, etc.)
//! 2. Or requiring signed requests to prove identity ownership
//!
//! For multi-node deployments, use the gossip protocol which authenticates
//! peers via the Secret Handshake.
//!
//! # Endpoints
//!
//! - POST /v1/groups - Create a new consumer group
//! - GET /v1/groups - List all consumer groups
//! - GET /v1/groups/:id - Get a specific consumer group
//! - DELETE /v1/groups/:id - Delete a consumer group
//! - GET /v1/groups/:id/members - List group members
//! - POST /v1/groups/:id/join - Join a consumer group
//! - POST /v1/groups/:id/leave - Leave a consumer group
//! - GET /v1/groups/:id/offsets - Get group offsets
//! - POST /v1/groups/:id/offsets - Commit an offset

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::api::response;
use crate::api::AppState;
use egregore::feed::store::{ConsumerGroup, GroupMember, GroupOffset, JoinGroupResult};
use egregore::identity::PublicId;

/// Response for group listing.
#[derive(Debug, Serialize)]
pub struct GroupInfo {
    pub group_id: String,
    pub generation: u64,
    pub member_count: u64,
    pub created_at: String,
    pub updated_at: String,
}

impl From<ConsumerGroup> for GroupInfo {
    fn from(g: ConsumerGroup) -> Self {
        Self {
            group_id: g.group_id,
            generation: g.generation,
            member_count: 0, // Will be filled in by handler
            created_at: g.created_at.to_rfc3339(),
            updated_at: g.updated_at.to_rfc3339(),
        }
    }
}

/// Response for group member listing.
#[derive(Debug, Serialize)]
pub struct MemberInfo {
    pub member_id: String,
    pub joined_at: String,
    pub assigned_feeds: Vec<String>,
    pub assignment_generation: u64,
}

impl From<GroupMember> for MemberInfo {
    fn from(m: GroupMember) -> Self {
        Self {
            member_id: m.member_id.0,
            joined_at: m.joined_at.to_rfc3339(),
            assigned_feeds: m.assigned_feeds.into_iter().map(|p| p.0).collect(),
            assignment_generation: m.assignment_generation,
        }
    }
}

/// Response for group offset.
#[derive(Debug, Serialize)]
pub struct OffsetInfo {
    pub author: String,
    pub committed_sequence: u64,
    pub committed_at: String,
    pub committed_by: String,
}

impl From<GroupOffset> for OffsetInfo {
    fn from(o: GroupOffset) -> Self {
        Self {
            author: o.author.0,
            committed_sequence: o.committed_sequence,
            committed_at: o.committed_at.to_rfc3339(),
            committed_by: o.committed_by.0,
        }
    }
}

/// Response for join group.
#[derive(Debug, Serialize)]
pub struct JoinGroupResponse {
    pub group_id: String,
    pub member_id: String,
    pub generation: u64,
    pub assigned_feeds: Vec<String>,
    pub is_leader: bool,
}

impl From<JoinGroupResult> for JoinGroupResponse {
    fn from(r: JoinGroupResult) -> Self {
        Self {
            group_id: r.group_id,
            member_id: r.member_id.0,
            generation: r.generation,
            assigned_feeds: r.assigned_feeds.into_iter().map(|p| p.0).collect(),
            is_leader: r.is_leader,
        }
    }
}

/// Request to create a consumer group.
#[derive(Debug, Deserialize)]
pub struct CreateGroupRequest {
    pub group_id: String,
}

/// Request to join a consumer group.
#[derive(Debug, Deserialize)]
pub struct JoinGroupRequest {
    pub member_id: String,
}

/// Request to leave a consumer group.
#[derive(Debug, Deserialize)]
pub struct LeaveGroupRequest {
    pub member_id: String,
}

/// Request to commit an offset.
#[derive(Debug, Deserialize)]
pub struct CommitOffsetRequest {
    pub author: String,
    pub sequence: u64,
    pub committed_by: String,
}

/// Validate group_id format (alphanumeric, hyphens, underscores, max 64 chars).
fn is_valid_group_id(group_id: &str) -> bool {
    !group_id.is_empty()
        && group_id.len() <= 64
        && group_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// POST /v1/groups - Create a new consumer group.
pub async fn create_group(
    State(state): State<AppState>,
    Json(req): Json<CreateGroupRequest>,
) -> impl IntoResponse {
    if !is_valid_group_id(&req.group_id) {
        return response::err::<GroupInfo>(
            StatusCode::BAD_REQUEST,
            "INVALID_GROUP_ID",
            "group_id must be 1-64 alphanumeric characters, hyphens, or underscores",
        )
        .into_response();
    }

    let engine = state.engine.clone();
    let group_id = req.group_id.clone();

    let result = tokio::task::spawn_blocking(move || engine.store().create_group(&group_id)).await;

    match result {
        Ok(Ok(group)) => {
            let info = GroupInfo {
                group_id: group.group_id,
                generation: group.generation,
                member_count: 0,
                created_at: group.created_at.to_rfc3339(),
                updated_at: group.updated_at.to_rfc3339(),
            };
            (StatusCode::CREATED, response::ok(info)).into_response()
        }
        Ok(Err(e)) => {
            if e.to_string().contains("already exists") {
                response::err::<GroupInfo>(
                    StatusCode::CONFLICT,
                    "GROUP_EXISTS",
                    "consumer group already exists",
                )
                .into_response()
            } else {
                response::from_error(e)
            }
        }
        Err(_) => response::err::<GroupInfo>(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "failed to create consumer group",
        )
        .into_response(),
    }
}

/// GET /v1/groups - List all consumer groups.
pub async fn list_groups(State(state): State<AppState>) -> impl IntoResponse {
    let engine = state.engine.clone();

    let result = tokio::task::spawn_blocking(move || {
        let store = engine.store();
        let groups = store.list_groups()?;
        let mut infos = Vec::with_capacity(groups.len());
        for group in groups {
            let member_count = store.group_member_count(&group.group_id)?;
            infos.push(GroupInfo {
                group_id: group.group_id,
                generation: group.generation,
                member_count,
                created_at: group.created_at.to_rfc3339(),
                updated_at: group.updated_at.to_rfc3339(),
            });
        }
        Ok::<_, egregore::error::EgreError>(infos)
    })
    .await;

    match result {
        Ok(Ok(groups)) => response::ok(groups).into_response(),
        Ok(Err(e)) => response::from_error(e),
        Err(_) => response::err::<Vec<GroupInfo>>(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "failed to list consumer groups",
        )
        .into_response(),
    }
}

/// GET /v1/groups/:id - Get a specific consumer group.
pub async fn get_group(
    State(state): State<AppState>,
    Path(group_id): Path<String>,
) -> impl IntoResponse {
    let engine = state.engine.clone();

    let result = tokio::task::spawn_blocking(move || {
        let store = engine.store();
        let group = store.get_group(&group_id)?;
        match group {
            Some(g) => {
                let member_count = store.group_member_count(&g.group_id)?;
                Ok(Some(GroupInfo {
                    group_id: g.group_id,
                    generation: g.generation,
                    member_count,
                    created_at: g.created_at.to_rfc3339(),
                    updated_at: g.updated_at.to_rfc3339(),
                }))
            }
            None => Ok(None),
        }
    })
    .await;

    match result {
        Ok(Ok(Some(group))) => response::ok(group).into_response(),
        Ok(Ok(None)) => response::err::<GroupInfo>(
            StatusCode::NOT_FOUND,
            "GROUP_NOT_FOUND",
            "consumer group not found",
        )
        .into_response(),
        Ok(Err(e)) => response::from_error(e),
        Err(_) => response::err::<GroupInfo>(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "failed to get consumer group",
        )
        .into_response(),
    }
}

/// DELETE /v1/groups/:id - Delete a consumer group.
pub async fn delete_group(
    State(state): State<AppState>,
    Path(group_id): Path<String>,
) -> impl IntoResponse {
    let engine = state.engine.clone();

    let result = tokio::task::spawn_blocking(move || engine.store().delete_group(&group_id)).await;

    match result {
        Ok(Ok(true)) => StatusCode::NO_CONTENT.into_response(),
        Ok(Ok(false)) => response::err::<()>(
            StatusCode::NOT_FOUND,
            "GROUP_NOT_FOUND",
            "consumer group not found",
        )
        .into_response(),
        Ok(Err(e)) => response::from_error(e),
        Err(_) => response::err::<()>(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "failed to delete consumer group",
        )
        .into_response(),
    }
}

/// GET /v1/groups/:id/members - List group members.
pub async fn get_group_members(
    State(state): State<AppState>,
    Path(group_id): Path<String>,
) -> impl IntoResponse {
    let engine = state.engine.clone();

    let result = tokio::task::spawn_blocking(move || {
        let store = engine.store();
        // Check if group exists first
        if store.get_group(&group_id)?.is_none() {
            return Ok(None);
        }
        Ok(Some(store.get_group_members(&group_id)?))
    })
    .await;

    match result {
        Ok(Ok(Some(members))) => {
            let infos: Vec<MemberInfo> = members.into_iter().map(MemberInfo::from).collect();
            response::ok(infos).into_response()
        }
        Ok(Ok(None)) => response::err::<Vec<MemberInfo>>(
            StatusCode::NOT_FOUND,
            "GROUP_NOT_FOUND",
            "consumer group not found",
        )
        .into_response(),
        Ok(Err(e)) => response::from_error(e),
        Err(_) => response::err::<Vec<MemberInfo>>(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "failed to get group members",
        )
        .into_response(),
    }
}

/// POST /v1/groups/:id/join - Join a consumer group.
pub async fn join_group(
    State(state): State<AppState>,
    Path(group_id): Path<String>,
    Json(req): Json<JoinGroupRequest>,
) -> impl IntoResponse {
    if !PublicId::is_valid_format(&req.member_id) {
        return response::err::<JoinGroupResponse>(
            StatusCode::BAD_REQUEST,
            "INVALID_MEMBER_ID",
            "member_id must be in @<base64>.ed25519 format",
        )
        .into_response();
    }

    if !is_valid_group_id(&group_id) {
        return response::err::<JoinGroupResponse>(
            StatusCode::BAD_REQUEST,
            "INVALID_GROUP_ID",
            "group_id must be 1-64 alphanumeric characters, hyphens, or underscores",
        )
        .into_response();
    }

    let engine = state.engine.clone();
    let member_id = PublicId(req.member_id);

    let result = tokio::task::spawn_blocking(move || {
        engine.store().join_group(&group_id, &member_id)
    })
    .await;

    match result {
        Ok(Ok(join_result)) => {
            (StatusCode::OK, response::ok(JoinGroupResponse::from(join_result))).into_response()
        }
        Ok(Err(e)) => response::from_error(e),
        Err(_) => response::err::<JoinGroupResponse>(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "failed to join consumer group",
        )
        .into_response(),
    }
}

/// POST /v1/groups/:id/leave - Leave a consumer group.
pub async fn leave_group(
    State(state): State<AppState>,
    Path(group_id): Path<String>,
    Json(req): Json<LeaveGroupRequest>,
) -> impl IntoResponse {
    if !PublicId::is_valid_format(&req.member_id) {
        return response::err::<()>(
            StatusCode::BAD_REQUEST,
            "INVALID_MEMBER_ID",
            "member_id must be in @<base64>.ed25519 format",
        )
        .into_response();
    }

    let engine = state.engine.clone();
    let member_id = PublicId(req.member_id);

    let result = tokio::task::spawn_blocking(move || {
        engine.store().leave_group(&group_id, &member_id)
    })
    .await;

    match result {
        Ok(Ok(true)) => StatusCode::NO_CONTENT.into_response(),
        Ok(Ok(false)) => response::err::<()>(
            StatusCode::NOT_FOUND,
            "MEMBER_NOT_FOUND",
            "member not found in consumer group",
        )
        .into_response(),
        Ok(Err(e)) => response::from_error(e),
        Err(_) => response::err::<()>(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "failed to leave consumer group",
        )
        .into_response(),
    }
}

/// GET /v1/groups/:id/offsets - Get group offsets.
pub async fn get_group_offsets(
    State(state): State<AppState>,
    Path(group_id): Path<String>,
) -> impl IntoResponse {
    let engine = state.engine.clone();

    let result = tokio::task::spawn_blocking(move || {
        let store = engine.store();
        // Check if group exists first
        if store.get_group(&group_id)?.is_none() {
            return Ok(None);
        }
        Ok(Some(store.get_group_offsets(&group_id)?))
    })
    .await;

    match result {
        Ok(Ok(Some(offsets))) => {
            let infos: Vec<OffsetInfo> = offsets.into_iter().map(OffsetInfo::from).collect();
            response::ok(infos).into_response()
        }
        Ok(Ok(None)) => response::err::<Vec<OffsetInfo>>(
            StatusCode::NOT_FOUND,
            "GROUP_NOT_FOUND",
            "consumer group not found",
        )
        .into_response(),
        Ok(Err(e)) => response::from_error(e),
        Err(_) => response::err::<Vec<OffsetInfo>>(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "failed to get group offsets",
        )
        .into_response(),
    }
}

/// POST /v1/groups/:id/offsets - Commit an offset.
pub async fn commit_offset(
    State(state): State<AppState>,
    Path(group_id): Path<String>,
    Json(req): Json<CommitOffsetRequest>,
) -> impl IntoResponse {
    if !PublicId::is_valid_format(&req.author) {
        return response::err::<OffsetInfo>(
            StatusCode::BAD_REQUEST,
            "INVALID_AUTHOR",
            "author must be in @<base64>.ed25519 format",
        )
        .into_response();
    }

    if !PublicId::is_valid_format(&req.committed_by) {
        return response::err::<OffsetInfo>(
            StatusCode::BAD_REQUEST,
            "INVALID_COMMITTED_BY",
            "committed_by must be in @<base64>.ed25519 format",
        )
        .into_response();
    }

    let engine = state.engine.clone();
    let author = PublicId(req.author);
    let committed_by = PublicId(req.committed_by);
    let sequence = req.sequence;

    let result = tokio::task::spawn_blocking(move || {
        engine
            .store()
            .commit_offset(&group_id, &author, sequence, &committed_by)
    })
    .await;

    match result {
        Ok(Ok(offset)) => response::ok(OffsetInfo::from(offset)).into_response(),
        Ok(Err(e)) => {
            if e.to_string().contains("not assigned") {
                response::err::<OffsetInfo>(
                    StatusCode::FORBIDDEN,
                    "NOT_ASSIGNED",
                    "member is not assigned this feed",
                )
                .into_response()
            } else {
                response::from_error(e)
            }
        }
        Err(_) => response::err::<OffsetInfo>(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "failed to commit offset",
        )
        .into_response(),
    }
}
