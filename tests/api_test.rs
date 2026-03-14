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
    let (identity, engine) = create_test_engine();
    let app = create_test_app_with_identity(identity, engine.clone());
    (engine, app)
}

fn create_test_engine() -> (Identity, Arc<FeedEngine>) {
    let identity = Identity::generate();
    let store = FeedStore::open_memory().unwrap();
    let engine = Arc::new(FeedEngine::new(store));
    (identity, engine)
}

fn create_test_app_with_identity(identity: Identity, engine: Arc<FeedEngine>) -> axum::Router {
    let state = AppState {
        identity,
        engine,
        config: Arc::new(Config::default()),
        started_at: Instant::now(),
        mcp_registry: mcp_registry::create_registry(),
    };

    router_with_mcp(state, false)
}

/// Helper to parse JSON response body.
async fn json_body(response: axum::response::Response) -> Value {
    let body = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

fn assert_validation_error(
    json: &Value,
    expected_code: &str,
    expected_field: &str,
    expected_message_fragment: &str,
) {
    assert_eq!(json["success"], false);
    assert_eq!(json["error"]["code"], expected_code);
    assert_eq!(json["error"]["details"][0]["field"], expected_field);
    assert!(
        json["error"]["details"][0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains(expected_message_fragment),
        "expected validation message containing {:?}, got {:?}",
        expected_message_fragment,
        json["error"]["details"][0]["message"]
    );
}

fn assert_validation_error_code_and_field(json: &Value, expected_code: &str, expected_field: &str) {
    assert_eq!(json["success"], false);
    assert_eq!(json["error"]["code"], expected_code);
    assert_eq!(json["error"]["details"][0]["field"], expected_field);
    assert!(
        json["error"]["details"][0]["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty()),
        "expected non-empty validation detail message, got {:?}",
        json["error"]["details"][0]["message"]
    );
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
async fn test_join_group_invalid_member_id_returns_structured_error() {
    let (_engine, app) = create_test_app();

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

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/groups/join-test/join")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"member_id":"not-a-public-id"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = json_body(response).await;
    assert_validation_error(&json, "INVALID_MEMBER_ID", "member_id", "@<base64>.ed25519");
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

#[tokio::test]
async fn test_topic_subscription_invalid_topic_returns_structured_error() {
    let (_engine, app) = create_test_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/topics/%20")
                .header("content-type", "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = json_body(response).await;
    assert_validation_error(&json, "INVALID_TOPIC", "topic", "1-256 characters");
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

#[tokio::test]
async fn test_register_schema_invalid_payload_returns_structured_error() {
    let (_engine, app) = create_test_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/schemas")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"content_type":"","version":1,"json_schema":"not-an-object"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = json_body(response).await;
    assert_validation_error(&json, "INVALID_CONTENT_TYPE", "content_type", "non-empty");
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

#[tokio::test]
async fn test_create_retention_policy_invalid_payload_returns_structured_error() {
    let (_engine, app) = create_test_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/retention/policies")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"scope":"global","max_age_secs":0}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = json_body(response).await;
    assert_validation_error(
        &json,
        "INVALID_MAX_AGE_SECS",
        "max_age_secs",
        "greater than 0",
    );
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

// ============ PUBLISH TESTS ============

#[tokio::test]
async fn test_publish_message() {
    let (_engine, app) = create_test_app();

    // Publish a simple message
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/publish")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{
                        "content": {
                            "type": "message",
                            "text": "Hello, mesh!"
                        }
                    }"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let json = json_body(response).await;
    assert!(json["success"].as_bool().unwrap());
    assert!(json["data"]["hash"].as_str().is_some());
    assert!(json["data"]["author"].as_str().is_some());
    assert_eq!(json["data"]["sequence"], 1);
}

#[tokio::test]
async fn test_publish_with_tags_and_relates() {
    let (engine, app) = create_test_app();

    // First publish a message to get a hash for relating
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/publish")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"content": {"type": "message", "text": "First message"}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let first_json = json_body(response).await;
    let first_hash = first_json["data"]["hash"].as_str().unwrap();

    // Publish a reply with tags and relates
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/publish")
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{
                        "content": {{"type": "message", "text": "Reply message"}},
                        "relates": "{}",
                        "tags": ["reply", "test"]
                    }}"#,
                    first_hash
                )))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let json = json_body(response).await;
    assert!(json["success"].as_bool().unwrap());
    assert_eq!(json["data"]["sequence"], 2);

    // Verify the messages are in the feed
    let query = egregore::feed::models::FeedQuery::default();
    let messages = engine.store().query_messages(&query).unwrap();
    assert_eq!(messages.len(), 2);
}

#[tokio::test]
async fn test_get_feed_filters_by_trace_id() {
    let (_engine, app) = create_test_app();

    for trace_id in ["trace-keep", "trace-skip"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/publish")
                    .header("content-type", "application/json")
                    .body(Body::from(format!(
                        r#"{{
                            "content": {{
                                "type": "message",
                                "text": "message for {trace_id}"
                            }},
                            "trace_id": "{trace_id}"
                        }}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CREATED);
    }

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/feed?include_self=true&trace_id=trace-keep")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    let data = json["data"].as_array().unwrap();

    assert_eq!(data.len(), 1);
    assert_eq!(data[0]["trace_id"], "trace-keep");
    assert_eq!(data[0]["content"]["text"], "message for trace-keep");
}

#[tokio::test]
async fn test_publish_invalid_json_returns_error() {
    let (_engine, app) = create_test_app();

    // Invalid JSON body
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/publish")
                .header("content-type", "application/json")
                .body(Body::from("not valid json"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(
        response.status() == StatusCode::UNPROCESSABLE_ENTITY
            || response.status() == StatusCode::BAD_REQUEST
    );

    let json = json_body(response).await;
    assert_validation_error_code_and_field(&json, "INVALID_JSON_BODY", "body");
}

#[tokio::test]
async fn test_private_box_publish_stores_ciphertext_but_decrypts_for_sender() {
    let (sender, engine) = create_test_engine();
    let app = create_test_app_with_identity(sender.clone(), engine.clone());
    let recipient = Identity::generate();

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/publish")
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{
                        "content": {{
                            "type": "message",
                            "text": "top secret",
                            "encrypt_for": ["{}"]
                        }}
                    }}"#,
                    recipient.public_id().0
                )))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);

    let stored = engine.query(&Default::default()).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].content["type"], "private_box");

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/feed?include_self=true")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    let data = json["data"].as_array().unwrap();
    assert_eq!(data[0]["content"]["type"], "message");
    assert_eq!(data[0]["content"]["text"], "top secret");
}

#[tokio::test]
async fn test_private_box_remains_ciphertext_for_non_recipient() {
    let (sender, engine) = create_test_engine();
    let sender_app = create_test_app_with_identity(sender, engine.clone());
    let outsider_app = create_test_app_with_identity(Identity::generate(), engine.clone());
    let recipient = Identity::generate();

    let response = sender_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/publish")
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{
                        "content": {{
                            "type": "message",
                            "text": "top secret",
                            "encrypt_for": ["{}"]
                        }}
                    }}"#,
                    recipient.public_id().0
                )))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);

    let response = outsider_app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/feed?include_self=true")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    let data = json["data"].as_array().unwrap();
    assert_eq!(data[0]["content"]["type"], "private_box");
    assert!(data[0]["content"]["box"].is_string());
}

// ============ EVENTS (SSE) TESTS ============

#[tokio::test]
async fn test_events_endpoint_returns_sse_content_type() {
    let (_engine, app) = create_test_app();

    // Request the events endpoint
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    // Check content-type is text/event-stream
    let content_type = response
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap_or(""));
    assert!(
        content_type
            .map(|ct| ct.contains("text/event-stream"))
            .unwrap_or(false),
        "Expected text/event-stream content-type, got: {:?}",
        content_type
    );
}

#[tokio::test]
async fn test_events_endpoint_accepts_query_params() {
    let (_engine, app) = create_test_app();

    // Request with filter parameters
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/events?content_type=query&author=@test.ed25519")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // Should successfully connect even with filters
    assert_eq!(response.status(), StatusCode::OK);
}
