//! Server-Sent Events endpoint — stream messages in real-time.
//!
//! Subscribes to the FeedEngine's broadcast channel and streams
//! new messages as SSE events. Supports filtering by content_type
//! and author.

use std::convert::Infallible;

use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::stream::Stream;
use serde::Deserialize;
use tokio::sync::broadcast;

use crate::api::AppState;

/// Query parameters for event subscription.
#[derive(Deserialize, Default)]
pub struct EventParams {
    /// Filter by content type (e.g., "query", "insight").
    pub content_type: Option<String>,
    /// Filter by author public ID.
    pub author: Option<String>,
}

/// Subscribe to message events via SSE.
///
/// Returns a stream of events as messages are published or ingested.
/// Each event contains the full message JSON.
///
/// # Query Parameters
///
/// - `content_type`: Only receive messages matching this type
/// - `author`: Only receive messages from this author
///
/// # Example
///
/// ```bash
/// curl -N "http://localhost:7654/v1/events?content_type=query"
/// ```
pub async fn subscribe(
    State(state): State<AppState>,
    Query(params): Query<EventParams>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = state.engine.subscribe();

    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(msg) => {
                    // Apply content_type filter
                    if let Some(ref ct) = params.content_type {
                        let msg_type = msg
                            .content
                            .get("type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if msg_type != ct {
                            continue;
                        }
                    }

                    // Apply author filter
                    if let Some(ref author) = params.author {
                        if msg.author.0 != *author {
                            continue;
                        }
                    }

                    // Serialize and yield event
                    if let Ok(json) = serde_json::to_string(msg.as_ref()) {
                        yield Ok(Event::default().data(json));
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    // Client fell behind, notify them
                    tracing::warn!(lagged = n, "SSE client lagged behind");
                    yield Ok(Event::default()
                        .event("lagged")
                        .data(format!("missed {} events", n)));
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::debug!("SSE channel closed");
                    break;
                }
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}
