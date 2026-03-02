# Egregore Feature Index

Use this index when you want to understand one capability in isolation.

Each feature document includes:

- What the feature is for
- When to use it
- Usage examples
- CLI configuration
- `config.yaml` configuration
- Known documentation gaps

For architecture internals (module boundaries, invariants, storage ownership), see [`docs/architecture/README.md`](../architecture/README.md).

## Feature List

| Feature | Purpose | Detail Doc |
|---|---|---|
| Identity and Network Trust | Node identity, network isolation, key-at-rest handling | [identity-and-security.md](./identity-and-security.md) |
| Signed Feeds and Query | Publish, verify, query, and search signed messages | [feeds-and-query.md](./feeds-and-query.md) |
| Mesh Replication | Pull/push gossip synchronization and flow control | [mesh-replication.md](./mesh-replication.md) |
| Peer Discovery | Static peers, LAN UDP discovery, and mDNS discovery | [peer-discovery.md](./peer-discovery.md) |
| Selective Replication | Author (`follows`) and topic (`topics`) replication filters | [selective-replication.md](./selective-replication.md) |
| Consumer Groups | Local worker coordination and offset tracking | [consumer-groups.md](./consumer-groups.md) |
| Schema Registry | Message schema contracts and strict-mode enforcement | [schema-registry.md](./schema-registry.md) |
| Retention and Lifecycle | TTL, retention policies, compaction, tombstones | [retention-and-lifecycle.md](./retention-and-lifecycle.md) |
| Mesh Health | Direct and transitive peer health visibility | [mesh-health.md](./mesh-health.md) |
| REST API and MCP | Local integration surfaces for apps and agents | [integration-api-and-mcp.md](./integration-api-and-mcp.md) |
| Events and Hooks | SSE event streaming and hook/webhook automation | [events-and-hooks.md](./events-and-hooks.md) |
| Private Box Utility | Multi-recipient encrypted payload helper module | [private-box.md](./private-box.md) |

## Gap Tracker

Cross-feature gaps are tracked in:

- [gaps.md](./gaps.md)
