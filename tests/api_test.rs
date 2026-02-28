//! API integration tests for consumer groups, schemas, and topics.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use egregore::feed::engine::FeedEngine;
use egregore::feed::store::FeedStore;
use egregore::identity::Identity;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

/// Create test app state and router.
fn create_test_app() -> (Arc<FeedEngine>, axum::Router) {
    let identity = Identity::generate();
    let store = FeedStore::open_memory().unwrap();
    let engine = Arc::new(FeedEngine::new(store));

    // Build a minimal router for testing (similar to api/mod.rs)
    let state = TestAppState {
        engine: engine.clone(),
        identity: Arc::new(identity),
    };

    let app = axum::Router::new()
        // Groups routes
        .route("/v1/groups", axum::routing::post(create_group))
        .route("/v1/groups", axum::routing::get(list_groups))
        .route("/v1/groups/:id", axum::routing::get(get_group))
        .route("/v1/groups/:id", axum::routing::delete(delete_group))
        .route("/v1/groups/:id/members", axum::routing::get(get_group_members))
        .route("/v1/groups/:id/join", axum::routing::post(join_group))
        .route("/v1/groups/:id/leave", axum::routing::post(leave_group))
        .route("/v1/groups/:id/offsets", axum::routing::get(get_group_offsets))
        .route("/v1/groups/:id/offsets", axum::routing::post(commit_offset))
        // Topics routes
        .route("/v1/topics", axum::routing::get(list_topics))
        .route("/v1/topics/:topic", axum::routing::post(subscribe_topic))
        .route("/v1/topics/:topic", axum::routing::delete(unsubscribe_topic))
        .with_state(state);

    (engine, app)
}

#[derive(Clone)]
struct TestAppState {
    engine: Arc<FeedEngine>,
    #[allow(dead_code)]
    identity: Arc<Identity>,
}

// Re-implement minimal route handlers for testing
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

#[derive(Deserialize)]
struct CreateGroupRequest {
    group_id: String,
}

#[derive(Deserialize)]
struct JoinGroupRequest {
    member_id: String,
}

#[derive(Deserialize)]
struct LeaveGroupRequest {
    member_id: String,
}

#[derive(Deserialize)]
struct CommitOffsetRequest {
    author: String,
    sequence: u64,
    committed_by: String,
}

async fn create_group(
    State(state): State<TestAppState>,
    Json(req): Json<CreateGroupRequest>,
) -> impl IntoResponse {
    let engine = state.engine.clone();
    let group_id = req.group_id;
    match engine.store().create_group(&group_id) {
        Ok(group) => (StatusCode::CREATED, Json(json!({
            "success": true,
            "data": {
                "group_id": group.group_id,
                "generation": group.generation
            }
        }))),
        Err(_) => (StatusCode::CONFLICT, Json(json!({
            "success": false,
            "error": { "code": "GROUP_EXISTS" }
        }))),
    }
}

async fn list_groups(State(state): State<TestAppState>) -> impl IntoResponse {
    let groups = state.engine.store().list_groups().unwrap();
    Json(json!({
        "success": true,
        "data": groups.into_iter().map(|g| json!({
            "group_id": g.group_id,
            "generation": g.generation
        })).collect::<Vec<_>>()
    }))
}

async fn get_group(
    State(state): State<TestAppState>,
    Path(group_id): Path<String>,
) -> impl IntoResponse {
    match state.engine.store().get_group(&group_id).unwrap() {
        Some(group) => (StatusCode::OK, Json(json!({
            "success": true,
            "data": {
                "group_id": group.group_id,
                "generation": group.generation
            }
        }))),
        None => (StatusCode::NOT_FOUND, Json(json!({
            "success": false,
            "error": { "code": "GROUP_NOT_FOUND" }
        }))),
    }
}

async fn delete_group(
    State(state): State<TestAppState>,
    Path(group_id): Path<String>,
) -> impl IntoResponse {
    match state.engine.store().delete_group(&group_id).unwrap() {
        true => StatusCode::NO_CONTENT,
        false => StatusCode::NOT_FOUND,
    }
}

async fn get_group_members(
    State(state): State<TestAppState>,
    Path(group_id): Path<String>,
) -> impl IntoResponse {
    if state.engine.store().get_group(&group_id).unwrap().is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({
            "success": false,
            "error": { "code": "GROUP_NOT_FOUND" }
        })));
    }
    let members = state.engine.store().get_group_members(&group_id).unwrap();
    (StatusCode::OK, Json(json!({
        "success": true,
        "data": members.into_iter().map(|m| json!({
            "member_id": m.member_id.0
        })).collect::<Vec<_>>()
    })))
}

async fn join_group(
    State(state): State<TestAppState>,
    Path(group_id): Path<String>,
    Json(req): Json<JoinGroupRequest>,
) -> impl IntoResponse {
    use egregore::identity::PublicId;
    let member_id = PublicId(req.member_id);
    match state.engine.store().join_group(&group_id, &member_id) {
        Ok(result) => (StatusCode::OK, Json(json!({
            "success": true,
            "data": {
                "group_id": result.group_id,
                "member_id": result.member_id.0,
                "generation": result.generation,
                "is_leader": result.is_leader
            }
        }))),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({
            "success": false,
            "error": { "message": e.to_string() }
        }))),
    }
}

async fn leave_group(
    State(state): State<TestAppState>,
    Path(group_id): Path<String>,
    Json(req): Json<LeaveGroupRequest>,
) -> impl IntoResponse {
    use egregore::identity::PublicId;
    let member_id = PublicId(req.member_id);
    match state.engine.store().leave_group(&group_id, &member_id).unwrap() {
        true => StatusCode::NO_CONTENT,
        false => StatusCode::NOT_FOUND,
    }
}

async fn get_group_offsets(
    State(state): State<TestAppState>,
    Path(group_id): Path<String>,
) -> impl IntoResponse {
    if state.engine.store().get_group(&group_id).unwrap().is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({
            "success": false,
            "error": { "code": "GROUP_NOT_FOUND" }
        })));
    }
    let offsets = state.engine.store().get_group_offsets(&group_id).unwrap();
    (StatusCode::OK, Json(json!({
        "success": true,
        "data": offsets.into_iter().map(|o| json!({
            "author": o.author.0,
            "committed_sequence": o.committed_sequence
        })).collect::<Vec<_>>()
    })))
}

async fn commit_offset(
    State(state): State<TestAppState>,
    Path(group_id): Path<String>,
    Json(req): Json<CommitOffsetRequest>,
) -> impl IntoResponse {
    use egregore::identity::PublicId;
    let author = PublicId(req.author);
    let committed_by = PublicId(req.committed_by);
    match state.engine.store().commit_offset(&group_id, &author, req.sequence, &committed_by) {
        Ok(offset) => (StatusCode::OK, Json(json!({
            "success": true,
            "data": {
                "author": offset.author.0,
                "committed_sequence": offset.committed_sequence
            }
        }))),
        Err(e) => (StatusCode::FORBIDDEN, Json(json!({
            "success": false,
            "error": { "message": e.to_string() }
        }))),
    }
}

async fn list_topics(State(state): State<TestAppState>) -> impl IntoResponse {
    let topics = state.engine.store().get_topic_subscriptions().unwrap();
    Json(json!({
        "success": true,
        "data": topics
    }))
}

async fn subscribe_topic(
    State(state): State<TestAppState>,
    Path(topic): Path<String>,
) -> impl IntoResponse {
    state.engine.store().add_topic_subscription(&topic).unwrap();
    StatusCode::CREATED
}

async fn unsubscribe_topic(
    State(state): State<TestAppState>,
    Path(topic): Path<String>,
) -> impl IntoResponse {
    // Check if subscribed first
    let topics = state.engine.store().get_topic_subscriptions().unwrap();
    if !topics.contains(&topic) {
        return StatusCode::NOT_FOUND;
    }
    state.engine.store().remove_topic_subscription(&topic).unwrap();
    StatusCode::NO_CONTENT
}

// ============ TESTS ============

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
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body).unwrap();
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
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body).unwrap();
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
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body).unwrap();
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

    // Delete it
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/v1/groups/delete-test")
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
async fn test_topic_subscription_lifecycle() {
    let (_engine, app) = create_test_app();

    // Subscribe to a topic
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/topics/rust")
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
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert!(json["data"].as_array().unwrap().contains(&json!("rust")));

    // Unsubscribe
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/v1/topics/rust")
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

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert!(!json["data"].as_array().unwrap().contains(&json!("rust")));
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
