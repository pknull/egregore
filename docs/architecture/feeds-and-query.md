# Architecture Slice: Signed Feeds and Query

## Scope

Covers message model, publish/ingest integrity path, and query/search read paths.

## Primary Modules

- `src/feed/models.rs`
- `src/feed/engine.rs`
- `src/feed/store/messages.rs`
- `src/feed/store/mod.rs`

## Core Design

- Per-author append-only chain (`sequence`, `previous`, `hash`, `signature`).
- Publish path signs hash-linked messages with local identity.
- Ingest path verifies signature/hash and chain structure before insert.
- Query path supports feed reads, filtered reads, and FTS-based search.

## Invariants

- `sequence=1` has no `previous`; `sequence>1` has `previous`.
- Per-message content cap is 64 KB in publish and ingest paths.
- Duplicate and malformed chain data are rejected.
- Gap-tolerant ingest marks unresolved chain links via `chain_valid`.

## Storage Ownership

- `messages`, `feeds`, `message_tags`, `messages_fts`

## Related Usage Doc

- `docs/features/feeds-and-query.md`
