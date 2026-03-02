# Architecture Slice: REST API and MCP

## Scope

Defines local integration surfaces exposed by the node process.

## Primary Modules

- `src/api/mod.rs`
- `src/api/response.rs`
- `src/api/routes_*.rs`
- `src/api/mcp.rs`
- `src/api/mcp_tools.rs`
- `src/api/mcp_registry.rs`

## Core Design

- Axum router composes all HTTP endpoints in one localhost-oriented API plane.
- Responses use a common envelope: `{ success, data, error, metadata }`.
- MCP is exposed as JSON-RPC 2.0 at `POST /mcp` and maps tool calls to existing engine/store operations.
- MCP argument validation is schema-backed in the tool registry/handler path.

## Invariants

- API and MCP are independently toggleable (`api_enabled`, `mcp_enabled`; `--no-api`, `--no-mcp`).
- HTTP bind is loopback by default (`127.0.0.1`) and is the primary trust boundary.
- REST and MCP share the same core runtime semantics; MCP does not bypass store/engine rules.

## Security Boundary Notes

- Localhost binding is relied on for process-level trust.
- External exposure is expected to be handled at a system edge (reverse proxy, firewall, tunnel).

## Related Usage Doc

- `docs/features/integration-api-and-mcp.md`
