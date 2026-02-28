//! Retention policy API routes.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::api::response;
use crate::api::AppState;
use egregore::feed::store::retention::{RetentionPolicy, RetentionScope};

/// API request for creating a retention policy.
#[derive(Debug, Deserialize)]
pub struct CreatePolicyRequest {
    /// What this policy applies to.
    pub scope: ScopeInput,
    /// Maximum age in seconds.
    #[serde(default)]
    pub max_age_secs: Option<u64>,
    /// Maximum number of messages to keep.
    #[serde(default)]
    pub max_count: Option<u64>,
    /// Maximum total size in bytes.
    #[serde(default)]
    pub max_bytes: Option<u64>,
    /// JSON path for compaction key.
    #[serde(default)]
    pub compact_key: Option<String>,
}

/// Scope specification for API requests.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ScopeInput {
    /// Simple string "global".
    Simple(String),
    /// Object with scope type.
    Object(ScopeObject),
}

#[derive(Debug, Deserialize)]
pub struct ScopeObject {
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub content_type: Option<String>,
}

impl ScopeInput {
    fn to_retention_scope(&self) -> Result<RetentionScope, &'static str> {
        match self {
            ScopeInput::Simple(s) if s == "global" => Ok(RetentionScope::Global),
            ScopeInput::Simple(_) => Err("invalid scope string, use \"global\" or an object"),
            ScopeInput::Object(obj) => {
                if let Some(ref topic) = obj.topic {
                    Ok(RetentionScope::Topic(topic.clone()))
                } else if let Some(ref author) = obj.author {
                    Ok(RetentionScope::Author(author.clone()))
                } else if let Some(ref ct) = obj.content_type {
                    Ok(RetentionScope::ContentType(ct.clone()))
                } else {
                    Err("scope object must have one of: topic, author, content_type")
                }
            }
        }
    }
}

/// API response for a retention policy.
#[derive(Debug, Serialize)]
pub struct PolicyResponse {
    pub id: i64,
    pub scope: ScopeResponse,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_age_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compact_key: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ScopeResponse {
    Global(String),
    Topic { topic: String },
    Author { author: String },
    ContentType { content_type: String },
}

impl From<&RetentionScope> for ScopeResponse {
    fn from(scope: &RetentionScope) -> Self {
        match scope {
            RetentionScope::Global => ScopeResponse::Global("global".to_string()),
            RetentionScope::Topic(t) => ScopeResponse::Topic { topic: t.clone() },
            RetentionScope::Author(a) => ScopeResponse::Author { author: a.clone() },
            RetentionScope::ContentType(ct) => ScopeResponse::ContentType {
                content_type: ct.clone(),
            },
        }
    }
}

impl From<RetentionPolicy> for PolicyResponse {
    fn from(p: RetentionPolicy) -> Self {
        PolicyResponse {
            id: p.id.unwrap_or(0),
            scope: ScopeResponse::from(&p.scope),
            max_age_secs: p.max_age_secs,
            max_count: p.max_count,
            max_bytes: p.max_bytes,
            compact_key: p.compact_key,
        }
    }
}

/// List all retention policies.
pub async fn list_policies(State(state): State<AppState>) -> impl IntoResponse {
    let engine = state.engine.clone();

    let result =
        tokio::task::spawn_blocking(move || engine.store().list_retention_policies()).await;

    match result {
        Ok(Ok(policies)) => {
            let responses: Vec<PolicyResponse> =
                policies.into_iter().map(PolicyResponse::from).collect();
            response::ok(responses).into_response()
        }
        Ok(Err(e)) => response::from_error(e),
        Err(_) => response::err::<()>(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "failed to list retention policies",
        )
        .into_response(),
    }
}

/// Create a retention policy.
pub async fn create_policy(
    State(state): State<AppState>,
    Json(req): Json<CreatePolicyRequest>,
) -> impl IntoResponse {
    // Validate scope
    let scope = match req.scope.to_retention_scope() {
        Ok(s) => s,
        Err(msg) => {
            return response::err::<()>(StatusCode::BAD_REQUEST, "INVALID_SCOPE", msg)
                .into_response()
        }
    };

    // Require at least one retention criterion
    if req.max_age_secs.is_none()
        && req.max_count.is_none()
        && req.max_bytes.is_none()
        && req.compact_key.is_none()
    {
        return response::err::<()>(
            StatusCode::BAD_REQUEST,
            "MISSING_CRITERION",
            "policy must have at least one of: max_age_secs, max_count, max_bytes, compact_key",
        )
        .into_response();
    }

    let policy = RetentionPolicy {
        id: None,
        scope,
        max_age_secs: req.max_age_secs,
        max_count: req.max_count,
        max_bytes: req.max_bytes,
        compact_key: req.compact_key.clone(),
    };

    let engine = state.engine.clone();
    let policy_clone = policy.clone();
    let result =
        tokio::task::spawn_blocking(move || engine.store().save_retention_policy(&policy_clone))
            .await;

    match result {
        Ok(Ok(id)) => {
            let mut resp = PolicyResponse::from(policy);
            resp.id = id;
            (StatusCode::CREATED, response::ok(resp)).into_response()
        }
        Ok(Err(e)) => response::from_error(e),
        Err(_) => response::err::<()>(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "failed to create retention policy",
        )
        .into_response(),
    }
}

/// Delete a retention policy by ID.
pub async fn delete_policy(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let engine = state.engine.clone();

    let result =
        tokio::task::spawn_blocking(move || engine.store().delete_retention_policy(id)).await;

    match result {
        Ok(Ok(true)) => StatusCode::NO_CONTENT.into_response(),
        Ok(Ok(false)) => response::err::<()>(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            "retention policy not found",
        )
        .into_response(),
        Ok(Err(e)) => response::from_error(e),
        Err(_) => response::err::<()>(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "failed to delete retention policy",
        )
        .into_response(),
    }
}
