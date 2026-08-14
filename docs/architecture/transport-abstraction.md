# Architecture Slice: Transport Abstraction

## Scope

Covers the wire-neutral transport boundary, the gossip and NATS JetStream adapters,
and the composite behavior that emerges when two or more transports are attached.
Signing, validation, storage, and API behavior remain above it in `FeedEngine`.

## Primary Modules

- `src/transport/trait_def.rs`
- `src/transport/gossip.rs`
- `src/transport/bus/`
- `src/transport/composite/`
- `src/transport/health.rs`

## Trait Boundary

`Transport` is a narrow async trait between `FeedEngine` and a delivery wire.
It has four semantic operations:

- `publish` accepts an already-signed `Message` for eventual delivery.
- `subscribe` returns a cancellable handle and a live filtered stream.
- `request_from` returns a forward-open, per-author backfill stream.
- Lifecycle and health group `start`, `shutdown(deadline)`, and `health`.

RFC 0002 §18 ratifies two defaulted bridge-only hooks without changing the core four-operation model:

- `ack_after_publish` lets a composite finalize a source-side broker ack after
  every destination settles; transports without broker acks use the default no-op.
- `self_echo_total` exposes bus self-echo suppression to composite health;
  transports without a self-echo path use the default zero.

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

### BusTransport

`BusTransport` maps the same trait onto NATS JetStream. Publish first enqueues
the message in SQLite `pending_forwarding`, then attempts the broker publish;
the durable outbox depth is reported as `unreplicated_count` and survives a
failed PubAck path.

The subject is `egregore.feed.{hash}`, where `hash` is the first 16 bytes of
SHA-256 over the decoded Ed25519 public key, rendered as lowercase hex. This
keeps author subjects bounded and excludes NATS-special characters.

On subscribe, a bus message must pass `FeedEngine::ingest` and become durable
locally before it is yielded to bridge ingress. Newly ingested messages retain
their broker ack handle; duplicates are acked and counted as self-echoes rather
than forwarded again.

PubAck processing writes `(author, author_sequence, stream_sequence)` into
`bus_author_seq_index`. `request_from` uses that index to locate the first
JetStream sequence after the requested author sequence, then opens an ordered,
ephemeral consumer for that author's hashed subject.

Startup revalidates live JetStream ordering assumptions. It refuses
`WorkQueue` stream retention and consumers with a `deliver_group`, as required
by RFC 0001 §9.2, because either can split an author's feed across competing
consumers.

## CompositeTransport

Bridge mode is emergent: attaching two or more child transports creates a
`CompositeTransport`; there is no separate bridge backend configuration.

- Local publish fans out concurrently to every child and records
  `egregore_transport_composite_forward_latency_seconds` per child backend.
- Subscribe merges every child stream behind one cancellation handle.
- `request_from` tries children in configured order and falls back only when a
  child returns an error; an empty stream is indistinguishable from feed head.
- Cross-transport ingress uses bounded per-`(source, destination, author)` FIFO
  queues, with high/low watermarks so one slow author path does not starve all
  other authors.
- When a destination queue fills, ingress waits for egress to drain it. A
  bus-sourced message remains unacked during that wait, coupling JetStream
  delivery pressure to the slowest bridge destination.
- Each source message has a one-shot ack barrier. For two or more destinations,
  an atomic acquire-release decrement ensures exactly one resolver performs
  the source `ack_after_publish` call after every destination settles.
- Health aggregates child connectivity, timestamps, backlog, in-flight counts,
  and errors; each child receives `bridge_queues` describing traffic destined
  for that child, while the top-level composite exposes `children`.

## Observability

`GET /v1/status` includes an optional `transport` object. Leaf deployments
report one `TransportHealth`; composites add recursive `children`, each with
optional `BridgeQueuesHealth` for queue depth, backpressure, age, redelivery,
self-echo, ack-on-error, and direction-specific error state.

Prometheus exposes the `egregore_bridge_*` families for queue depth, active and
backpressured authors, queue and publish age, backpressure, ack-on-error, NATS
redelivery, and self-echo. The `egregore_transport_composite_*` families report
child count, per-child forward latency, and dedup drops.

## Normative and Operational References

- [RFC 0001: Transport Abstraction](../../../docs/rfcs/0001-transport-abstraction.md) and [RFC 0002: Transport Bridge Mode](../../../docs/rfcs/0002-transport-bridge-mode.md), including the governing §18 amendments
- [Specification §5A: Transport Abstraction](../../../docs/SPECIFICATION.md#5a-transport-abstraction)
- Deployment guides: [bus](../../../docs/deployment/bus.md), [bridge](../../../docs/deployment/bridge.md), and [gossip/bus migration](../../../docs/deployment/migration-gossip-bus.md)
