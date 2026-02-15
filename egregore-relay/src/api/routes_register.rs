use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use egregore::feed::store::PeerRecord;
use egregore::identity::PublicId;

use crate::api::RelayState;

fn ok_json<T: Serialize>(data: T) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "success": true,
        "data": data,
    }))
}

fn err_json(code: &str, message: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "success": false,
            "error": { "code": code, "message": message },
        })),
    )
}

pub(crate) fn internal_err() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "success": false,
            "error": { "code": "INTERNAL_ERROR", "message": "an internal error occurred" },
        })),
    )
        .into_response()
}

/// Collapse a spawn_blocking result into a Response, mapping errors to internal_err().
pub(crate) fn from_blocking<T>(
    result: std::result::Result<egregore::error::Result<T>, tokio::task::JoinError>,
    on_ok: impl FnOnce(T) -> Response,
) -> Response {
    match result {
        Ok(Ok(val)) => on_ok(val),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "store error");
            internal_err()
        }
        Err(_) => internal_err(),
    }
}

const MAX_NICKNAME_LENGTH: usize = 64;

/// Max age of a settings request timestamp before it's considered stale.
const SETTINGS_MAX_AGE_SECS: i64 = 300; // 5 minutes
/// Max forward clock skew tolerated (client clock ahead of server).
const SETTINGS_MAX_SKEW_SECS: i64 = 30;

/// Verify a signed request: decode signature, check timestamp freshness, verify Ed25519.
fn verify_signed_request(
    pub_id: &PublicId,
    timestamp: &str,
    signature_b64: &str,
) -> std::result::Result<(), Response> {
    let vk = pub_id
        .to_verifying_key()
        .map_err(|_| err_json("INVALID_PUBLIC_ID", "public_id is not a valid Ed25519 key").into_response())?;

    let sig_bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        signature_b64,
    )
    .map_err(|_| err_json("INVALID_SIGNATURE", "signature is not valid base64").into_response())?;

    let sig_array: [u8; 64] = sig_bytes
        .try_into()
        .map_err(|_| err_json("INVALID_SIGNATURE", "signature must be 64 bytes").into_response())?;

    let ts = chrono::DateTime::parse_from_rfc3339(timestamp)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|_| {
            err_json("INVALID_TIMESTAMP", "timestamp must be RFC 3339 format").into_response()
        })?;

    let age_secs = (chrono::Utc::now() - ts).num_seconds();
    if age_secs < -SETTINGS_MAX_SKEW_SECS {
        return Err(
            err_json("INVALID_TIMESTAMP", "request timestamp is too far in the future")
                .into_response(),
        );
    }
    if age_secs > SETTINGS_MAX_AGE_SECS {
        return Err(err_json("STALE_REQUEST", "request timestamp is too old").into_response());
    }

    let signed_payload = format!("{}:{}", pub_id.0, timestamp);
    let sig = ed25519_dalek::Signature::from_bytes(&sig_array);
    egregore::identity::verify_signature(&vk, signed_payload.as_bytes(), &sig)
        .map_err(|_| err_json("SIGNATURE_INVALID", "signature verification failed").into_response())
}

/// Check whether the relay has capacity for another peer.
async fn check_peer_capacity(state: &RelayState) -> std::result::Result<(), Response> {
    if state.relay_config.max_peers == 0 {
        return Ok(());
    }
    let engine = state.engine.clone();
    let count = match tokio::task::spawn_blocking(move || engine.store().peer_count()).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "peer_count query failed, rejecting registration");
            return Err(internal_err());
        }
        Err(e) => {
            tracing::warn!(error = %e, "peer_count task failed, rejecting registration");
            return Err(internal_err());
        }
    };
    if count as usize >= state.relay_config.max_peers {
        return Err(
            err_json("MAX_PEERS_REACHED", "relay has reached maximum peer capacity")
                .into_response(),
        );
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct RegisterRequest {
    pub public_id: String,
    #[serde(default)]
    pub nickname: Option<String>,
    #[serde(default)]
    pub private: bool,
}

#[derive(Serialize)]
pub struct RegisterResponse {
    pub public_id: String,
    pub registered: bool,
}

pub async fn register(
    State(state): State<RelayState>,
    Json(req): Json<RegisterRequest>,
) -> impl IntoResponse {
    let pub_id = PublicId(req.public_id.clone());

    if pub_id.to_verifying_key().is_err() {
        return err_json("INVALID_PUBLIC_ID", "public_id is not a valid Ed25519 key")
            .into_response();
    }

    if let Some(ref nick) = req.nickname {
        if nick.len() > MAX_NICKNAME_LENGTH {
            return err_json("NICKNAME_TOO_LONG", "nickname must be at most 64 characters")
                .into_response();
        }
    }

    // Reject re-registration: updating settings requires a signed request.
    {
        let engine = state.engine.clone();
        let pid = pub_id.clone();
        let exists = tokio::task::spawn_blocking(move || engine.store().get_peer(&pid)).await;
        match exists {
            Ok(Ok(Some(_))) => {
                return err_json("ALREADY_REGISTERED", "this public_id is already registered")
                    .into_response();
            }
            Ok(Ok(None)) => {} // not registered, proceed
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "peer lookup failed, rejecting registration");
                return internal_err();
            }
            Err(e) => {
                tracing::warn!(error = %e, "peer lookup task failed, rejecting registration");
                return internal_err();
            }
        }
    }

    if let Err(resp) = check_peer_capacity(&state).await {
        return resp;
    }

    let engine = state.engine.clone();
    let record = PeerRecord {
        public_id: pub_id,
        address: None,
        nickname: req.nickname,
        private: req.private,
        authorized: true,
        first_seen: chrono::Utc::now(),
        last_connected: None,
        last_synced: None,
    };

    let result = tokio::task::spawn_blocking(move || engine.store().insert_peer(&record)).await;

    from_blocking(result, |()| {
        (
            StatusCode::CREATED,
            ok_json(RegisterResponse {
                public_id: req.public_id,
                registered: true,
            }),
        )
            .into_response()
    })
}

#[derive(Deserialize)]
pub struct SettingsRequest {
    pub public_id: String,
    pub private: Option<bool>,
    /// ISO 8601 timestamp included in the signed payload for replay protection
    pub timestamp: String,
    /// Base64-encoded Ed25519 signature of "{public_id}:{timestamp}"
    pub signature: String,
}

pub async fn update_settings(
    State(state): State<RelayState>,
    Json(req): Json<SettingsRequest>,
) -> impl IntoResponse {
    let pub_id = PublicId(req.public_id.clone());

    if let Err(resp) = verify_signed_request(&pub_id, &req.timestamp, &req.signature) {
        return resp;
    }

    let engine = state.engine.clone();
    let pid = pub_id.clone();
    let private = req.private;

    let result = tokio::task::spawn_blocking(move || {
        if let Some(p) = private {
            engine.store().update_peer_private(&pid, p)?;
        }
        Ok::<_, egregore::error::EgreError>(())
    })
    .await;

    from_blocking(result, |()| {
        ok_json(serde_json::json!({ "updated": true })).into_response()
    })
}
