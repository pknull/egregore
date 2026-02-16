//! MCP JSON-RPC 2.0 dispatcher — POST /mcp (Streamable HTTP transport).
//!
//! Native LLM interface. Handles initialize, ping, tools/list, tools/call.
//! Notifications (no `id` field) return 202 Accepted with empty body.
//! Tool definitions and handlers live in mcp_tools.rs.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::mcp_tools;
use super::AppState;

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

#[derive(Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

#[derive(Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    id: Value,
    result: Value,
}

#[derive(Serialize)]
struct JsonRpcErrorResponse {
    jsonrpc: &'static str,
    id: Value,
    error: JsonRpcErrorDetail,
}

#[derive(Serialize)]
struct JsonRpcErrorDetail {
    code: i64,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

#[derive(Serialize)]
struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    protocol_version: &'static str,
    capabilities: Capabilities,
    #[serde(rename = "serverInfo")]
    server_info: ServerInfo,
    instructions: &'static str,
}

#[derive(Serialize)]
struct Capabilities {
    tools: ToolsCapability,
}

#[derive(Serialize)]
struct ToolsCapability {}

#[derive(Serialize)]
struct ServerInfo {
    name: &'static str,
    version: &'static str,
}

fn rpc_ok(id: Value, result: Value) -> Response {
    Json(JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result,
    })
    .into_response()
}

fn rpc_err(id: Value, code: i64, message: String) -> Response {
    Json(JsonRpcErrorResponse {
        jsonrpc: "2.0",
        id,
        error: JsonRpcErrorDetail {
            code,
            message,
            data: None,
        },
    })
    .into_response()
}

pub async fn mcp_handler(State(state): State<AppState>, body: String) -> Response {
    let req: JsonRpcRequest = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(_) => return rpc_err(Value::Null, PARSE_ERROR, "Parse error".into()),
    };

    if req.jsonrpc != "2.0" {
        return rpc_err(
            req.id.unwrap_or(Value::Null),
            INVALID_REQUEST,
            "jsonrpc must be \"2.0\"".into(),
        );
    }

    let id = match req.id {
        Some(id) => id,
        None => return StatusCode::ACCEPTED.into_response(),
    };

    match req.method.as_str() {
        "initialize" => {
            let result = InitializeResult {
                protocol_version: "2025-03-26",
                capabilities: Capabilities {
                    tools: ToolsCapability {},
                },
                server_info: ServerInfo {
                    name: "egregore",
                    version: env!("CARGO_PKG_VERSION"),
                },
                instructions: "Egregore is a decentralized knowledge sharing network for LLMs. \
                    Use tools to publish insights, query feeds, manage peers and follows.",
            };
            rpc_ok(id, serde_json::to_value(result).unwrap())
        }
        "ping" => rpc_ok(id, Value::Object(serde_json::Map::new())),
        "tools/list" => {
            let tools = mcp_tools::tool_definitions();
            rpc_ok(id, serde_json::json!({ "tools": tools }))
        }
        "tools/call" => {
            let params = req.params.unwrap_or(Value::Object(serde_json::Map::new()));
            let name = match params.get("name").and_then(|v| v.as_str()) {
                Some(n) => n.to_string(),
                None => return rpc_err(id, INVALID_PARAMS, "Missing tool name".into()),
            };
            let arguments = match params.get("arguments") {
                Some(Value::Null) | None => Value::Object(serde_json::Map::new()),
                Some(v) if v.is_object() => v.clone(),
                Some(_) => {
                    return rpc_err(id, INVALID_PARAMS, "arguments must be an object".into())
                }
            };
            let result = mcp_tools::dispatch_tool(&name, arguments, &state).await;
            rpc_ok(id, serde_json::to_value(result).unwrap())
        }
        _ => rpc_err(id, METHOD_NOT_FOUND, "Method not found".into()),
    }
}

pub async fn mcp_method_not_allowed() -> impl IntoResponse {
    StatusCode::METHOD_NOT_ALLOWED
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::router;
    use axum::body::Body;
    use axum::http::Request;
    use egregore::config::Config;
    use egregore::feed::engine::FeedEngine;
    use egregore::feed::store::FeedStore;
    use egregore::identity::Identity;
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use std::time::Instant;
    use tower::ServiceExt;

    fn test_state() -> AppState {
        let store = FeedStore::open_memory().unwrap();
        let engine = FeedEngine::new(store);
        AppState {
            identity: Identity::generate(),
            engine: Arc::new(engine),
            config: Arc::new(Config::default()),
            started_at: Instant::now(),
        }
    }

    async fn rpc_post(app: axum::Router, body: &Value) -> (StatusCode, Option<Value>) {
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        if bytes.is_empty() {
            (status, None)
        } else {
            (status, Some(serde_json::from_slice(&bytes).unwrap()))
        }
    }

    #[tokio::test]
    async fn initialize() {
        let state = test_state();
        let app = router(state);
        let (status, body) = rpc_post(
            app,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {}
                }
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let body = body.unwrap();
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["id"], 1);
        assert_eq!(body["result"]["protocolVersion"], "2025-03-26");
        assert!(body["result"]["capabilities"]["tools"].is_object());
        assert_eq!(body["result"]["serverInfo"]["name"], "egregore");
    }

    #[tokio::test]
    async fn notification_accepted() {
        let state = test_state();
        let app = router(state);
        let (status, body) = rpc_post(
            app,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            }),
        )
        .await;

        assert_eq!(status, StatusCode::ACCEPTED);
        assert!(body.is_none());
    }

    #[tokio::test]
    async fn ping() {
        let state = test_state();
        let app = router(state);
        let (status, body) = rpc_post(
            app,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "ping"
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let body = body.unwrap();
        assert_eq!(body["result"], serde_json::json!({}));
    }

    #[tokio::test]
    async fn tools_list() {
        let state = test_state();
        let app = router(state);
        let (status, body) = rpc_post(
            app,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/list"
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let body = body.unwrap();
        let tools = body["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 10);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"egregore_status"));
        assert!(names.contains(&"egregore_publish"));
        assert!(names.contains(&"egregore_query"));
        assert!(names.contains(&"egregore_peers"));
        assert!(names.contains(&"egregore_add_peer"));
        assert!(names.contains(&"egregore_remove_peer"));
        assert!(names.contains(&"egregore_follows"));
        assert!(names.contains(&"egregore_follow"));
        assert!(names.contains(&"egregore_unfollow"));
        assert!(names.contains(&"egregore_identity"));

        // Verify schemas have required fields
        for tool in tools {
            assert!(tool["inputSchema"]["type"].as_str() == Some("object"));
        }
    }

    #[tokio::test]
    async fn tool_status() {
        let state = test_state();
        let app = router(state);
        let (status, body) = rpc_post(
            app,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": { "name": "egregore_status" }
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let body = body.unwrap();
        assert_eq!(body["result"]["isError"], false);
        let text = body["result"]["content"][0]["text"].as_str().unwrap();
        let data: Value = serde_json::from_str(text).unwrap();
        assert!(data["version"].is_string());
        assert!(data["identity"].is_string());
        assert!(data["message_count"].is_number());
    }

    #[tokio::test]
    async fn tool_publish_and_query() {
        let state = test_state();

        // Publish
        let app = router(state.clone());
        let (status, body) = rpc_post(
            app,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "tools/call",
                "params": {
                    "name": "egregore_publish",
                    "arguments": {
                        "content": {
                            "type": "insight",
                            "title": "Test MCP Insight",
                            "observation": "MCP integration works",
                            "confidence": 0.9,
                            "tags": ["test"]
                        }
                    }
                }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body = body.unwrap();
        assert_eq!(body["result"]["isError"], false);

        // Query back
        let app = router(state.clone());
        let (status, body) = rpc_post(
            app,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 6,
                "method": "tools/call",
                "params": {
                    "name": "egregore_query",
                    "arguments": { "limit": 10 }
                }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body = body.unwrap();
        assert_eq!(body["result"]["isError"], false);
        let text = body["result"]["content"][0]["text"].as_str().unwrap();
        let msgs: Vec<Value> = serde_json::from_str(text).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["content"]["title"], "Test MCP Insight");
    }

    #[tokio::test]
    async fn tool_peer_management() {
        let state = test_state();

        // Add peer
        let app = router(state.clone());
        let (_, body) = rpc_post(
            app,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "tools/call",
                "params": {
                    "name": "egregore_add_peer",
                    "arguments": { "address": "10.0.0.1:7655" }
                }
            }),
        )
        .await;
        assert_eq!(body.unwrap()["result"]["isError"], false);

        // List peers
        let app = router(state.clone());
        let (_, body) = rpc_post(
            app,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 8,
                "method": "tools/call",
                "params": { "name": "egregore_peers" }
            }),
        )
        .await;
        let body = body.unwrap();
        let text = body["result"]["content"][0]["text"].as_str().unwrap();
        let peers: Vec<Value> = serde_json::from_str(text).unwrap();
        assert!(peers.iter().any(|p| p["address"] == "10.0.0.1:7655"));

        // Remove peer
        let app = router(state.clone());
        let (_, body) = rpc_post(
            app,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 9,
                "method": "tools/call",
                "params": {
                    "name": "egregore_remove_peer",
                    "arguments": { "address": "10.0.0.1:7655" }
                }
            }),
        )
        .await;
        assert_eq!(body.unwrap()["result"]["isError"], false);
    }

    #[tokio::test]
    async fn tool_follow_management() {
        let state = test_state();
        let author = "@AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=.ed25519";

        // Follow
        let app = router(state.clone());
        let (_, body) = rpc_post(
            app,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 10,
                "method": "tools/call",
                "params": {
                    "name": "egregore_follow",
                    "arguments": { "author": author }
                }
            }),
        )
        .await;
        assert_eq!(body.unwrap()["result"]["isError"], false);

        // List follows
        let app = router(state.clone());
        let (_, body) = rpc_post(
            app,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 11,
                "method": "tools/call",
                "params": { "name": "egregore_follows" }
            }),
        )
        .await;
        let body = body.unwrap();
        let text = body["result"]["content"][0]["text"].as_str().unwrap();
        let follows: Vec<String> = serde_json::from_str(text).unwrap();
        assert!(follows.contains(&author.to_string()));

        // Unfollow
        let app = router(state.clone());
        let (_, body) = rpc_post(
            app,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 12,
                "method": "tools/call",
                "params": {
                    "name": "egregore_unfollow",
                    "arguments": { "author": author }
                }
            }),
        )
        .await;
        assert_eq!(body.unwrap()["result"]["isError"], false);
    }

    #[tokio::test]
    async fn unknown_method() {
        let state = test_state();
        let app = router(state);
        let (status, body) = rpc_post(
            app,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 13,
                "method": "nonexistent/method"
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let body = body.unwrap();
        assert_eq!(body["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn invalid_json() {
        let state = test_state();
        let app = router(state);
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from("not valid json{{{"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], -32700);
    }

    #[tokio::test]
    async fn unknown_tool() {
        let state = test_state();
        let app = router(state);
        let (_, body) = rpc_post(
            app,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 14,
                "method": "tools/call",
                "params": {
                    "name": "nonexistent_tool",
                    "arguments": {}
                }
            }),
        )
        .await;

        let body = body.unwrap();
        assert_eq!(body["result"]["isError"], true);
        let text = body["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("Unknown tool"));
    }

    #[tokio::test]
    async fn get_returns_405() {
        let state = test_state();
        let app = router(state);
        let req = Request::builder()
            .method("GET")
            .uri("/mcp")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }
}
