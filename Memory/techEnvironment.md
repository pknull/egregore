---
version: "1.0"
lastUpdated: "2026-02-13"
---

# techEnvironment

## Platform

**OS**: Linux (Ubuntu)
**Working Directory**: `/home/pknull/Code/egregore/`

## Asha Framework

Tools are provided by the Asha plugin. Tool paths are injected via SessionStart hook.

### Available Commands

| Command | Purpose |
|---------|---------|
| `/asha:save` | Save session context, archive, refresh index, commit |
| `/asha:index` | Index files for semantic search |
| `/asha:init` | Initialize Asha in a new project |

### Tool Invocation

Tools are executed via the plugin's Python environment. Example patterns provided in session context.

**Semantic Search**: Query indexed files using memory_index.py
**Pattern Tracking**: Track and query patterns via reasoning_bank.py

## Project-Specific Stack

### Languages & Frameworks

- **Rust**: 2021 edition
- **Tokio**: Full-featured async runtime
- **Axum**: HTTP API framework (Tower-integrated)

### Dependencies

| Crate | Purpose |
|-------|---------|
| `ed25519-dalek` | Ed25519 signing/verification |
| `x25519-dalek` | Curve25519 DH key exchange |
| `chacha20poly1305` | AEAD encryption (Box Stream, Private Box) |
| `argon2` | KDF for private key encryption at rest |
| `sha2` | SHA-256 hashing (feed chain, network key) |
| `hmac` | HMAC-SHA256 (Secret Handshake) |
| `rusqlite` (bundled) | SQLite storage with FTS5 |
| `serde` + `serde_json` | Serialization |
| `clap` (derive) | CLI argument parsing |
| `tracing` | Structured logging |
| `chrono` | Timestamps (serde-compatible) |
| `base64` | Key encoding |
| `curve25519-dalek` | Ed25519→X25519 key conversion |

### Development Tools

- `cargo test` — 42 tests (unit + integration)
- `cargo clippy` — lint checking
- `cargo build --release` — release binary

## Verification

Commands run by `verify-app` agent:

| Command | Purpose |
|---------|---------|
| `cargo test` | Run all tests (42) |
| `cargo clippy` | Check for lint warnings |
| `cargo build` | Verify compilation |

## Key Conventions

- `spawn_blocking` for all rusqlite calls (sync lib in async runtime)
- `#[serde(tag = "type")]` for content type enum dispatch
- Standard API response: `{ success, data, error, metadata }`
- Network key isolation: SHA-256 of string key → 32-byte SHS cap
- Feed integrity: sequence chain + SHA-256 hash chain + Ed25519 signature
