# Architecture Slice: Transport Abstraction

## Scope

Covers the wire-neutral transport boundary and the gossip adapter behind it.
Signing, validation, storage, and API behavior remain above it in `FeedEngine`.

## Primary Modules

- `src/transport/trait_def.rs`
- `src/transport/gossip.rs`
- `src/transport/health.rs`

## Trait Boundary

`Transport` is a narrow async trait between `FeedEngine` and a delivery wire.
It has four semantic operations:

- `publish` accepts an already-signed `Message` for eventual delivery.
- `subscribe` returns a cancellable handle and a live filtered stream.
- `request_from` returns a forward-open, per-author backfill stream.
- Lifecycle and health group `start`, `shutdown(deadline)`, and `health`.

## Invariants

- **Per-author FIFO:** deliver one author's messages in strictly increasing sequence order; cross-author order is unspecified.
- **No silent drop:** accept for eventual delivery only with durable local state, and expose transport failure or backlog through health.
- **At-least-once:** duplicates are allowed and `FeedEngine::ingest` deduplicates them by message hash.
- **Envelope preservation:** forward the signed `Message` unchanged; never re-sign, mutate, or re-canonicalize it.
- **Gap detectability:** sequence and `previous` links expose missing messages even when a backfill stream ends without a distinct error.
- **Filter honesty:** `subscribe` may return a superset of its filter, but never a subset.
- **Shutdown drains:** flush in-flight publishes by the deadline or surface an explicit error; never fire and forget.

## Adapters

### GossipTransport

`GossipTransport` delegates delivery to the existing encrypted gossip stack:
registry fan-out for publish, the engine event channel for live subscription,
and paged local-store reads for `request_from`. Gossip remains pull-based for
convergence and does not maintain a durable transport outbox. Its
`unreplicated_count` is therefore `0` by definition; Have/Want reconciliation
discovers messages when peers reconnect.

`main.rs` attaches the gossip adapter when push replication is enabled and
hands it to the `publish_dispatcher` task, which forwards each locally
authored message from the engine's bounded dispatch channel.

## Engine Shape

`FeedEngine` holds `transports: Vec<Arc<dyn Transport>>`. With one transport,
`transport_health()` returns that adapter's health verbatim. With two or more,
it returns `TransportHealth::aggregate("composite", ..)`: `connected` is the
AND of the children, counts are summed, timestamps take the most recent value,
and the per-transport `children` are kept for inspection.

## Observability

`GET /v1/status` includes an optional `transport` object carrying the attached
transport's `TransportHealth`, and signed `node_status/v1` messages embed the
same shape as `transport_health`.

## Normative References

- [RFC 0001: Transport Abstraction](../../../docs/rfcs/0001-transport-abstraction.md), including the governing §18 and §19 amendments
- [Specification §5A: Transport Abstraction](../../../docs/SPECIFICATION.md#5a-transport-abstraction)
