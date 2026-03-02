# Architecture Slice: Mesh Replication

## Scope

Covers pull sync protocol, optional persistent push mode, and flow-control primitives.

## Primary Modules

- `src/gossip/replication.rs`
- `src/gossip/client.rs`
- `src/gossip/server.rs`
- `src/gossip/connection.rs`
- `src/gossip/push.rs`
- `src/gossip/registry.rs`
- `src/gossip/persistent.rs`
- `src/gossip/flow_control.rs`

## Core Design

- Baseline pull protocol: `Have -> Want -> Messages -> Done`.
- Bidirectional sync per connection over SHS + Box Stream transport.
- Optional persistent negotiation upgrades connection for real-time `Push`.
- Flow-control layer adds credits/rate-limits/adaptive fanout to bound pressure.

## Invariants

- Pull sync remains the baseline convergence mechanism.
- Push is additive (`push_enabled`) and must fail back to pull safely.
- Connection and frame limits enforce bounded resource usage.

## Operational Behavior

- Sync loop merges peer sources and dials periodically.
- Server accepts concurrent peer connections with limits.
- Registry tracks active persistent writers keyed by peer identity.

## Related Usage Doc

- `docs/features/mesh-replication.md`
