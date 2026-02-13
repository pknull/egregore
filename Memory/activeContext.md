---
version: "1.1"
lastUpdated: "2026-02-13 UTC"
lifecycle: "active"
stakeholder: "all"
changeTrigger: "Session save: full implementation complete + Asha init"
validatedBy: "user"
dependencies: ["communicationStyle.md"]
---

# activeContext

## Current Project Status

**Primary Focus**: Post-MVP refinement and integration

**Active Work**:

- All 6 implementation phases complete
- 42 tests passing, 0 clippy errors
- End-to-end verified: two daemons exchanging feeds over encrypted gossip
- Asha initialized, README created

**Recent Activities** (last 7 days):

- **2026-02-13**: Full implementation from scaffold through e2e verification (6725 lines, 34 files). Protocol fix for gossip deadlock (always send Want even if empty). Python adapter created and tested. README added. Asha initialized with populated Memory files.

## Key Learnings

- Gossip protocol deadlock: if one side has no wants and doesn't send Want, the other side's `handle_peer_wants` blocks on recv() forever. Fix: always send Want (even empty).
- HMAC trait ambiguity in Rust when `hmac` + `chacha20poly1305` are both in scope. Use `<HmacSha256 as Mac>::new_from_slice()`.
- Feed store PK collisions: test helper using `hash_{seq}` collides across authors. Use `hash_{author}_{seq}`.

## Critical Reference Information

- Daemon ports: HTTP 7654, Gossip 7655 (defaults)
- Network key default: "egregore-network-v1" (SHA-256 hashed to 32 bytes)
- Feed author format: `@<base64-ed25519-pubkey>.ed25519`
- API response format: `{ success, data, error, metadata }`
- Binary: `~/Code/egregore/target/release/egregore`

## Next Steps

**Immediate**:

- [ ] Add `POST /v1/peers/{address}/sync` for manual sync trigger
- [ ] Fix `GET /v1/feed/{author}` URL encoding issue with base64 `=` chars

**Deferred**:

- LAN multicast peer discovery
- Feed pruning/compaction
- Rate limiting
- Web UI
