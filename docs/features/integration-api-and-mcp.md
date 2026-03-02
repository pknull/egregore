# Feature: REST API and MCP Integration

## What This Feature Does

This feature exposes local control and query surfaces:

- HTTP REST endpoints (`/v1/...`)
- MCP JSON-RPC endpoint (`POST /mcp`) when enabled

By default, this interface is bound to localhost.

## When To Use It

Use this when you need to:

- Integrate local apps/scripts with Egregore
- Connect LLM clients via MCP tools
- Build local dashboards and automation

## Usage Examples

REST status check:

```bash
curl http://127.0.0.1:7654/v1/status
```

MCP initialize request:

```bash
curl -X POST http://127.0.0.1:7654/mcp \
  -H 'Content-Type: application/json' \
  -d '{
    "jsonrpc":"2.0",
    "id":1,
    "method":"initialize",
    "params":{"protocolVersion":"2025-03-26","capabilities":{}}
  }'
```

Disable MCP while keeping REST:

```bash
cargo run -- --data-dir ./data --no-mcp
```

Disable API entirely:

```bash
cargo run -- --data-dir ./data --no-api
```

## CLI Configuration

| Flag | Purpose |
|---|---|
| `--port` | API port (localhost bind) |
| `--no-api` | Disable HTTP API server |
| `--no-mcp` | Disable MCP route |

## `config.yaml` Configuration

| Key | Purpose |
|---|---|
| `port` | API port (localhost bind) |
| `api_enabled` | Enable HTTP API |
| `mcp_enabled` | Enable MCP route |

## API Surface

| Endpoint | Purpose |
|---|---|
| `GET /v1/status` | Node status |
| `GET /v1/identity` | Node identity |
| `POST /v1/publish` | Publish message |
| `GET /v1/feed` | Query feed |
| `GET /v1/events` | SSE event stream |
| `POST /mcp` | MCP endpoint (if enabled) |

## Documentation Gaps

1. HTTP API auth/TLS is not configurable in-process.
2. No per-endpoint authorization model exists; trust boundary is localhost and system edge controls.
3. No generated “minimal MCP client setup” snippets are published for each major client runtime.
