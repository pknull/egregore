# Architecture Slice: Retention and Lifecycle

## Scope

Covers message expiration, retention policy enforcement, compaction, and tombstone cleanup.

## Primary Modules

- `src/feed/store/retention.rs`
- `src/api/routes_retention.rs`
- `src/feed/store/mod.rs`
- `src/main.rs` (scheduler wiring)

## Core Design

- Message-level expiry via `expires_at`.
- Policy-scoped cleanup (global/topic/author/content_type).
- Optional compaction by JSON-path key.
- Tombstones preserve deletion semantics for replication consistency.

## Invariants

- Retention task is optional (`retention_enabled`).
- Cleanup runs periodically and is executed through store routines.
- Tombstones are cleaned based on configured max age.

## Storage Ownership

- `retention_policies`, `tombstones`

## Related Usage Doc

- `docs/features/retention-and-lifecycle.md`
