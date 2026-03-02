//! HTTP API — localhost-only REST + SSE streaming + optional MCP JSON-RPC.
//!
//! All endpoints bind to 127.0.0.1 (binding happens in main.rs). Only local
//! processes can access the API — there is no authentication on the HTTP layer.
//! The security boundary is the loopback interface itself.
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

/// CSRF protection middleware.
///
/// Requires `Content-Type: application/json` for mutating HTTP methods (POST, PUT, DELETE, PATCH).
/// This prevents simple CSRF attacks from HTML forms which cannot set custom Content-Type headers.
///
/// Returns 415 Unsupported Media Type if the Content-Type is missing or not application/json.
async fn require_json_content_type(req: Request<Body>, next: Next) -> Response {
    // Only check mutating methods
    let is_mutating = matches!(
        *req.method(),
        Method::POST | Method::PUT | Method::DELETE | Method::PATCH
    );

    if is_mutating {
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

pub fn router(state: AppState) -> Router {
    router_with_mcp(state, true)
}

pub fn router_with_mcp(state: AppState, mcp_enabled: bool) -> Router {
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
        .layer(middleware::from_fn(require_json_content_type))
        .layer(DefaultBodyLimit::max(1024 * 1024)) // 1 MB
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use crate::feed::engine::FeedEngine;
    use crate::feed::store::FeedStore;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_state() -> AppState {
        let store = FeedStore::open_memory().unwrap();
        let engine = FeedEngine::new(store);

        AppState {
            identity: Identity::generate(),
            engine: Arc::new(engine),
            config: Arc::new(Config::default()),
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
            .body(Body::from(r#"{"content":{"type":"message","text":"pwned"}}"#))
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
            .body(Body::from(r#"{"content":{"type":"message","text":"test"}}"#))
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
}
