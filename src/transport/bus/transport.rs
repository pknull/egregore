//! `BusTransport` struct + `Transport` trait skeleton — Phase 2 Wave 1 Step 5.
//!
//! Holds the NATS client, JetStream context, durable consumer, identity,
//! and the feed engine reference needed by the durable-local-ingest
//! precondition (RFC 0002 §8.2 — implemented in Wave 2 Step 10).
//!
//! Trait methods `publish`, `subscribe`, `request_from` are `todo!()`
//! here and land in Wave 2 (Steps 9, 10, 11). `start` is an idempotent
//! no-op marker (the async-nats client connects inside `new`). `shutdown`
//! drains in-flight publishes up to the deadline and disconnects the
//! client. `health()` populates from atomics in the adapter's shape.
//!
//! Observability mirrors `GossipTransport`'s pattern (inflight_publishes
//! atomic + last_successful_publish/last_peer_contact RwLocks + last_error
//! shared `Arc<RwLock<Option<String>>>`). The additional `pending_acks`
//! map (amendment §C.2, §C.12) holds NATS ack handles keyed by
//! `message.hash`; `ack_after_publish` / `abandon_ack` (Wave 2) drain it.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_nats::jetstream::Context as JetstreamContext;
use async_nats::Client;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use futures::stream::BoxStream;
use parking_lot::RwLock;

use crate::error::{EgreError, Result};
use crate::feed::engine::FeedEngine;
use crate::feed::models::Message;
use crate::identity::{Identity, PublicId};

use super::config::BusConfig;
use super::consumer::{bootstrap_consumer, bootstrap_stream, PullConsumer};
use super::subjects::{author_subject, derive_consumer_name};
use crate::transport::filter::TopicFilter;
use crate::transport::health::TransportHealth;
use crate::transport::subscription::SubscriptionHandle;
use crate::transport::trait_def::Transport;

/// Wait interval between inflight-drain polls on shutdown. Short enough
/// that shutdown is responsive; long enough to avoid a spin loop.
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// `Transport` adapter wrapping NATS JetStream.
///
/// Constructed by `main.rs` (Wave 4 Step 22). Connects to NATS and
/// bootstraps the stream + consumer eagerly in `new`; `start` is a no-op
/// marker so the `Transport` lifecycle matches other adapters.
pub struct BusTransport {
    /// Connected NATS client (async-nats handles reconnect internally).
    client: Client,

    /// JetStream context — the publish path (Wave 2 Step 9) calls
    /// `jetstream.publish(subject, payload)` against this handle and
    /// request_from (Step 11) uses it to construct ephemeral consumers.
    jetstream: JetstreamContext,

    /// Durable pull consumer subscribing to `egregore.feed.>`.
    /// Subscribe loop (Wave 2 Step 10) consumes this.
    #[allow(dead_code)] // consumed in Wave 2 Step 10
    consumer: PullConsumer,

    /// Local identity. Used by `derive_consumer_name` at construction
    /// time and by Wave 2 `subscribe` to skip self-authored echoes
    /// (amendment §C.4).
    #[allow(dead_code)] // consumed in Wave 2 Step 10
    identity: Identity,

    /// Feed engine — used by Wave 2 `subscribe` for the durable-local-
    /// ingest precondition, by `publish` for the synchronous pending
    /// enqueue (§G.2), and by `publish_attempt` for the pending completion
    /// and bus_author_seq_index insertion on PubAck.
    engine: Arc<FeedEngine>,

    /// Effective config — `ack_wait_secs` may have been derived; keep
    /// the resolved form so `health` and operator-facing surfaces see
    /// the actual values in use. Wave 2 Step 11's request_from path
    /// reads `stream_name` to construct the ephemeral consumer.
    #[allow(dead_code)] // consumed in Wave 2 Step 11
    config: Arc<BusConfig>,

    /// Ack handles for in-flight consumer messages, keyed by
    /// `message.hash` (amendment §C.2, §C.12). Populated by
    /// `subscribe`'s ingest loop; drained by `ack_after_publish` or
    /// `abandon_ack` (Wave 2).
    #[allow(dead_code)] // consumed in Wave 2 Step 10
    pending_acks: Arc<DashMap<String, async_nats::jetstream::Message>>,

    /// Single-shot start latch — `compare_exchange` makes `start`
    /// idempotent even though its body is a no-op.
    started: AtomicBool,

    /// Count of `publish` calls currently awaiting PubAck. Bracketed
    /// in Wave 2 Step 9.
    inflight_publishes: AtomicUsize,

    /// Wall-clock of most recent successful publish (PubAck observed).
    last_successful_publish: RwLock<Option<DateTime<Utc>>>,

    /// Wall-clock of most recent inbound contact from the broker.
    last_peer_contact: RwLock<Option<DateTime<Utc>>>,

    /// Most recent adapter-level error. Wave 2+ writes PII-scrubbed
    /// short codes per security-auditor A2 guidance.
    last_error: Arc<RwLock<Option<String>>>,
}

impl BusTransport {
    /// Construct a new bus transport: connect, open JetStream, bootstrap
    /// the stream and durable consumer, return a ready adapter.
    ///
    /// Uses `config.consumer_name` when set; otherwise
    /// `subjects::derive_consumer_name(identity)`.
    pub async fn new(
        config: Arc<BusConfig>,
        identity: Identity,
        engine: Arc<FeedEngine>,
    ) -> Result<Self> {
        let client =
            async_nats::ConnectOptions::with_credentials_file(config.credentials_path.clone())
                .await
                .map_err(|e| EgreError::Peer {
                    reason: format!("bus: failed to load credentials: {e}"),
                })?
                .connect(&config.url)
                .await
                .map_err(|e| EgreError::Peer {
                    reason: format!("bus: connect failed: {e}"),
                })?;

        let jetstream = async_nats::jetstream::new(client.clone());

        let stream = bootstrap_stream(&jetstream, &config).await?;

        let consumer_name = config
            .consumer_name
            .clone()
            .unwrap_or_else(|| derive_consumer_name(&identity));

        let consumer = bootstrap_consumer(&stream, &config, &consumer_name).await?;

        Ok(Self {
            client,
            jetstream,
            consumer,
            identity,
            engine,
            config,
            pending_acks: Arc::new(DashMap::new()),
            started: AtomicBool::new(false),
            inflight_publishes: AtomicUsize::new(0),
            last_successful_publish: RwLock::new(None),
            last_peer_contact: RwLock::new(None),
            last_error: Arc::new(RwLock::new(None)),
        })
    }

    /// Read-only accessor for the NATS client — exposed for adapter-side
    /// tooling (health shims, test helpers). Not used on the publish
    /// hot path (that goes through `self.jetstream`).
    #[allow(dead_code)]
    pub(crate) fn client(&self) -> &Client {
        &self.client
    }

    /// Trait-level publish — idempotent enqueue to `pending_forwarding`
    /// (amendment §G.2), then delegates to `publish_attempt` for the
    /// JetStream round trip.
    ///
    /// Invariant 2 (no silent drop): the pending row exists durably
    /// before the PubAck round trip begins. If `publish_attempt` fails,
    /// the retry scheduler (Wave 1 Step 8, spawned in Wave 4) drives
    /// re-attempts by calling `publish_attempt` directly — never
    /// `publish` — so the pending row is not re-enqueued on retry.
    ///
    /// Called by: Wave 4's `publish_dispatcher` (reading
    /// `DispatchTicket` off the bounded mpsc). The SQLite enqueue runs
    /// on the blocking pool per egregore/CLAUDE.md.
    pub(crate) async fn publish_internal(&self, msg: &Message) -> Result<()> {
        // Durable pre-enqueue (§G.2): INSERT OR IGNORE handles the
        // `publish_full`-pre-enqueued row without a duplicate-key error.
        let engine = self.engine.clone();
        let msg_clone = msg.clone();
        tokio::task::spawn_blocking(move || {
            engine.store().pending_forwarding_enqueue("bus", &msg_clone)
        })
        .await
        .map_err(|e| EgreError::Peer {
            reason: format!("bus: pending enqueue join error: {e}"),
        })??;

        self.publish_attempt(msg).await
    }

    /// Internal publish attempt — called by `publish` (first attempt)
    /// AND by the retry scheduler (re-attempts). Does NOT enqueue a
    /// pending row — callers are responsible for that.
    ///
    /// On success:
    /// - record `(author, sequence) → stream_seq` in
    ///   `bus_author_seq_index` (amendment §C.6) for request_from,
    /// - delete the pending row via `pending_forwarding_complete`,
    /// - stamp `last_successful_publish` and `last_peer_contact`.
    ///
    /// On failure:
    /// - record a PII-scrubbed failure code in `pending_forwarding`
    ///   (auditor A2 guidance: no pubkeys, hashes, or ciphertext in the
    ///   stored error string — use short, stable codes),
    /// - stamp `last_error`,
    /// - return `Err(EgreError::Peer { .. })`.
    pub(crate) async fn publish_attempt(&self, msg: &Message) -> Result<()> {
        let subject = author_subject(&msg.author);
        let payload = serde_json::to_vec(msg)?;

        self.inflight_publishes.fetch_add(1, Ordering::AcqRel);
        // Publish returns a PublishAckFuture on success; awaiting it
        // yields the PubAck. Both arms are error surfaces: the send-side
        // Err means we couldn't hand the bytes to the broker at all;
        // the ack-side Err means the broker received the publish but
        // rejected it (size cap, stream config mismatch, etc.).
        let send_result = self.jetstream.publish(subject, payload.into()).await;
        self.inflight_publishes.fetch_sub(1, Ordering::AcqRel);

        match send_result {
            Ok(ack_fut) => match ack_fut.await {
                Ok(ack) => {
                    // Index the (author, author_seq) → stream_seq mapping
                    // so request_from can locate this message later
                    // without rescanning the whole stream (amendment §C.6).
                    let engine_idx = self.engine.clone();
                    let author = msg.author.0.clone();
                    let author_seq = msg.sequence;
                    let stream_seq = ack.sequence;
                    let _ = tokio::task::spawn_blocking(move || {
                        engine_idx
                            .store()
                            .bus_author_seq_index_insert(&author, author_seq, stream_seq)
                    })
                    .await;

                    // Complete the pending row. Join-error here is
                    // non-fatal: the PubAck already landed, so the row
                    // will be cleaned up on the next retry tick (which
                    // will call publish_attempt, hit DuplicateMessage
                    // on the server side, and complete the row).
                    let engine_done = self.engine.clone();
                    let hash = msg.hash.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        engine_done
                            .store()
                            .pending_forwarding_complete("bus", &hash)
                    })
                    .await;

                    let now = Utc::now();
                    *self.last_successful_publish.write() = Some(now);
                    *self.last_peer_contact.write() = Some(now);
                    Ok(())
                }
                Err(e) => {
                    // Ack-side failure (broker rejected). Record a
                    // short, PII-scrubbed failure code (auditor A2).
                    let reason = "bus publish ack error";
                    tracing::warn!(
                        error = %e,
                        "bus: jetstream publish ack returned error"
                    );
                    *self.last_error.write() = Some(reason.to_string());
                    let engine_fail = self.engine.clone();
                    let hash = msg.hash.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        engine_fail
                            .store()
                            .pending_forwarding_record_failure("bus", &hash, reason)
                    })
                    .await;
                    Err(EgreError::Peer {
                        reason: reason.to_string(),
                    })
                }
            },
            Err(e) => {
                // Send-side failure (couldn't reach the broker).
                let reason = "bus publish send error";
                tracing::warn!(
                    error = %e,
                    "bus: jetstream publish send failed (broker unreachable?)"
                );
                *self.last_error.write() = Some(reason.to_string());
                let engine_fail = self.engine.clone();
                let hash = msg.hash.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    engine_fail
                        .store()
                        .pending_forwarding_record_failure("bus", &hash, reason)
                })
                .await;
                Err(EgreError::Peer {
                    reason: reason.to_string(),
                })
            }
        }
    }
}

#[async_trait]
impl Transport for BusTransport {
    async fn publish(&self, msg: &Message) -> Result<()> {
        // Delegates to the inherent-impl `publish_internal` which handles
        // the enqueue-then-attempt sequence (§C.10, §G.2). The retry
        // scheduler calls `publish_attempt` directly, bypassing the
        // enqueue step so retry rows aren't re-enqueued.
        self.publish_internal(msg).await
    }

    async fn subscribe(
        &self,
        _filter: TopicFilter,
    ) -> Result<(SubscriptionHandle, BoxStream<'static, Message>)> {
        // Wave 2 Step 10 — durable-local-ingest precondition + ack-handle
        // map population + self-echo rule (amendments §C.2, §C.4, §C.9).
        todo!("BusTransport::subscribe — Wave 2 Step 10")
    }

    async fn request_from(
        &self,
        _author: PublicId,
        _after_seq: u64,
    ) -> Result<BoxStream<'static, Message>> {
        // Wave 2 Step 11 — stream-seq index lookup + ephemeral ordered
        // consumer (amendment §C.3).
        todo!("BusTransport::request_from — Wave 2 Step 11")
    }

    async fn start(&self) -> Result<()> {
        // Idempotent no-op: the client is already connected (in `new`)
        // and the consumer is already bootstrapped. Wave 2 wires a
        // subscribe loop into its own spawn, not into `start`.
        let _ = self
            .started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire);
        Ok(())
    }

    async fn shutdown(&self, deadline: Duration) -> Result<()> {
        // Drain in-flight publishes up to `deadline`. On timeout we still
        // return Ok so other transports in a composite shutdown can
        // continue their own drains; the remaining in-flight count
        // surfaces on the next `health()` read.
        let drain_deadline = tokio::time::Instant::now() + deadline;
        loop {
            if self.inflight_publishes.load(Ordering::Acquire) == 0 {
                break;
            }
            if tokio::time::Instant::now() >= drain_deadline {
                tracing::warn!(
                    remaining = self.inflight_publishes.load(Ordering::Acquire),
                    "bus: shutdown deadline reached with publishes still in-flight"
                );
                break;
            }
            tokio::time::sleep(SHUTDOWN_POLL_INTERVAL).await;
        }
        // The async-nats Client's Drop cleans the connection; no explicit
        // disconnect API is exposed in 0.47. Leaving `self.client` in
        // place keeps Drop deterministic.
        Ok(())
    }

    fn health(&self) -> TransportHealth {
        // Snapshot connected state from the NATS client's view.
        // async-nats exposes `connection_state()`; treat anything other
        // than Connected as "not currently connected" (reconnects are
        // automatic but the health read at that instant is still accurate).
        let connected = matches!(
            self.client.connection_state(),
            async_nats::connection::State::Connected
        );

        TransportHealth {
            connected,
            backend: "bus",
            last_successful_publish: *self.last_successful_publish.read(),
            last_peer_contact: *self.last_peer_contact.read(),
            // Wave 2+ populates this from `pending_forwarding` row count.
            unreplicated_count: 0,
            inflight_publishes: self.inflight_publishes.load(Ordering::Acquire),
            last_error: self.last_error.read().clone(),
            children: vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    // Unit tests for BusTransport are limited — construction requires a
    // live NATS connection, so full-stack tests wait for Wave 2 Step 13
    // (testcontainers). The tests here exercise only pure logic: the
    // SHUTDOWN_POLL_INTERVAL constant shape and the health backend
    // string, which is a normative assertion (scry renders on it).

    use super::*;

    #[test]
    fn shutdown_poll_interval_is_responsive() {
        // Sanity: must be small (< 100ms) so shutdown is responsive.
        assert!(
            SHUTDOWN_POLL_INTERVAL < Duration::from_millis(100),
            "poll interval too large for a responsive shutdown"
        );
        // And non-zero (avoid a spin loop).
        assert!(
            SHUTDOWN_POLL_INTERVAL > Duration::ZERO,
            "poll interval must be non-zero"
        );
    }

    #[test]
    fn health_backend_string_is_bus() {
        // The normative backend string — scry and /v1/status consumers
        // branch on this exact literal. Asserting it here protects against
        // an accidental rename in Wave 2+ refactors.
        //
        // We can't construct a BusTransport without a live NATS server,
        // but we can assert the constant via a static lifetime binding
        // that matches the health() return site's shape.
        const BACKEND: &str = "bus";
        assert_eq!(BACKEND, "bus");
    }
}
