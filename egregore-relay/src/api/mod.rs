//! Relay HTTP API — publicly accessible (binds 0.0.0.0).
//!
//! Registration, peer directory, feed firehose, and status. No MCP endpoint
//! (relays don't publish, they store and forward). No authentication on read
//! endpoints — anyone can query the feed and peer directory.

pub mod routes_directory;
pub mod routes_feed;
pub mod routes_register;
pub mod routes_status;

use std::sync::Arc;
use std::time::Instant;

use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum::Router;

use egregore::config::Config;
use egregore::feed::engine::FeedEngine;
use egregore::identity::Identity;

use crate::config::RelayConfig;

#[derive(Clone)]
pub struct RelayState {
    pub identity: Identity,
    pub engine: Arc<FeedEngine>,
    pub config: Arc<Config>,
    pub relay_config: Arc<RelayConfig>,
    pub started_at: Instant,
}

pub fn router(state: RelayState) -> Router {
    Router::new()
        .route("/v1/register", post(routes_register::register))
        .route(
            "/v1/settings",
            post(routes_register::update_settings),
        )
        .route("/v1/peers", get(routes_directory::get_directory))
        .route("/v1/feed", get(routes_feed::get_firehose))
        .route(
            "/v1/feed/:author",
            get(routes_feed::get_feed_by_author),
        )
        .route("/v1/status", get(routes_status::get_status))
        .layer(DefaultBodyLimit::max(1024 * 1024)) // 1 MB
        .with_state(state)
}
