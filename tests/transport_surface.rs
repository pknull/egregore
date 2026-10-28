//! HTTP transport surface (RFC 0001 §19 A2, A8): the `/v1/transport/*`
//! family is not routed, and `/v1/status` reports the attached transport's
//! health as the bare single-transport object.

mod common;

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

use common::mock_transport::MockTransport;

fn create_app(engine: Arc<FeedEngine>) -> axum::Router {
    let tmp = tempfile::tempdir().unwrap();
    let state = AppState {
        identity: Identity::generate(),
        engine,
        config: Arc::new(Config {
            api_auth_enabled: false,
            ..Config::default()
        }),
        started_at: Instant::now(),
        mcp_registry: mcp_registry::create_registry(),
        blob_store: egregore::blob::BlobStore::new(tmp.path()),
    };
    router_with_mcp(state, false)
}

async fn get(app: axum::Router, uri: &str) -> (StatusCode, Vec<u8>) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, bytes.to_vec())
}

#[tokio::test]
async fn transport_route_family_is_not_routed() {
    let engine = Arc::new(FeedEngine::new(FeedStore::open_memory().unwrap()));
    for uri in [
        "/v1/transport/pending?transport_id=bus",
        "/v1/transport/bus/authors",
    ] {
        let (status, _) = get(create_app(engine.clone()), uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri} must not be routed");
    }
}

#[tokio::test]
async fn status_reports_the_single_attached_transport() {
    let engine = Arc::new(FeedEngine::new(FeedStore::open_memory().unwrap()));
    let mock: Arc<dyn egregore::transport::Transport> = MockTransport::new();
    engine.attach_transport(mock);

    let (status, body) = get(create_app(engine), "/v1/status").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_slice(&body).unwrap();
    let transport = json["data"]["transport"]
        .as_object()
        .expect("/v1/status must carry the transport object when one is attached");

    assert_eq!(transport["backend"], "mock");
    assert_eq!(transport["connected"], true);
    assert_eq!(transport["unreplicated_count"], 0);
    let mut keys: Vec<&str> = transport.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "backend",
            "connected",
            "inflight_publishes",
            "unreplicated_count"
        ],
        "a single idle transport serializes only its required fields"
    );
}
