---
version: "1.2"
lastUpdated: "2026-02-13 UTC"
lifecycle: "active"
stakeholder: "all"
changeTrigger: "Session save: security test suite + distributable v1 features"
validatedBy: "user"
dependencies: ["communicationStyle.md"]
---

# activeContext

## Current Project Status

**Primary Focus**: Distributable v1 hardening — security coverage, relay, MCP

**Active Work**:

- 63 tests passing (52 unit + 3 gossip integration + 8 security), 0 clippy errors
- Security test suite covering 8 adversarial scenarios over-the-wire
- Distributable v1 phases A-C implemented (follows, content types, relay, MCP scaffold)
- `.gitignore` updated for `node_modules/` and `dist/`

**Recent Activities** (last 7 days):

- **2026-02-13 (session 2)**: Implemented 8-test security suite (`tests/security_test.rs`) covering: network key isolation, AuthorizeFn enforcement, forged signature rejection, tampered content/hash rejection, duplicate message idempotency, malformed gossip handling, empty state safety. Code-reviewed with parallel reviewers. Updated `.gitignore` for mcp/ build artifacts.
- **2026-02-13 (session 1)**: Full implementation from scaffold through e2e verification (6725 lines, 34 files). Protocol fix for gossip deadlock (always send Want even if empty). Python adapter created and tested. README added. Asha initialized with populated Memory files. Distributable v1 phases A-C: follows system, content types, egregore-relay workspace member, MCP server scaffold.

## Key Learnings

- Gossip protocol deadlock: if one side has no wants and doesn't send Want, the other side's `handle_peer_wants` blocks on recv() forever. Fix: always send Want (even empty).
- HMAC trait ambiguity in Rust when `hmac` + `chacha20poly1305` are both in scope. Use `<HmacSha256 as Mac>::new_from_slice()`.
- Feed store PK collisions: test helper using `hash_{seq}` collides across authors. Use `hash_{author}_{seq}`.
- Security test pattern: `serve_malicious_messages` helper implements a gossip server that follows protocol structure but injects crafted messages. Tests 3-5 use this to validate ingest() rejection paths over the wire.
- `ingest()` errors in `receive_messages()` are logged but not propagated (design choice). Security tests verify end-state (empty store) rather than error type, which is sound because if ingest() were broken, store would contain messages and assertions would fail.
- `ingest()` does NOT validate `previous` hash chain linkage or catch sequence gaps — documented gaps for future hardening.

## Critical Reference Information

- Daemon ports: HTTP 7654, Gossip 7655 (defaults)
- Network key default: "egregore-network-v1" (SHA-256 hashed to 32 bytes)
- Feed author format: `@<base64-ed25519-pubkey>.ed25519`
- API response format: `{ success, data, error, metadata }`
- Binary: `~/Code/egregore/target/release/egregore`
- Test commands: `cargo test --workspace`, `cargo test security_test -- --nocapture`

## Next Steps

**Immediate**:

- [ ] Add `POST /v1/peers/{address}/sync` for manual sync trigger
- [ ] Fix `GET /v1/feed/{author}` URL encoding issue with base64 `=` chars
- [ ] Validate `previous` hash chain in `ingest()` (no fork attacks)
- [ ] Handle sequence gap attacks in `ingest()` (prevent feed bricking)

**Deferred**:

- LAN multicast peer discovery
- Feed pruning/compaction
- Rate limiting
- Web UI
