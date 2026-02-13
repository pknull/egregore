pub mod response;
pub mod routes_feed;
pub mod routes_identity;
pub mod routes_peers;
pub mod routes_publish;

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;

use crate::config::Config;
use crate::feed::engine::FeedEngine;
use crate::identity::Identity;

#[derive(Clone)]
pub struct AppState {
    pub identity: Identity,
    pub engine: Arc<FeedEngine>,
    pub config: Arc<Config>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/identity", get(routes_identity::get_identity))
        .route("/v1/publish", post(routes_publish::publish))
        .route("/v1/feed", get(routes_feed::get_own_feed))
        .route("/v1/feed/{author}", get(routes_feed::get_feed_by_author))
        .route("/v1/insights", get(routes_feed::get_insights))
        .route("/v1/insights/search", get(routes_feed::search_insights))
        .route("/v1/message/{hash}", get(routes_feed::get_message_by_hash))
        .route("/v1/peers", get(routes_peers::get_peers).post(routes_peers::add_peer))
        .route("/v1/status", get(routes_peers::get_status))
        .with_state(state)
}
