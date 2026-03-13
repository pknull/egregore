//! HTTP API — localhost-only REST + SSE streaming + optional MCP JSON-RPC.
//!
//! All endpoints bind to 127.0.0.1 (binding happens in main.rs). Only local
//! processes can access the API. Mutating REST endpoints can optionally require
//! a Bearer token, while read-only routes remain loopback-only without auth.
//! MCP auth is handled separately.
//!
//! ## CSRF Protection
//!
//! Mutating endpoints (POST, PUT, DELETE, PATCH) require `Content-Type: application/json`.
//! This blocks simple CSRF attacks from HTML forms, which can only send
//! `application/x-www-form-urlencoded` or `multipart/form-data`. JavaScript fetch()
//! with custom headers triggers CORS preflight, which fails cross-origin to localhost.
//!
//! Routes include feed queries, publish, peer management, follows, consumer
//! groups, identity, status, and SSE at GET /v1/events. MCP is enabled
//! conditionally.

pub mod mcp;
pub mod mcp_registry;
pub mod mcp_tools;
pub mod response;
pub mod routes_events;
pub mod routes_feed;
pub mod routes_follows;
pub mod routes_groups;
pub mod routes_identity;
pub mod routes_mesh;
pub mod routes_peers;
pub mod routes_publish;
pub mod routes_retention;
pub mod routes_schema;
pub mod routes_topics;

use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::DefaultBodyLimit;
use axum::extract::State;
use axum::http::{header, Method, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::Router;

use crate::config::Config;
use crate::feed::engine::FeedEngine;
use crate::identity::Identity;

use mcp_registry::SharedRegistry;

#[derive(Clone)]
pub struct AppState {
    pub identity: Identity,
    pub engine: Arc<FeedEngine>,
    pub config: Arc<Config>,
    pub started_at: Instant,
    pub mcp_registry: SharedRegistry,
}

fn is_mutating_rest_request(req: &Request<Body>) -> bool {
    req.uri().path().starts_with("/v1/")
        && matches!(
            *req.method(),
            Method::POST | Method::PUT | Method::DELETE | Method::PATCH
        )
}

/// CSRF protection middleware.
///
/// Requires `Content-Type: application/json` for mutating HTTP methods (POST, PUT, DELETE, PATCH).
/// This prevents simple CSRF attacks from HTML forms which cannot set custom Content-Type headers.
///
/// Returns 415 Unsupported Media Type if the Content-Type is missing or not application/json.
async fn require_json_content_type(req: Request<Body>, next: Next) -> Response {
    // Only check mutating methods
    if matches!(
        *req.method(),
        Method::POST | Method::PUT | Method::DELETE | Method::PATCH
    ) {
        let content_type = req
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok());

        match content_type {
            Some(ct) if ct.starts_with("application/json") => {
                // Valid JSON content type, proceed
            }
            _ => {
                // Missing or invalid Content-Type
                return response::err::<()>(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    "UNSUPPORTED_MEDIA_TYPE",
                    "Content-Type must be application/json for mutating requests",
                )
                .into_response();
            }
        }
    }

    next.run(req).await
}

/// Optional Bearer token auth for mutating REST endpoints.
///
/// Only applies to mutating `/v1/...` routes. Read-only routes remain accessible
/// without auth, and `/mcp` is intentionally excluded until issue #89.
async fn require_api_auth(
    State(config): State<Arc<Config>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    if !config.api_auth_enabled || !is_mutating_rest_request(&req) {
        return next.run(req).await;
    }

    let expected_token = config
        .api_auth_token
        .as_deref()
        .filter(|token| !token.trim().is_empty());
    let provided_token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));

    if expected_token.is_some() && provided_token == expected_token {
        return next.run(req).await;
    }

    response::err::<()>(
        StatusCode::UNAUTHORIZED,
        "UNAUTHORIZED",
        "missing or invalid API token",
    )
    .into_response()
}

pub fn router(state: AppState) -> Router {
    router_with_mcp(state, true)
}

pub fn router_with_mcp(state: AppState, mcp_enabled: bool) -> Router {
    let auth_config = state.config.clone();
    let router = Router::new()
        .route("/v1/identity", get(routes_identity::get_identity))
        .route("/v1/publish", post(routes_publish::publish))
        .route("/v1/feed", get(routes_feed::get_feed))
        .route("/v1/feed/:author", get(routes_feed::get_feed_by_author))
        .route("/v1/insights", get(routes_feed::get_insights))
        .route("/v1/insights/search", get(routes_feed::search_insights))
        .route("/v1/message/:hash", get(routes_feed::get_message_by_hash))
        .route(
            "/v1/peers",
            get(routes_peers::get_peers).post(routes_peers::add_peer),
        )
        .route("/v1/peers/:address", delete(routes_peers::delete_peer))
        .route("/v1/status", get(routes_peers::get_status))
        .route("/v1/mesh", get(routes_mesh::get_mesh))
        .route(
            "/v1/follows/:author",
            post(routes_follows::add_follow).delete(routes_follows::remove_follow),
        )
        .route("/v1/follows", get(routes_follows::get_follows))
        .route(
            "/v1/topics/:topic",
            post(routes_topics::add_topic_subscription)
                .delete(routes_topics::remove_topic_subscription),
        )
        .route("/v1/topics", get(routes_topics::get_topic_subscriptions))
        .route("/v1/topics/known", get(routes_topics::get_known_topics))
        .route(
            "/v1/groups",
            get(routes_groups::list_groups).post(routes_groups::create_group),
        )
        .route(
            "/v1/groups/:id",
            get(routes_groups::get_group).delete(routes_groups::delete_group),
        )
        .route(
            "/v1/groups/:id/members",
            get(routes_groups::get_group_members),
        )
        .route("/v1/groups/:id/join", post(routes_groups::join_group))
        .route("/v1/groups/:id/leave", post(routes_groups::leave_group))
        .route(
            "/v1/groups/:id/offsets",
            get(routes_groups::get_group_offsets).post(routes_groups::commit_offset),
        )
        .route("/v1/events", get(routes_events::subscribe))
        .route(
            "/v1/schemas",
            get(routes_schema::list_schemas).post(routes_schema::register_schema),
        )
        .route(
            "/v1/schemas/validate",
            post(routes_schema::validate_content),
        )
        .route("/v1/schemas/*schema_id", get(routes_schema::get_schema))
        .route(
            "/v1/retention/policies",
            get(routes_retention::list_policies).post(routes_retention::create_policy),
        )
        .route(
            "/v1/retention/policies/:id",
            delete(routes_retention::delete_policy),
        );
    let router = if mcp_enabled {
        router.route(
            "/mcp",
            post(mcp::mcp_handler)
                .get(mcp::mcp_method_not_allowed)
                .delete(mcp::mcp_method_not_allowed),
        )
    } else {
        router
    };
    router
        .layer(middleware::from_fn_with_state(
            auth_config,
            require_api_auth,
        ))
        .layer(middleware::from_fn(require_json_content_type))
        .layer(DefaultBodyLimit::max(1024 * 1024)) // 1 MB
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::engine::FeedEngine;
    use crate::feed::store::FeedStore;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_state() -> AppState {
        test_state_with_config(Config::default())
    }

    fn test_state_with_config(config: Config) -> AppState {
        let store = FeedStore::open_memory().unwrap();
        let engine = FeedEngine::new(store);

        AppState {
            identity: Identity::generate(),
            engine: Arc::new(engine),
            config: Arc::new(config),
            started_at: Instant::now(),
            mcp_registry: mcp_registry::create_registry(),
        }
    }

    /// Test that POST requests without Content-Type are rejected (CSRF protection).
    #[tokio::test]
    async fn csrf_rejects_post_without_content_type() {
        let state = test_state();
        let app = router(state);

        // POST without Content-Type header (simulates HTML form CSRF)
        let req = Request::builder()
            .method("POST")
            .uri("/v1/publish")
            .body(Body::from(
                r#"{"content":{"type":"message","text":"pwned"}}"#,
            ))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "UNSUPPORTED_MEDIA_TYPE");
    }

    /// Test that POST requests with wrong Content-Type are rejected.
    #[tokio::test]
    async fn csrf_rejects_post_with_form_content_type() {
        let state = test_state();
        let app = router(state);

        // POST with form content type (what HTML forms send)
        let req = Request::builder()
            .method("POST")
            .uri("/v1/publish")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from("content=%7B%22type%22%3A%22message%22%7D"))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    /// Test that POST requests with correct Content-Type are allowed.
    #[tokio::test]
    async fn csrf_allows_post_with_json_content_type() {
        let state = test_state();
        let app = router(state);

        // POST with JSON content type - should be allowed through to handler
        // (handler may still reject for other reasons, but not 415)
        let req = Request::builder()
            .method("POST")
            .uri("/v1/publish")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"content":{"type":"message","text":"test"}}"#,
            ))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        // Should not be 415 - the middleware passed it through
        assert_ne!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    /// Test that GET requests don't require Content-Type.
    #[tokio::test]
    async fn csrf_allows_get_without_content_type() {
        let state = test_state();
        let app = router(state);

        let req = Request::builder()
            .method("GET")
            .uri("/v1/status")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// Test that DELETE requests require Content-Type.
    #[tokio::test]
    async fn csrf_rejects_delete_without_content_type() {
        let state = test_state();
        let app = router(state);

        let req = Request::builder()
            .method("DELETE")
            .uri("/v1/peers/127.0.0.1:7655")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    async fn auth_rejects_mutating_rest_request_without_token() {
        let state = test_state_with_config(Config {
            api_auth_enabled: true,
            api_auth_token: Some("secret-token".to_string()),
            ..Config::default()
        });
        let app = router(state);

        let req = Request::builder()
            .method("POST")
            .uri("/v1/publish")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"content":{"type":"message","text":"auth required"}}"#,
            ))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["success"], false);
        assert_eq!(body["error"]["code"], "UNAUTHORIZED");
        assert_eq!(body["error"]["message"], "missing or invalid API token");
    }

    #[tokio::test]
    async fn auth_rejects_mutating_rest_request_with_invalid_token() {
        let state = test_state_with_config(Config {
            api_auth_enabled: true,
            api_auth_token: Some("secret-token".to_string()),
            ..Config::default()
        });
        let app = router(state);

        let req = Request::builder()
            .method("POST")
            .uri("/v1/publish")
            .header("content-type", "application/json")
            .header("authorization", "Bearer wrong-token")
            .body(Body::from(
                r#"{"content":{"type":"message","text":"auth required"}}"#,
            ))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn auth_allows_mutating_rest_request_with_valid_token() {
        let state = test_state_with_config(Config {
            api_auth_enabled: true,
            api_auth_token: Some("secret-token".to_string()),
            ..Config::default()
        });
        let app = router(state);

        let req = Request::builder()
            .method("POST")
            .uri("/v1/publish")
            .header("content-type", "application/json")
            .header("authorization", "Bearer secret-token")
            .body(Body::from(
                r#"{"content":{"type":"message","text":"authorized"}}"#,
            ))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn auth_keeps_read_only_routes_public() {
        let state = test_state_with_config(Config {
            api_auth_enabled: true,
            api_auth_token: Some("secret-token".to_string()),
            ..Config::default()
        });
        let app = router(state);

        let req = Request::builder()
            .method("GET")
            .uri("/v1/status")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn auth_does_not_change_csrf_content_type_behavior() {
        let state = test_state_with_config(Config {
            api_auth_enabled: true,
            api_auth_token: Some("secret-token".to_string()),
            ..Config::default()
        });
        let app = router(state);

        let req = Request::builder()
            .method("POST")
            .uri("/v1/publish")
            .body(Body::from(
                r#"{"content":{"type":"message","text":"wrong content type"}}"#,
            ))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    async fn auth_does_not_gate_mcp_requests_in_this_branch() {
        let state = test_state_with_config(Config {
            api_auth_enabled: true,
            api_auth_token: Some("secret-token".to_string()),
            ..Config::default()
        });
        let app = router_with_mcp(state, true);

        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"unknown"}"#))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
