# Architecture Slice: Events

## Scope

Covers push-style local integrations triggered by message activity.

## Primary Modules

- `src/api/routes_events.rs`

## Core Design

- SSE endpoint (`GET /v1/events`) streams message events to local clients.
- Event fan-out is local-node scoped (driven by publish/ingest on that node).

## Invariants

- A slow or disconnected SSE client does not invalidate the underlying message ingest/publish result.

## Operational Notes

- Filtered SSE consumers reduce downstream client work without changing stored data.

## Related Usage Doc

- `docs/features/events.md`
