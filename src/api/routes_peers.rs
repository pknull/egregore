use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::api::response;
use crate::api::AppState;
use crate::transport::health::TransportHealth;

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

    /// Phase 2 Wave 5 Step 24: transport health from
    /// `engine.transport_health()`. Present when `transport_count() >= 1`.
    /// Composite deployments carry `TransportHealth.children` with per-child
    /// `BridgeQueuesHealth` (RFC 0002 §8.4). Pre-Phase-2 deployments (no
    /// transports attached; test harnesses) omit the field entirely — the
    /// `#[serde(default, skip_serializing_if = "Option::is_none")]` pair
    /// preserves byte-for-byte compatibility with the pre-Phase-2 shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<TransportHealth>,
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

    // Phase 2 Wave 5 Step 24: surface transport health when at least one
    // transport is attached. `transport_health()` returns None for
    // test harnesses that don't attach a transport; the optional field
    // is omitted from the JSON in that case (see serde attribute).
    let transport = state.engine.transport_health();

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
        transport,
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
            transport: None,
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
            transport: None,
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

    // ------------------------------------------------------------------
    // Step 24 — /v1/status exposes `transport: Option<TransportHealth>`.
    // The field is `None` when no transports are attached (the test-harness
    // case); `Some(..)` when `engine.transport_count() >= 1`. This mirrors
    // the `engine.transport_health()` contract exercised in
    // `src/feed/engine.rs` Step 5 tests.
    // ------------------------------------------------------------------

    use crate::api::mcp_registry;
    use crate::blob::BlobStore;
    use crate::config::Config;
    use crate::feed::engine::FeedEngine;
    use crate::feed::store::FeedStore;
    use crate::identity::Identity;
    use crate::transport::health::TransportHealth;
    use crate::transport::{filter::TopicFilter, subscription::SubscriptionHandle, Transport};
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// Minimal inline mock transport — canned `health()` response; other
    /// trait methods unreachable here because `build_status` only consumes
    /// `engine.transport_health()`.
    struct StatusInlineMockTransport {
        health: TransportHealth,
    }

    #[async_trait]
    impl Transport for StatusInlineMockTransport {
        async fn publish(&self, _msg: &crate::feed::models::Message) -> crate::error::Result<()> {
            unimplemented!(
                "StatusInlineMockTransport::publish — not exercised by build_status tests"
            )
        }
        async fn subscribe(
            &self,
            _filter: TopicFilter,
        ) -> crate::error::Result<(
            SubscriptionHandle,
            BoxStream<'static, crate::feed::models::Message>,
        )> {
            unimplemented!(
                "StatusInlineMockTransport::subscribe — not exercised by build_status tests"
            )
        }
        async fn request_from(
            &self,
            _author: crate::identity::PublicId,
            _after_seq: u64,
        ) -> crate::error::Result<BoxStream<'static, crate::feed::models::Message>> {
            unimplemented!(
                "StatusInlineMockTransport::request_from — not exercised by build_status tests"
            )
        }
        async fn start(&self) -> crate::error::Result<()> {
            Ok(())
        }
        async fn shutdown(&self, _deadline: Duration) -> crate::error::Result<()> {
            Ok(())
        }
        fn health(&self) -> TransportHealth {
            self.health.clone()
        }
    }

    fn test_app_state_with_engine(engine: Arc<FeedEngine>) -> AppState {
        let tmp = tempfile::tempdir().unwrap();
        let blob_store = BlobStore::new(tmp.path());
        AppState {
            identity: Identity::generate(),
            engine,
            config: Arc::new(Config::default()),
            started_at: Instant::now(),
            mcp_registry: mcp_registry::create_registry(),
            blob_store,
        }
    }

    #[tokio::test]
    async fn build_status_includes_transport_when_attached() {
        let store = FeedStore::open_memory().unwrap();
        let engine = Arc::new(FeedEngine::new(store));

        // Attach one mock transport reporting connected=true. Single-transport
        // path: `transport_health()` returns that child's health verbatim.
        engine.attach_transport(Arc::new(StatusInlineMockTransport {
            health: TransportHealth {
                connected: true,
                backend: "mock-gossip",
                last_successful_publish: None,
                last_peer_contact: None,
                unreplicated_count: 0,
                inflight_publishes: 0,
                last_error: None,
                children: vec![],
                bridge_queues: None,
            },
        }));

        let state = test_app_state_with_engine(engine);
        let status = build_status(&state).await;

        let transport = status
            .transport
            .expect("transport attached => /v1/status.transport is Some");
        assert_eq!(transport.backend, "mock-gossip");
        assert!(transport.connected);
    }

    #[tokio::test]
    async fn build_status_omits_transport_when_no_transports() {
        let store = FeedStore::open_memory().unwrap();
        let engine = Arc::new(FeedEngine::new(store));
        // Do NOT attach any transport — test harness case.

        let state = test_app_state_with_engine(engine);
        let status = build_status(&state).await;

        assert!(
            status.transport.is_none(),
            "no transports attached => transport field is None (preserves Phase 1 shape)"
        );

        // Confirm the field is omitted from JSON output (byte-for-byte
        // compatibility with pre-Phase-2 consumers).
        let json = serde_json::to_value(&status).unwrap();
        assert!(
            json.get("transport").is_none(),
            "transport field must be omitted from JSON when None; got: {json}"
        );
    }
}
