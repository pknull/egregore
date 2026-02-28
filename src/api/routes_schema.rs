//! GET/POST /v1/schemas — schema registry management.
//!
//! Provides endpoints for listing registered schemas, getting schema details,
//! and registering custom schemas.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::api::response;
use crate::api::AppState;

use egregore::feed::schema::{Codec, CompatibilityMode, SchemaDefinition};

/// Response for schema listing.
#[derive(Serialize)]
pub struct SchemaListResponse {
    pub schemas: Vec<SchemaInfo>,
}

/// Summary info about a schema.
#[derive(Serialize)]
pub struct SchemaInfo {
    pub schema_id: String,
    pub content_type: String,
    pub version: u32,
    pub codec: Codec,
    pub compatibility: CompatibilityMode,
    pub description: Option<String>,
}

impl From<&SchemaDefinition> for SchemaInfo {
    fn from(s: &SchemaDefinition) -> Self {
        Self {
            schema_id: s.schema_id.clone(),
            content_type: s.content_type.clone(),
            version: s.version,
            codec: s.codec,
            compatibility: s.compatibility,
            description: s.description.clone(),
        }
    }
}

/// Request to register a new schema.
#[derive(Deserialize)]
pub struct RegisterSchemaRequest {
    /// Content type this schema validates (e.g., "custom_type").
    pub content_type: String,
    /// Schema version (must be greater than existing versions).
    pub version: u32,
    /// JSON Schema definition.
    pub json_schema: serde_json::Value,
    /// Preferred codec (defaults to JSON).
    #[serde(default)]
    pub codec: Option<Codec>,
    /// Compatibility mode (defaults to Backward).
    #[serde(default)]
    pub compatibility: Option<CompatibilityMode>,
    /// Human-readable description.
    #[serde(default)]
    pub description: Option<String>,
}

/// Request to validate content against a schema.
#[derive(Deserialize)]
pub struct ValidateRequest {
    /// Content to validate.
    pub content: serde_json::Value,
    /// Schema ID to validate against (optional, inferred from content type if not provided).
    #[serde(default)]
    pub schema_id: Option<String>,
}

/// Response for validation.
#[derive(Serialize)]
pub struct ValidateResponse {
    pub valid: bool,
    pub schema_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// GET /v1/schemas - List all registered schemas.
pub async fn list_schemas(State(state): State<AppState>) -> impl IntoResponse {
    let schemas: Vec<SchemaInfo> = state
        .engine
        .schema_registry()
        .list_all()
        .iter()
        .map(SchemaInfo::from)
        .collect();

    (StatusCode::OK, response::ok(SchemaListResponse { schemas }))
}

/// GET /v1/schemas/:schema_id - Get a specific schema by ID.
pub async fn get_schema(
    State(state): State<AppState>,
    Path(schema_id): Path<String>,
) -> impl IntoResponse {
    // URL-decode the path parameter (handles slashes like "insight/v1")
    let decoded_id = urlencoding::decode(&schema_id)
        .map(|s| s.into_owned())
        .unwrap_or(schema_id);

    match state.engine.schema_registry().get(&decoded_id) {
        Some(schema) => (StatusCode::OK, response::ok(schema)).into_response(),
        None => response::err::<SchemaInfo>(
            StatusCode::NOT_FOUND,
            "SCHEMA_NOT_FOUND",
            &format!("schema not found: {}", decoded_id),
        )
        .into_response(),
    }
}

/// POST /v1/schemas - Register a new schema.
pub async fn register_schema(
    State(state): State<AppState>,
    Json(req): Json<RegisterSchemaRequest>,
) -> impl IntoResponse {
    let mut schema = SchemaDefinition::new(req.content_type, req.version, req.json_schema);

    if let Some(codec) = req.codec {
        schema = schema.with_codec(codec);
    }
    if let Some(compat) = req.compatibility {
        schema = schema.with_compatibility(compat);
    }
    if let Some(desc) = req.description {
        schema = schema.with_description(desc);
    }

    match state.engine.schema_registry().register(schema.clone()) {
        Ok(()) => (
            StatusCode::CREATED,
            response::ok(SchemaInfo::from(&schema)),
        )
            .into_response(),
        Err(e) => response::from_error(e),
    }
}

/// POST /v1/schemas/validate - Validate content against a schema.
pub async fn validate_content(
    State(state): State<AppState>,
    Json(req): Json<ValidateRequest>,
) -> impl IntoResponse {
    let registry = state.engine.schema_registry();

    // Determine effective schema_id
    let effective_schema_id = req
        .schema_id
        .clone()
        .or_else(|| registry.infer_schema_id(&req.content));

    match registry.validate(&req.content, effective_schema_id.as_deref()) {
        Ok(()) => (
            StatusCode::OK,
            response::ok(ValidateResponse {
                valid: true,
                schema_id: effective_schema_id,
                error: None,
            }),
        )
            .into_response(),
        Err(e) => (
            StatusCode::OK,
            response::ok(ValidateResponse {
                valid: false,
                schema_id: effective_schema_id,
                error: Some(e.to_string()),
            }),
        )
            .into_response(),
    }
}
