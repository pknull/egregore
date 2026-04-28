use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

#[derive(Serialize)]
pub struct ApiResponse<T: Serialize> {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<ApiMetadata>,
}

#[derive(Serialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Vec<ApiValidationDetail>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ApiValidationDetail {
    pub field: String,
    pub message: String,
}

#[derive(Serialize)]
pub struct ApiMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
}

pub fn ok<T: Serialize>(data: T) -> Json<ApiResponse<T>> {
    Json(ApiResponse {
        success: true,
        data: Some(data),
        error: None,
        metadata: None,
    })
}

pub fn ok_with_metadata<T: Serialize>(data: T, metadata: ApiMetadata) -> Json<ApiResponse<T>> {
    Json(ApiResponse {
        success: true,
        data: Some(data),
        error: None,
        metadata: Some(metadata),
    })
}

pub fn err<T: Serialize>(
    status: StatusCode,
    code: &str,
    message: &str,
) -> (StatusCode, Json<ApiResponse<T>>) {
    err_with_details(status, code, message, None)
}

pub fn err_with_detail<T: Serialize>(
    status: StatusCode,
    code: &str,
    message: &str,
    detail: ApiValidationDetail,
) -> (StatusCode, Json<ApiResponse<T>>) {
    err_with_details(status, code, message, Some(vec![detail]))
}

pub fn err_with_details<T: Serialize>(
    status: StatusCode,
    code: &str,
    message: &str,
    details: Option<Vec<ApiValidationDetail>>,
) -> (StatusCode, Json<ApiResponse<T>>) {
    (
        status,
        Json(ApiResponse {
            success: false,
            data: None,
            error: Some(ApiError {
                code: code.to_string(),
                message: message.to_string(),
                details,
            }),
            metadata: None,
        }),
    )
}

pub fn validation_detail(
    field: impl Into<String>,
    message: impl Into<String>,
) -> ApiValidationDetail {
    ApiValidationDetail {
        field: field.into(),
        message: message.into(),
    }
}

pub fn json_rejection<T: Serialize>(
    rejection: JsonRejection,
) -> (StatusCode, Json<ApiResponse<T>>) {
    let status = rejection.status();
    let body_text = rejection.body_text();

    let (code, message) = match &rejection {
        JsonRejection::JsonSyntaxError(_) => {
            ("INVALID_JSON_BODY", "request body is not valid JSON")
        }
        JsonRejection::JsonDataError(_) => ("INVALID_JSON_BODY", "request body failed validation"),
        JsonRejection::MissingJsonContentType(_) => (
            "UNSUPPORTED_MEDIA_TYPE",
            "Content-Type must be application/json for mutating requests",
        ),
        _ => ("INVALID_JSON_BODY", "request body could not be processed"),
    };

    let detail = validation_detail(
        extract_json_field_name(&body_text).unwrap_or_else(|| "body".to_string()),
        normalize_json_rejection_message(&body_text),
    );

    err_with_detail(status, code, message, detail)
}

fn normalize_json_rejection_message(message: &str) -> String {
    const PREFIXES: [&str; 4] = [
        "Failed to deserialize the JSON body into the target type: ",
        "Failed to parse the request body as JSON: ",
        "Failed to buffer the request body: ",
        "Request body didn't contain valid UTF-8: ",
    ];

    for prefix in PREFIXES {
        if let Some(stripped) = message.strip_prefix(prefix) {
            return stripped.to_string();
        }
    }

    message.to_string()
}

fn extract_json_field_name(message: &str) -> Option<String> {
    for marker in ["missing field `", "unknown field `", "duplicate field `"] {
        if let Some(rest) = message.split(marker).nth(1) {
            if let Some(field) = rest.split('`').next() {
                return Some(field.to_string());
            }
        }
    }

    None
}

pub fn from_error(e: crate::error::EgreError) -> Response {
    let (status, code, message) = match &e {
        crate::error::EgreError::IdentityNotFound { .. } => {
            (StatusCode::NOT_FOUND, "IDENTITY_NOT_FOUND", e.to_string())
        }
        crate::error::EgreError::FeedIntegrity { .. } => {
            (StatusCode::BAD_REQUEST, "FEED_INTEGRITY", e.to_string())
        }
        crate::error::EgreError::DuplicateMessage { .. } => {
            (StatusCode::CONFLICT, "DUPLICATE_MESSAGE", e.to_string())
        }
        crate::error::EgreError::SignatureInvalid => {
            (StatusCode::BAD_REQUEST, "SIGNATURE_INVALID", e.to_string())
        }
        crate::error::EgreError::Serialization(_) => (
            StatusCode::BAD_REQUEST,
            "SERIALIZATION_ERROR",
            e.to_string(),
        ),
        _ => {
            tracing::warn!(error = %e, "internal error in API handler");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "an internal error occurred".to_string(),
            )
        }
    };

    let body = Json(ApiResponse::<()> {
        success: false,
        data: None,
        error: Some(ApiError {
            code: code.to_string(),
            message,
            details: None,
        }),
        metadata: None,
    });

    (status, body).into_response()
}

/// Run a synchronous, blocking workload (typically a SQLite call) on Tokio's
/// blocking pool and lift the standard error mapping out of every API
/// handler:
///
/// - `Ok(Ok(value))` → `Ok(value)` for the caller's success path.
/// - `Ok(Err(EgreError))` → `Err(from_error(e))`.
/// - `Err(JoinError)` → logged with `ctx`, returned as `INTERNAL_SERVER_ERROR`.
///
/// Caller pattern:
/// ```ignore
/// let value = match run_blocking(move || engine.query(...), "failed to query").await {
///     Ok(v) => v,
///     Err(resp) => return resp,
/// };
/// ```
///
/// Or, if the handler's signature is `Result<Response, Response>`, just `?`.
///
/// Do NOT use this helper for handlers that need to introspect `EgreError`
/// before converting it (e.g. group "already exists" → 409, status/health
/// builders that intentionally degrade rather than fail). Those keep the
/// inline `match`.
pub async fn run_blocking<T, F>(f: F, ctx: &'static str) -> std::result::Result<T, Response>
where
    F: FnOnce() -> std::result::Result<T, crate::error::EgreError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(t)) => Ok(t),
        Ok(Err(e)) => Err(from_error(e)),
        Err(join_err) => {
            tracing::error!(error = %join_err, ctx, "blocking task join failure");
            Err(err::<()>(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", ctx).into_response())
        }
    }
}
