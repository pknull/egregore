//! Topic subscription management API routes.
//!
//! Allows subscribing to topics (tags) for selective replication.
//! When topic subscriptions are configured, only messages with matching
//! tags will be replicated from peers.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Serialize;

use crate::api::response;
use crate::api::AppState;

/// Information about a topic subscription.
#[derive(Serialize)]
pub struct TopicInfo {
    pub topic: String,
}

/// Information about a known topic (discovered from messages).
#[derive(Serialize)]
pub struct KnownTopicInfo {
    pub topic: String,
    pub subscribed: bool,
}

/// Subscribe to a topic for selective replication.
pub async fn add_topic_subscription(
    State(state): State<AppState>,
    Path(topic): Path<String>,
) -> impl IntoResponse {
    let trimmed = topic.trim();
    if trimmed.is_empty() || trimmed.len() > 256 {
        return response::err_with_detail::<TopicInfo>(
            StatusCode::BAD_REQUEST,
            "INVALID_TOPIC",
            "topic must be 1-256 characters",
            response::validation_detail("topic", "must be 1-256 characters"),
        )
        .into_response();
    }

    let engine = state.engine.clone();
    let topic_owned = trimmed.to_string();

    if let Err(resp) = response::run_blocking(
        move || engine.store().add_topic_subscription(&topic_owned),
        "failed to add topic subscription",
    )
    .await
    {
        return resp;
    }

    (
        StatusCode::CREATED,
        response::ok(TopicInfo {
            topic: trimmed.to_string(),
        }),
    )
        .into_response()
}

/// Unsubscribe from a topic.
pub async fn remove_topic_subscription(
    State(state): State<AppState>,
    Path(topic): Path<String>,
) -> impl IntoResponse {
    let trimmed = topic.trim();
    if trimmed.is_empty() || trimmed.len() > 256 {
        return response::err_with_detail::<()>(
            StatusCode::BAD_REQUEST,
            "INVALID_TOPIC",
            "topic must be 1-256 characters",
            response::validation_detail("topic", "must be 1-256 characters"),
        )
        .into_response();
    }

    let engine = state.engine.clone();
    let topic_owned = trimmed.to_string();

    if let Err(resp) = response::run_blocking(
        move || engine.store().remove_topic_subscription(&topic_owned),
        "failed to remove topic subscription",
    )
    .await
    {
        return resp;
    }

    StatusCode::NO_CONTENT.into_response()
}

/// List all topic subscriptions.
pub async fn get_topic_subscriptions(State(state): State<AppState>) -> impl IntoResponse {
    let engine = state.engine.clone();

    let topics = match response::run_blocking(
        move || engine.store().get_topic_subscriptions(),
        "failed to list topic subscriptions",
    )
    .await
    {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let infos: Vec<TopicInfo> = topics
        .into_iter()
        .map(|topic| TopicInfo { topic })
        .collect();
    response::ok(infos).into_response()
}

/// List all known topics (discovered from messages) with subscription status.
pub async fn get_known_topics(State(state): State<AppState>) -> impl IntoResponse {
    let engine = state.engine.clone();

    let infos = match response::run_blocking(
        move || {
            let store = engine.store();
            let known = store.get_all_known_topics()?;
            let subscribed = store.get_topic_subscriptions()?;
            let subscribed_set: std::collections::HashSet<String> =
                subscribed.into_iter().collect();

            let infos: Vec<KnownTopicInfo> = known
                .into_iter()
                .map(|topic| KnownTopicInfo {
                    subscribed: subscribed_set.contains(&topic),
                    topic,
                })
                .collect();
            Ok::<_, crate::error::EgreError>(infos)
        },
        "failed to list known topics",
    )
    .await
    {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    response::ok(infos).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_info_serializes() {
        let info = TopicInfo {
            topic: "rust-lang".to_string(),
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("rust-lang"));
    }

    #[test]
    fn known_topic_info_serializes() {
        let info = KnownTopicInfo {
            topic: "llm".to_string(),
            subscribed: true,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("llm"));
        assert!(json.contains("true"));
    }
}
