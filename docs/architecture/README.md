# Architecture Slices

This directory contains architecture docs split by feature area.

Use this when you want internals (design boundaries, module ownership, invariants).
For usage and operator workflows, use [`docs/features/README.md`](../features/README.md).

## Core Runtime

- [system-baseline.md](./system-baseline.md)

## Feature-Aligned Slices

| Feature | Architecture Slice | Usage/Config Guide |
|---|---|---|
| Identity and Network Trust | [identity-and-security.md](./identity-and-security.md) | [`docs/features/identity-and-security.md`](../features/identity-and-security.md) |
| Signed Feeds and Query | [feeds-and-query.md](./feeds-and-query.md) | [`docs/features/feeds-and-query.md`](../features/feeds-and-query.md) |
| Mesh Replication | [mesh-replication.md](./mesh-replication.md) | [`docs/features/mesh-replication.md`](../features/mesh-replication.md) |
| Peer Discovery | [peer-discovery.md](./peer-discovery.md) | [`docs/features/peer-discovery.md`](../features/peer-discovery.md) |
| Selective Replication | [selective-replication.md](./selective-replication.md) | [`docs/features/selective-replication.md`](../features/selective-replication.md) |
| Consumer Groups | [consumer-groups.md](./consumer-groups.md) | [`docs/features/consumer-groups.md`](../features/consumer-groups.md) |
| Schema Registry | [schema-registry.md](./schema-registry.md) | [`docs/features/schema-registry.md`](../features/schema-registry.md) |
| Retention and Lifecycle | [retention-and-lifecycle.md](./retention-and-lifecycle.md) | [`docs/features/retention-and-lifecycle.md`](../features/retention-and-lifecycle.md) |
| Mesh Health | [mesh-health.md](./mesh-health.md) | [`docs/features/mesh-health.md`](../features/mesh-health.md) |
| REST API and MCP | [integration-api-and-mcp.md](./integration-api-and-mcp.md) | [`docs/features/integration-api-and-mcp.md`](../features/integration-api-and-mcp.md) |
| Events and Hooks | [events-and-hooks.md](./events-and-hooks.md) | [`docs/features/events-and-hooks.md`](../features/events-and-hooks.md) |
| Private Box Utility | [private-box.md](./private-box.md) | [`docs/features/private-box.md`](../features/private-box.md) |

## Shared Maps

- [api-surface-map.md](./api-surface-map.md)
- [storage-ownership.md](./storage-ownership.md)
- [module-capability-index.md](./module-capability-index.md)
