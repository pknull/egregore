//! Transport trait — see RFC 0001 §5.1 and §6.
//!
//! Narrow abstraction between `FeedEngine` and the wire. Any implementation
//! MUST uphold all seven invariants in RFC 0001 §6. The trait is
//! intentionally minimal: four wire methods (`publish`, `subscribe`,
//! `request_from`) plus lifecycle (`start`, `shutdown`, `health`).

use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::error::Result;
use crate::feed::models::Message;
use crate::identity::PublicId;

use super::filter::TopicFilter;
use super::health::TransportHealth;
use super::subscription::SubscriptionHandle;

/// Narrow abstraction between `FeedEngine` and the wire.
///
/// Any implementation MUST uphold all seven invariants in RFC 0001 §6:
///
/// - **Invariant 1 — Per-author FIFO.** Messages from a single author reach
///   a subscriber in strictly increasing sequence order.
/// - **Invariant 2 — No silent drop.** `publish` accepts for eventual
///   delivery; transport-level unavailability surfaces via `TransportHealth`,
///   not through the `publish` return. Messages are never silently dropped.
/// - **Invariant 3 — At-least-once.** Duplicates are allowed; consumers
///   deduplicate by hash. `publish` is idempotent with respect to
///   `(author, sequence)`.
/// - **Invariant 4 — Envelope preservation.** Implementations MUST NOT mutate
///   the signed `Message` bytes. Signatures remain verifiable end-to-end.
/// - **Invariant 5 — Gap detectability.** Stream end is not a distinguishable
///   error. Consumers detect gaps via the per-author `previous` hash chain.
/// - **Invariant 6 — Filter honesty.** `subscribe` MAY deliver a superset of
///   the filter's match set (extras are allowed); delivering a subset is a
///   bug. Canonical filtering is the consumer's responsibility.
/// - **Invariant 7 — Shutdown drains.** `shutdown(deadline)` flushes
///   in-flight publishes or surfaces them as explicit errors; fire-and-forget
///   is forbidden.
#[async_trait]
pub trait Transport: Send + Sync + 'static {
    /// Broadcast an already-signed message to peers/subscribers.
    ///
    /// MUST NOT mutate the message (Invariant 4). MUST be idempotent w.r.t.
    /// `(author, sequence)` (Invariant 3). Returns `Ok(())` when the message
    /// has been accepted for eventual delivery (Invariant 2); transport-level
    /// unavailability surfaces through `TransportHealth`, not through this
    /// return.
    async fn publish(&self, msg: &Message) -> Result<()>;

    /// Live stream of inbound messages matching `filter`.
    ///
    /// Yields messages as they arrive; per-author ordering MUST be preserved
    /// (Invariant 1). Dropping the returned `SubscriptionHandle` cancels the
    /// subscription; the accompanying `BoxStream` then ends on its next poll.
    ///
    /// Filter honesty (Invariant 6): the delivered set may be a superset of
    /// the filter's match set, but MUST NOT be a subset.
    async fn subscribe(
        &self,
        filter: TopicFilter,
    ) -> Result<(SubscriptionHandle, BoxStream<'static, Message>)>;

    /// Live-forward stream of messages for `author` with sequence > `after_seq`.
    ///
    /// Yields in strictly increasing sequence; caller closes when caught up.
    /// The stream MAY end due to transport disconnection, broker replay-window
    /// expiration, or head-of-feed — caller CANNOT distinguish (Invariant 5).
    /// Gaps are detected after the fact via the chain's `previous` linkage;
    /// no in-stream error signaling is required.
    async fn request_from(
        &self,
        author: PublicId,
        after_seq: u64,
    ) -> Result<BoxStream<'static, Message>>;

    /// Start the transport. Idempotent — a second call while already started
    /// MUST return `Ok(())` without relaunching background tasks.
    async fn start(&self) -> Result<()>;

    /// Graceful shutdown. MUST flush in-flight publishes or surface them as
    /// errors (Invariant 7); MUST NOT fire-and-forget. If `deadline` elapses
    /// before the drain completes, the implementation MAY return an error
    /// describing what remained unflushed.
    async fn shutdown(&self, deadline: Duration) -> Result<()>;

    /// Coarse liveness signal for `/v1/status`. Per-peer detail is adapter-
    /// internal (e.g. `gossip::health`); this is the wire-agnostic summary.
    fn health(&self) -> TransportHealth;
}
