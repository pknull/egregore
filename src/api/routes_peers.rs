use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::api::response;
use crate::api::AppState;

#[derive(Serialize)]
pub struct PeerInfo {
    pub address: String,
    pub public_id: Option<String>,
    pub source: String,
}

#[derive(Deserialize)]
pub struct AddPeerRequest {
    pub address: String,
}

/// Validate a peer address is in host:port format.
pub fn is_valid_peer_address(address: &str) -> bool {
    let parts: Vec<&str> = address.rsplitn(2, ':').collect();
    parts.len() == 2 && parts[0].parse::<u16>().is_ok() && !parts[1].is_empty()
}

/// Health status for individual subsystems.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SubsystemHealth {
    /// Subsystem is operating normally.
    Healthy,
    /// Subsystem encountered an error but node continues operating.
    Degraded { error: String },
}

impl SubsystemHealth {
    pub fn is_healthy(&self) -> bool {
        matches!(self, SubsystemHealth::Healthy)
    }
}

/// Health indicators for observable subsystems.
#[derive(Debug, Serialize)]
pub struct HealthIndicators {
    /// Overall health: true if all subsystems are healthy.
    pub healthy: bool,
    /// Storage subsystem (SQLite database access).
    pub store: SubsystemHealth,
}

#[derive(Debug, Serialize)]
pub struct StatusInfo {
    pub version: String,
    pub identity: String,
    pub port: u16,
    pub gossip_port: u16,
    pub peer_count: usize,
    pub message_count: u64,
    pub feed_count: u64,
    pub follow_count: u64,
    pub uptime_secs: u64,
    /// Health indicators for subsystems. Null or absent means healthy (backward compat).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<HealthIndicators>,
}

/// Aggregate peers from all sources (CLI, address table, known_peers) with deduplication.
pub async fn aggregate_peers(state: &AppState) -> Vec<PeerInfo> {
    let engine = state.engine.clone();
    let cli_peers = state.config.peers.clone();

    let result = tokio::task::spawn_blocking(move || {
        let store = engine.store();
        let address_peers = store.list_address_peers().unwrap_or_default();
        let known_peers = store.list_peers(true).unwrap_or_default();
        (address_peers, known_peers)
    })
    .await;

    let (address_peers, known_peers) = result.unwrap_or_default();

    let mut peers: Vec<PeerInfo> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for addr in &cli_peers {
        seen.insert(addr.clone());
        peers.push(PeerInfo {
            address: addr.clone(),
            public_id: None,
            source: "cli".to_string(),
        });
    }

    for ap in &address_peers {
        if seen.insert(ap.address.clone()) {
            peers.push(PeerInfo {
                address: ap.address.clone(),
                public_id: ap.public_id.clone(),
                source: "manual".to_string(),
            });
        }
    }

    for kp in &known_peers {
        if let Some(ref addr) = kp.address {
            if seen.insert(addr.clone()) {
                peers.push(PeerInfo {
                    address: addr.clone(),
                    public_id: Some(kp.public_id.0.clone()),
                    source: "known".to_string(),
                });
            }
        }
    }

    peers
}

pub async fn get_peers(State(state): State<AppState>) -> impl IntoResponse {
    let peers = aggregate_peers(&state).await;
    response::ok(peers).into_response()
}

pub async fn add_peer(
    State(state): State<AppState>,
    payload: Result<Json<AddPeerRequest>, JsonRejection>,
) -> impl IntoResponse {
    let Json(req) = match payload {
        Ok(req) => req,
        Err(rejection) => return response::json_rejection::<PeerInfo>(rejection).into_response(),
    };

    if !is_valid_peer_address(&req.address) {
        return response::err_with_detail::<PeerInfo>(
            StatusCode::BAD_REQUEST,
            "INVALID_ADDRESS",
            "address must be in host:port format",
            response::validation_detail("address", "must be in host:port format"),
        )
        .into_response();
    }

    let address = req.address.clone();
    let engine = state.engine.clone();

    let result =
        tokio::task::spawn_blocking(move || engine.store().insert_address_peer(&address)).await;

    match result {
        Ok(Ok(())) => (
            StatusCode::CREATED,
            response::ok(PeerInfo {
                address: req.address,
                public_id: None,
                source: "manual".to_string(),
            }),
        )
            .into_response(),
        Ok(Err(e)) => response::from_error(e),
        Err(_) => response::err::<PeerInfo>(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "failed to persist peer",
        )
        .into_response(),
    }
}

pub async fn delete_peer(
    State(state): State<AppState>,
    Path(address): Path<String>,
) -> impl IntoResponse {
    if !is_valid_peer_address(&address) {
        return response::err_with_detail::<()>(
            StatusCode::BAD_REQUEST,
            "INVALID_ADDRESS",
            "address must be in host:port format",
            response::validation_detail("address", "must be in host:port format"),
        )
        .into_response();
    }

    let engine = state.engine.clone();

    let result =
        tokio::task::spawn_blocking(move || engine.store().remove_address_peer(&address)).await;

    match result {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(e)) => response::from_error(e),
        Err(_) => response::err::<()>(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "failed to remove peer",
        )
        .into_response(),
    }
}

/// Build status info from current state. Shared by HTTP route and MCP tool.
pub async fn build_status(state: &AppState) -> StatusInfo {
    let engine = state.engine.clone();
    let uptime = state.started_at.elapsed().as_secs();
    let peer_count = aggregate_peers(state).await.len();

    let result = tokio::task::spawn_blocking(move || {
        let store = engine.store();

        // Track individual failures for health reporting
        let message_result = store.message_count();
        let feed_result = store.feed_count();
        let follow_result = store.follow_count();

        // Collect any errors
        let mut errors: Vec<String> = Vec::new();
        if let Err(ref e) = message_result {
            errors.push(format!("message_count: {}", e));
        }
        if let Err(ref e) = feed_result {
            errors.push(format!("feed_count: {}", e));
        }
        if let Err(ref e) = follow_result {
            errors.push(format!("follow_count: {}", e));
        }

        let store_health = if errors.is_empty() {
            SubsystemHealth::Healthy
        } else {
            tracing::warn!(errors = ?errors, "store subsystem degraded");
            SubsystemHealth::Degraded {
                error: errors.join("; "),
            }
        };

        (
            message_result.unwrap_or(0),
            feed_result.unwrap_or(0),
            follow_result.unwrap_or(0),
            store_health,
        )
    })
    .await;

    let (message_count, feed_count, follow_count, store_health) = match result {
        Ok((m, f, fo, h)) => (m, f, fo, h),
        Err(e) => {
            tracing::error!(error = %e, "status task failed, store subsystem degraded");
            (
                0,
                0,
                0,
                SubsystemHealth::Degraded {
                    error: format!("task panic: {}", e),
                },
            )
        }
    };

    // Only include health indicators if degraded (backward compat)
    let health = if store_health.is_healthy() {
        None
    } else {
        Some(HealthIndicators {
            healthy: false,
            store: store_health,
        })
    };

    StatusInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        identity: state.identity.public_id().0,
        port: state.config.port,
        gossip_port: state.config.gossip_port,
        peer_count,
        message_count,
        feed_count,
        follow_count,
        uptime_secs: uptime,
        health,
    }
}

pub async fn get_status(State(state): State<AppState>) -> impl IntoResponse {
    response::ok(build_status(&state).await).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subsystem_health_is_healthy() {
        assert!(SubsystemHealth::Healthy.is_healthy());
        assert!(!SubsystemHealth::Degraded {
            error: "test".to_string()
        }
        .is_healthy());
    }

    #[test]
    fn healthy_status_omits_health_field() {
        let status = StatusInfo {
            version: "1.0.0".to_string(),
            identity: "@test.ed25519".to_string(),
            port: 7654,
            gossip_port: 7655,
            peer_count: 0,
            message_count: 0,
            feed_count: 0,
            follow_count: 0,
            uptime_secs: 0,
            health: None,
        };
        let json = serde_json::to_value(&status).unwrap();
        assert!(
            json.get("health").is_none(),
            "healthy status should omit health field"
        );
    }

    #[test]
    fn degraded_status_includes_health_field() {
        let status = StatusInfo {
            version: "1.0.0".to_string(),
            identity: "@test.ed25519".to_string(),
            port: 7654,
            gossip_port: 7655,
            peer_count: 0,
            message_count: 0,
            feed_count: 0,
            follow_count: 0,
            uptime_secs: 0,
            health: Some(HealthIndicators {
                healthy: false,
                store: SubsystemHealth::Degraded {
                    error: "db locked".to_string(),
                },
            }),
        };
        let json = serde_json::to_value(&status).unwrap();
        let health = json
            .get("health")
            .expect("degraded status should include health field");
        assert_eq!(health["healthy"], false);
        assert_eq!(health["store"]["degraded"]["error"], "db locked");
    }

    #[test]
    fn subsystem_health_serialization() {
        let healthy = serde_json::to_value(SubsystemHealth::Healthy).unwrap();
        assert_eq!(healthy, "healthy");

        let degraded = serde_json::to_value(SubsystemHealth::Degraded {
            error: "test error".to_string(),
        })
        .unwrap();
        assert_eq!(degraded["degraded"]["error"], "test error");
    }
}
