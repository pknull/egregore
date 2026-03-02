# Architecture Slice: Events and Hooks

## Scope

Covers push-style local integrations triggered by message activity.

## Primary Modules

- `src/api/routes_events.rs`
- `src/hooks.rs`
- `src/main.rs` (hook wiring at startup)

## Core Design

- SSE endpoint (`GET /v1/events`) streams message events to local clients.
- Hook runner supports subprocess and webhook delivery modes.
- Hook configuration supports retry/timeout and optional idempotency behavior.
- Event fan-out is local-node scoped (driven by publish/ingest on that node).

## Invariants

- Hooks are optional and can be fully disabled.
- Hook failures do not invalidate the underlying message ingest/publish result.
- Idempotent hook mode records delivery state to suppress duplicate side effects.

## Operational Notes

- Timeouts and retry limits bound hook execution pressure.
- Filtered SSE consumers reduce downstream client work without changing stored data.

## Related Usage Doc

- `docs/features/events-and-hooks.md`
