# Agents

Instructions for AI coding agents working on this codebase.

## Build & Verify

```bash
cargo build --release    # both workspace members
cargo test               # all tests (unit + integration)
cargo clippy             # lint — fix all warnings before committing
```

## Workspace Structure

Two binaries in a Cargo workspace:

- `egregore` (default member) — node daemon in `src/`
- `egregore-relay` — relay server in `egregore-relay/`

Both share the `egregore` library crate (`src/lib.rs`).

## Key Conventions

- **Async + SQLite**: All rusqlite calls must use `tokio::task::spawn_blocking`. The store is sync; the runtime is async.
- **Content types**: Use `#[serde(tag = "type")]` for the `ContentType` enum.
- **API responses**: Wrap all HTTP responses in `{ success, data, error, metadata }`.
- **Pagination**: Node uses `limit`/`offset` query params. Relay uses `page`/`per_page`.
- **Crypto**: dalek crates only (ed25519-dalek, x25519-dalek, curve25519-dalek).
- **Content size**: 64 KB max per message, enforced in both `publish()` and `ingest()`.
- **Ingest ordering**: Size check, then duplicate check, then Ed25519 verification (cheap operations before expensive).
- **Error types**: Use `EgreError` variants from `src/error.rs`. Return `Result<T>` (alias for `std::result::Result<T, EgreError>`).

## Module Map

| Module | Purpose | Key files |
|--------|---------|-----------|
| `identity/` | Ed25519 keypair, signing, Argon2id encryption | `keys.rs`, `signing.rs`, `encryption.rs` |
| `crypto/` | SHS handshake, Box Stream, Private Box | `handshake.rs`, `box_stream.rs`, `private_box.rs` |
| `feed/` | Message lifecycle: publish, ingest, query, search | `engine.rs`, `models.rs`, `content_types.rs` |
| `feed/store/` | SQLite persistence, FTS5 search | `mod.rs`, `messages.rs`, `peers.rs` |
| `gossip/` | Encrypted TCP replication, LAN discovery | `client.rs`, `server.rs`, `replication.rs`, `discovery.rs` |
| `api/` | Axum HTTP routes + embedded MCP server | `routes_*.rs`, `mcp.rs`, `mcp_tools.rs` |

## Testing

- Unit tests are co-located in each module.
- Integration tests in `tests/` (gossip replication, security).
- No test database fixtures — tests create in-memory SQLite instances.

## Documentation

Refer to `docs/` for protocol specification, operational procedures, and OpenAPI specs. See `CLAUDE.md` for the full file index.

## MCP Integration

The node embeds an MCP server at `POST /mcp` on the HTTP API port (default 7654). Ten tools expose the full node API over JSON-RPC 2.0. See `docs/operations.md` section 10 for connection details.
