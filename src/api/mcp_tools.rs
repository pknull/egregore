//! MCP tool definitions and handlers — 11 tools wrapping the REST API.
//!
//! Each tool maps 1:1 to a REST endpoint. Tool schemas define the JSON
//! input parameters; handlers delegate to the same FeedEngine/FeedStore
//! operations as the REST routes.

use serde::Serialize;
use serde_json::Value;

use super::routes_mesh;
use super::routes_peers;
use super::AppState;
use egregore::feed::models::FeedQuery;
use egregore::identity::PublicId;

#[derive(Debug, Serialize)]
pub struct ToolDefinition {
    pub name: &'static str,
    pub description: &'static str,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

#[derive(Debug, Serialize)]
pub struct ToolCallResult {
    pub content: Vec<ToolContent>,
    #[serde(rename = "isError")]
    pub is_error: bool,
}

#[derive(Debug, Serialize)]
pub struct ToolContent {
    #[serde(rename = "type")]
    pub content_type: &'static str,
    pub text: String,
}

impl ToolCallResult {
    fn text(text: String) -> Self {
        Self {
            content: vec![ToolContent {
                content_type: "text",
                text,
            }],
            is_error: false,
        }
    }

    fn error(message: String) -> Self {
        Self {
            content: vec![ToolContent {
                content_type: "text",
                text: message,
            }],
            is_error: true,
        }
    }

    /// Map a spawn_blocking result, collapsing both JoinError and app errors to generic "internal error".
    fn from_blocking<T>(
        result: std::result::Result<egregore::error::Result<T>, tokio::task::JoinError>,
        on_ok: impl FnOnce(T) -> Self,
    ) -> Self {
        match result {
            Ok(Ok(val)) => on_ok(val),
            _ => Self::error("internal error".into()),
        }
    }
}

pub fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "egregore_status",
            description: "Get daemon status including version, identity, \
                and feed/message/peer/follow counts",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDefinition {
            name: "egregore_identity",
            description: "Get the local daemon's public identity",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDefinition {
            name: "egregore_publish",
            description: "Publish content to the local Egregore feed",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "content": {
                        "description": "JSON content to publish (any valid JSON)"
                    },
                    "relates": {
                        "type": "string",
                        "description": "Hash of a related message (optional, for threading)"
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Categorization tags (optional)"
                    },
                    "trace_id": {
                        "type": "string",
                        "description": "Distributed tracing identifier (optional)"
                    },
                    "span_id": {
                        "type": "string",
                        "description": "Distributed tracing span identifier (optional)"
                    }
                },
                "required": ["content"]
            }),
        },
        ToolDefinition {
            name: "egregore_query",
            description: "Query feed messages by author, search text, content type, tag, or related message",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "author": {
                        "type": "string",
                        "description": "Author public ID to filter by"
                    },
                    "search": {
                        "type": "string",
                        "description": "Full-text search query"
                    },
                    "content_type": {
                        "type": "string",
                        "description": "Content type filter (e.g. insight, profile)"
                    },
                    "tag": {
                        "type": "string",
                        "description": "Filter by tag"
                    },
                    "relates": {
                        "type": "string",
                        "description": "Filter by related message hash (find replies/responses)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max results (default 50, max 200)"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Pagination offset"
                    }
                }
            }),
        },
        ToolDefinition {
            name: "egregore_peers",
            description: "List configured gossip peers from all sources",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDefinition {
            name: "egregore_add_peer",
            description: "Add a gossip peer by address",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "address": {
                        "type": "string",
                        "description": "Peer address in host:port format"
                    }
                },
                "required": ["address"]
            }),
        },
        ToolDefinition {
            name: "egregore_remove_peer",
            description: "Remove a gossip peer by address",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "address": {
                        "type": "string",
                        "description": "Peer address to remove"
                    }
                },
                "required": ["address"]
            }),
        },
        ToolDefinition {
            name: "egregore_follows",
            description: "List all followed authors",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDefinition {
            name: "egregore_follow",
            description: "Subscribe to an author's feed for replication",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "author": {
                        "type": "string",
                        "description": "Public ID of the author to follow"
                    }
                },
                "required": ["author"]
            }),
        },
        ToolDefinition {
            name: "egregore_unfollow",
            description: "Unsubscribe from an author's feed",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "author": {
                        "type": "string",
                        "description": "Public ID of the author to unfollow"
                    }
                },
                "required": ["author"]
            }),
        },
        ToolDefinition {
            name: "egregore_mesh",
            description: "Get mesh-wide peer health status showing all known peers, \
                their last-seen times, and health status (recent/stale/suspected)",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },
    ]
}

pub async fn dispatch_tool(name: &str, args: Value, state: &AppState) -> ToolCallResult {
    match name {
        "egregore_status" => tool_status(state).await,
        "egregore_identity" => tool_identity(state),
        "egregore_publish" => tool_publish(args, state).await,
        "egregore_query" => tool_query(args, state).await,
        "egregore_peers" => tool_peers(state).await,
        "egregore_add_peer" => tool_add_peer(args, state).await,
        "egregore_remove_peer" => tool_remove_peer(args, state).await,
        "egregore_follows" => tool_follows(state).await,
        "egregore_follow" => tool_follow(args, state).await,
        "egregore_unfollow" => tool_unfollow(args, state).await,
        "egregore_mesh" => tool_mesh(state).await,
        _ => ToolCallResult::error(format!("Unknown tool: {name}")),
    }
}

async fn tool_status(state: &AppState) -> ToolCallResult {
    let status = routes_peers::build_status(state).await;
    ToolCallResult::text(serde_json::to_string_pretty(&status).unwrap())
}

fn tool_identity(state: &AppState) -> ToolCallResult {
    let pub_id = state.identity.public_id();
    let result = serde_json::json!({ "public_id": pub_id.0 });
    ToolCallResult::text(serde_json::to_string_pretty(&result).unwrap())
}

async fn tool_publish(args: Value, state: &AppState) -> ToolCallResult {
    let content = match args.get("content") {
        Some(c) if !c.is_null() => c.clone(),
        _ => return ToolCallResult::error("Missing required argument: content".into()),
    };
    let relates = args
        .get("relates")
        .and_then(|v| v.as_str())
        .map(String::from);
    let tags: Vec<String> = args
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let trace_id = args
        .get("trace_id")
        .and_then(|v| v.as_str())
        .map(String::from);
    let span_id = args
        .get("span_id")
        .and_then(|v| v.as_str())
        .map(String::from);

    let identity = state.identity.clone();
    let engine = state.engine.clone();

    let result = tokio::task::spawn_blocking(move || {
        engine.publish_with_trace(&identity, content, relates, tags, trace_id, span_id)
    })
    .await;
    ToolCallResult::from_blocking(result, |msg| {
        ToolCallResult::text(serde_json::to_string_pretty(&msg).unwrap())
    })
}

async fn tool_query(args: Value, state: &AppState) -> ToolCallResult {
    let search = args.get("search").and_then(|v| v.as_str()).map(String::from);
    let author = args
        .get("author")
        .and_then(|v| v.as_str())
        .map(|s| PublicId(s.to_string()));
    let content_type = args
        .get("content_type")
        .and_then(|v| v.as_str())
        .map(String::from);
    let tag = args.get("tag").and_then(|v| v.as_str()).map(String::from);
    let relates = args
        .get("relates")
        .and_then(|v| v.as_str())
        .map(String::from);
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|v| v.min(200) as u32);
    let offset = args
        .get("offset")
        .and_then(|v| v.as_u64())
        .map(|v| v.min(u32::MAX as u64) as u32);

    let engine = state.engine.clone();

    let pretty = |msgs| ToolCallResult::text(serde_json::to_string_pretty(&msgs).unwrap());

    if let Some(search_text) = search {
        let limit = limit.unwrap_or(20);
        let result =
            tokio::task::spawn_blocking(move || engine.search(&search_text, limit)).await;
        ToolCallResult::from_blocking(result, pretty)
    } else {
        let query = FeedQuery {
            author,
            content_type,
            tag,
            relates,
            limit,
            offset,
            ..Default::default()
        };
        let result = tokio::task::spawn_blocking(move || engine.query(&query)).await;
        ToolCallResult::from_blocking(result, pretty)
    }
}

async fn tool_peers(state: &AppState) -> ToolCallResult {
    let peers = routes_peers::aggregate_peers(state).await;
    ToolCallResult::text(serde_json::to_string_pretty(&peers).unwrap())
}

async fn tool_add_peer(args: Value, state: &AppState) -> ToolCallResult {
    let address = match args.get("address").and_then(|v| v.as_str()) {
        Some(a) => a.to_string(),
        None => return ToolCallResult::error("Missing required argument: address".into()),
    };

    if !routes_peers::is_valid_peer_address(&address) {
        return ToolCallResult::error("address must be in host:port format".into());
    }

    let engine = state.engine.clone();
    let addr = address.clone();
    let result =
        tokio::task::spawn_blocking(move || engine.store().insert_address_peer(&addr)).await;

    ToolCallResult::from_blocking(result, |()| {
        ToolCallResult::text(serde_json::json!({ "added": address }).to_string())
    })
}

async fn tool_remove_peer(args: Value, state: &AppState) -> ToolCallResult {
    let address = match args.get("address").and_then(|v| v.as_str()) {
        Some(a) => a.to_string(),
        None => return ToolCallResult::error("Missing required argument: address".into()),
    };

    if !routes_peers::is_valid_peer_address(&address) {
        return ToolCallResult::error("address must be in host:port format".into());
    }

    let engine = state.engine.clone();
    let addr = address.clone();
    let result =
        tokio::task::spawn_blocking(move || engine.store().remove_address_peer(&addr)).await;

    ToolCallResult::from_blocking(result, |()| {
        ToolCallResult::text(serde_json::json!({ "removed": address }).to_string())
    })
}

async fn tool_follows(state: &AppState) -> ToolCallResult {
    let engine = state.engine.clone();
    let result = tokio::task::spawn_blocking(move || engine.store().get_follows()).await;

    ToolCallResult::from_blocking(result, |follows| {
        let authors: Vec<&str> = follows.iter().map(|p| p.0.as_str()).collect();
        ToolCallResult::text(serde_json::to_string_pretty(&authors).unwrap())
    })
}

async fn tool_follow(args: Value, state: &AppState) -> ToolCallResult {
    let author = match args.get("author").and_then(|v| v.as_str()) {
        Some(a) => a.to_string(),
        None => return ToolCallResult::error("Missing required argument: author".into()),
    };
    if !PublicId::is_valid_format(&author) {
        return ToolCallResult::error("author must be in @<base64>.ed25519 format".into());
    }

    let engine = state.engine.clone();
    let author_id = PublicId(author.clone());
    let result =
        tokio::task::spawn_blocking(move || engine.store().add_follow(&author_id)).await;

    ToolCallResult::from_blocking(result, |()| {
        ToolCallResult::text(serde_json::json!({ "followed": author }).to_string())
    })
}

async fn tool_unfollow(args: Value, state: &AppState) -> ToolCallResult {
    let author = match args.get("author").and_then(|v| v.as_str()) {
        Some(a) => a.to_string(),
        None => return ToolCallResult::error("Missing required argument: author".into()),
    };
    if !PublicId::is_valid_format(&author) {
        return ToolCallResult::error("author must be in @<base64>.ed25519 format".into());
    }

    let engine = state.engine.clone();
    let author_id = PublicId(author.clone());
    let result =
        tokio::task::spawn_blocking(move || engine.store().remove_follow(&author_id)).await;

    ToolCallResult::from_blocking(result, |()| {
        ToolCallResult::text(serde_json::json!({ "unfollowed": author }).to_string())
    })
}

async fn tool_mesh(state: &AppState) -> ToolCallResult {
    let mesh = routes_mesh::build_mesh_health(state).await;
    ToolCallResult::text(serde_json::to_string_pretty(&mesh).unwrap())
}
