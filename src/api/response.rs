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

pub fn err<T: Serialize>(status: StatusCode, code: &str, message: &str) -> (StatusCode, Json<ApiResponse<T>>) {
    (
        status,
        Json(ApiResponse {
            success: false,
            data: None,
            error: Some(ApiError {
                code: code.to_string(),
                message: message.to_string(),
            }),
            metadata: None,
        }),
    )
}

impl IntoResponse for crate::error::EgreError {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            crate::error::EgreError::IdentityNotFound { .. } => {
                (StatusCode::NOT_FOUND, "IDENTITY_NOT_FOUND")
            }
            crate::error::EgreError::FeedIntegrity { .. } => {
                (StatusCode::BAD_REQUEST, "FEED_INTEGRITY")
            }
            crate::error::EgreError::DuplicateMessage { .. } => {
                (StatusCode::CONFLICT, "DUPLICATE_MESSAGE")
            }
            crate::error::EgreError::SignatureInvalid => {
                (StatusCode::BAD_REQUEST, "SIGNATURE_INVALID")
            }
            crate::error::EgreError::Serialization(_) => {
                (StatusCode::BAD_REQUEST, "SERIALIZATION_ERROR")
            }
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR"),
        };

        let body = Json(ApiResponse::<()> {
            success: false,
            data: None,
            error: Some(ApiError {
                code: code.to_string(),
                message: self.to_string(),
            }),
            metadata: None,
        });

        (status, body).into_response()
    }
}
