//! HTTP API — localhost-only REST + MCP JSON-RPC endpoint.
//!
//! All endpoints bind to 127.0.0.1 (binding happens in main.rs). Only local
//! processes can access the API — there is no authentication on the HTTP layer.
//! The security boundary is the loopback interface itself.
//!
//! Routes: feed queries, publish, peer management, follows, identity, status,
//! and the MCP JSON-RPC 2.0 endpoint at POST /mcp.

pub mod mcp;
pub mod mcp_tools;
pub mod response;
pub mod routes_feed;
pub mod routes_follows;
pub mod routes_identity;
pub mod routes_peers;
pub mod routes_publish;

use std::sync::Arc;
use std::time::Instant;

use axum::extract::DefaultBodyLimit;
use axum::routing::{delete, get, post};
use axum::Router;

use egregore::config::Config;
use egregore::feed::engine::FeedEngine;
use egregore::identity::Identity;

#[derive(Clone)]
pub struct AppState {
    pub identity: Identity,
    pub engine: Arc<FeedEngine>,
    pub config: Arc<Config>,
    pub started_at: Instant,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/identity", get(routes_identity::get_identity))
        .route("/v1/publish", post(routes_publish::publish))
        .route("/v1/feed", get(routes_feed::get_own_feed))
        .route("/v1/feed/:author", get(routes_feed::get_feed_by_author))
        .route("/v1/insights", get(routes_feed::get_insights))
        .route("/v1/insights/search", get(routes_feed::search_insights))
        .route("/v1/message/:hash", get(routes_feed::get_message_by_hash))
        .route(
            "/v1/peers",
            get(routes_peers::get_peers).post(routes_peers::add_peer),
        )
        .route(
            "/v1/peers/:address",
            delete(routes_peers::delete_peer),
        )
        .route("/v1/status", get(routes_peers::get_status))
        .route(
            "/v1/follows/:author",
            post(routes_follows::add_follow).delete(routes_follows::remove_follow),
        )
        .route("/v1/follows", get(routes_follows::get_follows))
        .route(
            "/mcp",
            post(mcp::mcp_handler)
                .get(mcp::mcp_method_not_allowed)
                .delete(mcp::mcp_method_not_allowed),
        )
        .layer(DefaultBodyLimit::max(1024 * 1024)) // 1 MB
        .with_state(state)
}
