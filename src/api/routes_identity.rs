use axum::extract::State;
use axum::Json;
use base64::Engine;
use serde::Serialize;

use crate::api::response::{ok, ApiResponse};
use crate::api::AppState;

#[derive(Serialize)]
pub struct IdentityInfo {
    pub public_id: String,
    pub x25519_public: String,
}

pub async fn get_identity(
    State(state): State<AppState>,
) -> Json<ApiResponse<IdentityInfo>> {
    let identity = state.identity.clone();
    let pub_id = identity.public_id();
    let x25519 = identity.to_x25519_public_key();

    ok(IdentityInfo {
        public_id: pub_id.0,
        x25519_public: base64::engine::general_purpose::STANDARD.encode(x25519.as_bytes()),
    })
}
