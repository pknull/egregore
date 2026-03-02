# Module-to-Capability Index

Use this as a fast map from runtime capability to implementation files.

| Capability | Primary files |
|---|---|
| Identity and key handling | `src/identity/keys.rs`, `src/identity/signing.rs`, `src/identity/encryption.rs` |
| Handshake and encrypted transport | `src/crypto/handshake.rs`, `src/crypto/box_stream.rs`, `src/gossip/connection.rs` |
| Feed engine and models | `src/feed/engine.rs`, `src/feed/models.rs`, `src/feed/content_types.rs` |
| Store core | `src/feed/store/mod.rs`, `src/feed/store/messages.rs`, `src/feed/store/peers.rs` |
| Replication | `src/gossip/replication.rs`, `src/gossip/client.rs`, `src/gossip/server.rs` |
| Push and flow control | `src/gossip/push.rs`, `src/gossip/registry.rs`, `src/gossip/flow_control.rs`, `src/gossip/persistent.rs` |
| Discovery | `src/gossip/discovery.rs`, `src/gossip/mdns.rs` |
| Mesh health | `src/gossip/health.rs`, `src/feed/store/health.rs`, `src/api/routes_mesh.rs` |
| Schema governance | `src/feed/schema.rs`, `src/api/routes_schema.rs` |
| Consumer groups | `src/feed/store/groups.rs`, `src/api/routes_groups.rs` |
| Retention | `src/feed/store/retention.rs`, `src/api/routes_retention.rs` |
| API and integrations | `src/api/mod.rs`, `src/api/routes_*`, `src/api/mcp*.rs`, `src/hooks.rs` |
