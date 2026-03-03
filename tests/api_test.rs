//! API integration tests using production router.
//!
//! These tests use the real router from `egregore::api` rather than
//! re-implementing handlers. This ensures tests catch regressions in
//! actual production routes, middleware, and response handling.

use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use egregore::api::{mcp_registry, router_with_mcp, AppState};
use egregore::config::Config;
use egregore::feed::engine::FeedEngine;
use egregore::feed::store::FeedStore;
use egregore::identity::Identity;
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

/// Create test app state and production router.
///
/// Uses in-memory SQLite and generates a fresh identity for each test.
fn create_test_app() -> (Arc<FeedEngine>, axum::Router) {
    let identity = Identity::generate();
    let store = FeedStore::open_memory().unwrap();
    let engine = Arc::new(FeedEngine::new(store));

    let state = AppState {
        identity,
        engine: engine.clone(),
        config: Arc::new(Config::default()),
        started_at: Instant::now(),
        mcp_registry: mcp_registry::create_registry(),
    };

    // Use production router without MCP to avoid endpoint conflicts in tests
    let app = router_with_mcp(state, false);

    (engine, app)
}

/// Helper to parse JSON response body.
async fn json_body(response: axum::response::Response) -> Value {
    let body = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

// ============ GROUP TESTS ============

#[tokio::test]
async fn test_create_and_list_groups() {
    let (_engine, app) = create_test_app();

    // Create a group
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/groups")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"group_id": "test-group"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let json = json_body(response).await;
    assert!(json["success"].as_bool().unwrap());
    assert_eq!(json["data"]["group_id"], "test-group");

    // List groups
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/groups")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    assert!(json["success"].as_bool().unwrap());
    assert_eq!(json["data"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn test_get_nonexistent_group_returns_404() {
    let (_engine, app) = create_test_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/groups/nonexistent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_get_members_nonexistent_group_returns_404() {
    let (_engine, app) = create_test_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/groups/nonexistent/members")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_get_offsets_nonexistent_group_returns_404() {
    let (_engine, app) = create_test_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/groups/nonexistent/offsets")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_join_and_leave_group() {
    let (_engine, app) = create_test_app();

    // Create group first
    let _ = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/groups")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"group_id": "join-test"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    // Join the group
    let member_id = "@AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=.ed25519";
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/groups/join-test/join")
                .header("content-type", "application/json")
                .body(Body::from(format!(r#"{{"member_id": "{}"}}"#, member_id)))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    assert!(json["data"]["is_leader"].as_bool().unwrap());

    // Leave the group
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/groups/join-test/leave")
                .header("content-type", "application/json")
                .body(Body::from(format!(r#"{{"member_id": "{}"}}"#, member_id)))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn test_delete_group() {
    let (_engine, app) = create_test_app();

    // Create group
    let _ = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/groups")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"group_id": "delete-test"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    // Delete it (note: production route requires content-type for DELETE)
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/v1/groups/delete-test")
                .header("content-type", "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    // Verify it's gone
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/groups/delete-test")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_duplicate_group_creation_returns_conflict() {
    let (_engine, app) = create_test_app();

    // Create group
    let _ = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/groups")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"group_id": "dup-test"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    // Try to create again
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/groups")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"group_id": "dup-test"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CONFLICT);
}

// ============ TOPIC TESTS ============

#[tokio::test]
async fn test_topic_subscription_lifecycle() {
    let (_engine, app) = create_test_app();

    // Subscribe to a topic (note: production route requires content-type for POST)
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/topics/rust")
                .header("content-type", "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);

    // List topics
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/topics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    // Production returns [{topic: "rust"}] not ["rust"]
    let topics: Vec<&str> = json["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v["topic"].as_str())
        .collect();
    assert!(topics.contains(&"rust"));

    // Unsubscribe (note: production route requires content-type for DELETE)
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/v1/topics/rust")
                .header("content-type", "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    // Verify it's gone
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/topics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let json = json_body(response).await;
    let topics: Vec<&str> = json["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v["topic"].as_str())
        .collect();
    assert!(!topics.contains(&"rust"));
}

// ============ IDENTITY TESTS ============

#[tokio::test]
async fn test_get_identity() {
    let (_engine, app) = create_test_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/identity")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    assert!(json["success"].as_bool().unwrap());
    assert!(json["data"]["public_id"].as_str().is_some());
}

// ============ STATUS TESTS ============

#[tokio::test]
async fn test_get_status() {
    let (_engine, app) = create_test_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    assert!(json["success"].as_bool().unwrap());
    assert!(json["data"]["uptime_secs"].as_f64().is_some());
}

// ============ FEED TESTS ============

#[tokio::test]
async fn test_get_empty_feed() {
    let (_engine, app) = create_test_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/feed")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    assert!(json["success"].as_bool().unwrap());
    assert!(json["data"].as_array().unwrap().is_empty());
}

// ============ PEERS TESTS ============

#[tokio::test]
async fn test_get_peers() {
    let (_engine, app) = create_test_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/peers")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    assert!(json["success"].as_bool().unwrap());
}

// ============ FOLLOWS TESTS ============

#[tokio::test]
async fn test_get_follows() {
    let (_engine, app) = create_test_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/follows")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    assert!(json["success"].as_bool().unwrap());
    assert!(json["data"].as_array().unwrap().is_empty());
}

// ============ SCHEMA TESTS ============

#[tokio::test]
async fn test_list_schemas() {
    let (_engine, app) = create_test_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/schemas")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    assert!(json["success"].as_bool().unwrap());
}

// ============ RETENTION TESTS ============

#[tokio::test]
async fn test_list_retention_policies() {
    let (_engine, app) = create_test_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/retention/policies")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    assert!(json["success"].as_bool().unwrap());
}

// ============ CSRF PROTECTION TESTS ============
// These verify the production middleware is active.

#[tokio::test]
async fn test_csrf_rejects_post_without_content_type() {
    let (_engine, app) = create_test_app();

    // POST without Content-Type should be rejected
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/groups")
                .body(Body::from(r#"{"group_id": "csrf-test"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn test_csrf_rejects_form_content_type() {
    let (_engine, app) = create_test_app();

    // POST with form content type should be rejected
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/groups")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("group_id=csrf-test"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}
