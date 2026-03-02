# Architecture Slice: Schema Registry

## Scope

Provides message contract validation and schema lifecycle handling.

## Primary Modules

- `src/feed/schema.rs`
- `src/feed/engine.rs` (publish/ingest validation hooks)
- `src/api/routes_schema.rs`

## Core Design

- Schemas are registered by ID (typically `<content_type>/v<version>`).
- Built-ins are available for core content types.
- Validation runs in publish and ingest paths.
- Strict mode can reject unknown schemas/content types.

## Invariants

- Strict mode is configurable (`schema_strict`).
- Schema validation happens before expensive ingest operations where possible.
- Validation failures reject message acceptance.

## Related Usage Doc

- `docs/features/schema-registry.md`
