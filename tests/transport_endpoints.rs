//! Phase 2 Wave 5 Step 26 — backend endpoints for scry's bridge panel
//! (amendment §C.14).
//!
//! Three tests — kept small because these endpoints are thin wrappers
//! over CRUD helpers that already have unit-test coverage in
//! `src/feed/store/pending.rs`:
//!
//! 1. `/v1/transport/pending` count matches `pending_forwarding_count`.
//! 2. `/v1/transport/bus/authors` returns the expected rows when the
//!    `bus_author_seq_index` is populated.
//! 3. `/v1/transport/bus/authors` returns an empty list on a fresh
//!    node (no entries in `bus_author_seq_index`).

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

fn create_app() -> (Arc<FeedEngine>, axum::Router) {
    let identity = Identity::generate();
    let store = FeedStore::open_memory().unwrap();
    let engine = Arc::new(FeedEngine::new(store));
    let config = Config {
        api_auth_enabled: false,
        ..Config::default()
    };
    let tmp = tempfile::tempdir().unwrap();
    let blob_store = egregore::blob::BlobStore::new(tmp.path());
    let state = AppState {
        identity,
        engine: engine.clone(),
        config: Arc::new(config),
        started_at: Instant::now(),
        mcp_registry: mcp_registry::create_registry(),
        blob_store,
    };
    (engine, router_with_mcp(state, false))
}

async fn get_json(app: axum::Router, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    (status, v)
}

#[tokio::test]
async fn transport_pending_count_matches_pending_forwarding_count() {
    let (engine, app) = create_app();

    // Enqueue 2 pending messages under transport_id = "bus".
    let author = Identity::generate();
    let msg1 = engine
        .publish(
            &author,
            serde_json::json!({"type":"message","text":"one"}),
            None,
            vec![],
        )
        .unwrap();
    let msg2 = engine
        .publish(
            &author,
            serde_json::json!({"type":"message","text":"two"}),
            None,
            vec![],
        )
        .unwrap();
    engine
        .store()
        .pending_forwarding_enqueue("bus", &msg1)
        .unwrap();
    engine
        .store()
        .pending_forwarding_enqueue("bus", &msg2)
        .unwrap();

    let (status, body) = get_json(app, "/v1/transport/pending?transport_id=bus").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["success"], true);
    let data = &body["data"];
    assert_eq!(
        data["transport_id"], "bus",
        "endpoint echoes queried transport_id"
    );
    assert_eq!(
        data["count"], 2,
        "count must match the store's pending_forwarding_count"
    );
    assert!(
        data["rows"].as_array().unwrap().len() == 2,
        "rows array should contain both pending entries"
    );
}

#[tokio::test]
async fn bus_authors_returns_expected_rows() {
    let (engine, app) = create_app();

    // Populate bus_author_seq_index with 2 authors.
    engine
        .store()
        .bus_author_seq_index_insert("@alice.ed25519", 1, 100)
        .unwrap();
    engine
        .store()
        .bus_author_seq_index_insert("@alice.ed25519", 2, 101)
        .unwrap();
    engine
        .store()
        .bus_author_seq_index_insert("@bob.ed25519", 1, 102)
        .unwrap();

    let (status, body) = get_json(app, "/v1/transport/bus/authors").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["success"], true);
    let rows = body["data"]["rows"].as_array().expect("rows array");
    assert_eq!(rows.len(), 2, "two distinct authors expected");

    // Each row carries {author, last_indexed_at, author_seq, stream_seq}.
    for row in rows {
        assert!(row["author"].is_string());
        assert!(row["last_indexed_at"].is_string());
        assert!(row["author_seq"].is_number());
        assert!(row["stream_seq"].is_number());
    }
}

#[tokio::test]
async fn bus_authors_empty_when_index_empty() {
    let (_engine, app) = create_app();

    // No inserts into bus_author_seq_index — fresh node.
    let (status, body) = get_json(app, "/v1/transport/bus/authors").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["success"], true);
    let rows = body["data"]["rows"].as_array().expect("rows array");
    assert!(
        rows.is_empty(),
        "empty bus_author_seq_index must yield empty rows array"
    );
}
