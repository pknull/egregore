# Architecture Slice: Mesh Health Observability

## Scope

Provides direct and transitive peer health visibility across the mesh.

## Primary Modules

- `src/gossip/health.rs`
- `src/feed/store/health.rs`
- `src/gossip/replication.rs` (peer observations in gossip messages)
- `src/api/routes_mesh.rs`

## Core Design

- Local node records direct peer observations.
- Observations are exchanged transitively during replication.
- Merge logic uses generation/timestamp/source preference.
- Aggregated view is exposed via `/v1/mesh`.

## Invariants

- Generation increments help avoid stale overwrite after restarts.
- Observation acceptance is bounded by age/count safeguards.
- This slice is observability-only; it does not alter replication policy.

## Storage Ownership

- `peer_health`, `local_state`

## Related Usage Doc

- `docs/features/mesh-health.md`
