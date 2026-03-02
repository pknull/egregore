# Architecture Slice: Selective Replication Policy

## Scope

Controls ingress replication scope using follows (authors) and topics (tags).

## Primary Modules

- `src/feed/store/mod.rs` (policy tables)
- `src/api/routes_follows.rs`
- `src/api/routes_topics.rs`
- `src/gossip/client.rs`
- `src/gossip/replication.rs`

## Core Design

- `follows` controls author feed admission.
- `topics` controls tag-based message admission.
- Policies are applied during sync request construction and serve-side filtering.

## Invariants

- Empty follows means all authors allowed.
- Empty topic subscriptions means no topic filtering.
- Tightening policy does not delete already stored messages.

## Storage Ownership

- `follows`, `topic_subscriptions`

## Related Usage Doc

- `docs/features/selective-replication.md`
