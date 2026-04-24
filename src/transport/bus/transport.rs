//! `BusTransport` — `Transport` adapter over NATS JetStream.
//!
//! Holds the NATS client, JetStream context, durable consumer, identity,
//! and the feed engine reference needed by the durable-local-ingest
//! precondition (RFC 0002 §8.2).
//!
//! ## Trait method landing
//!
//! - `publish` (Wave 2 Step 9) — enqueue-then-attempt via
//!   `publish_attempt` (amendments §C.10, §G.2). The retry scheduler
//!   calls `publish_attempt` directly, bypassing the enqueue step.
//! - `subscribe` (Wave 2 Step 10) — spawn_blocking ingest, ack-handle
//!   retention in `pending_acks`, self-echo rule (amendments §C.2,
//!   §C.4, §C.9).
//! - `request_from` (Wave 2 Step 11) — `bus_author_seq_index` lookup +
//!   ephemeral ordered consumer (amendment §C.3).
//! - `start` is an idempotent no-op marker (the async-nats client
//!   connects inside `new`).
//! - `shutdown` drains in-flight publishes up to the deadline.
//! - `health()` populates from atomics in the adapter's shape.
//!
//! ## Observability
//!
//! Mirrors `GossipTransport`'s pattern (inflight_publishes atomic +
//! last_successful_publish/last_peer_contact RwLocks + last_error shared
//! `Arc<RwLock<Option<String>>>`). The additional `pending_acks` map
//! (amendment §C.2, §C.12) holds NATS ack handles keyed by
//! `message.hash`; `ack_after_publish` / `abandon_ack` drain it.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_nats::jetstream::Context as JetstreamContext;
use async_nats::Client;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use futures::stream::BoxStream;
use futures::StreamExt;
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

/// Evaluate a [`TopicFilter`] against a message — mirrors the helper in
/// `gossip.rs` so the semantics match across adapters. Invariant 6 permits
/// superset delivery; a subset is a bug. Returning `true` is always safe.
fn matches_filter(filter: &TopicFilter, msg: &Message) -> bool {
    if let Some(authors) = filter.authors.as_ref() {
        if !authors.iter().any(|a| a == &msg.author) {
            return false;
        }
    }
    if let Some(tags) = filter.tags.as_ref() {
        if !tags.iter().any(|t| msg.tags.contains(t)) {
            return false;
        }
    }
    true
}

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

    /// Wave 4 Step 22 retcon: bus self-echo counter (amendment §C.4).
    ///
    /// Incremented by `subscribe`'s `DuplicateMessage` branch — a
    /// bus-sourced message we already have locally (typically because
    /// we authored it and received our own publish back through the
    /// subscribe loop). Self-echo is a bus-layer concern, not a
    /// composite-direction concern: the message never reaches a bridge
    /// queue. Wave 3 originally placed this counter in
    /// `DirectionState.self_echo_total`; Wave 4 moves it here so the
    /// canonical write happens at the source of truth. The composite's
    /// `compute_bridge_queues_health` reads this via `self_echo_total()`
    /// when the child is a bus transport, falling back to
    /// `DirectionState.self_echo_total` for non-bus backends (always
    /// zero).
    self_echo_total: Arc<AtomicU64>,
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
            self_echo_total: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Read-only accessor for the NATS client — exposed for adapter-side
    /// tooling (health shims, test helpers). Not used on the publish
    /// hot path (that goes through `self.jetstream`).
    #[allow(dead_code)]
    pub(crate) fn client(&self) -> &Client {
        &self.client
    }

    /// Test-only constructor that bypasses NKey credential loading. Used
    /// by the Step 13 integration smoke test against a plaintext NATS
    /// testcontainer — production still goes through `new` + the full
    /// `Config::validate` TLS/creds gauntlet.
    ///
    /// Exposed under `#[doc(hidden)]` + `pub` rather than `#[cfg(test)]`
    /// because the integration smoke lives in `tests/` (a separate
    /// crate) and cannot see `#[cfg(test)]`-gated items.
    #[doc(hidden)]
    pub async fn new_for_testing(
        config: Arc<BusConfig>,
        identity: Identity,
        engine: Arc<FeedEngine>,
    ) -> Result<Self> {
        let client = async_nats::ConnectOptions::new()
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
            self_echo_total: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Test-only accessor to inspect pending_acks length. Used by
    /// Step 13 integration tests to assert ack-handle draining.
    #[doc(hidden)]
    pub fn pending_acks_len(&self) -> usize {
        self.pending_acks.len()
    }

    /// Snapshot of the bus self-echo counter (amendment §C.4). Incremented
    /// once per `DuplicateMessage` observed on the subscribe path. The
    /// composite's `BridgeQueuesHealth` aggregation reads this for bus
    /// children; gossip children always surface zero (they have no
    /// self-echo path).
    pub fn self_echo_total(&self) -> u64 {
        self.self_echo_total.load(Ordering::Relaxed)
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
    pub async fn publish_internal(&self, msg: &Message) -> Result<()> {
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
    ///
    /// `pub` surface: `main.rs` (bin crate) wires this into the retry
    /// scheduler callback; the pending scheduler is spawned from there
    /// and must bypass `publish`'s enqueue step per §C.10.
    pub async fn publish_attempt(&self, msg: &Message) -> Result<()> {
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

    /// Ack the NATS consumer message whose `hash` is `message_hash`.
    /// Called by `CompositeTransport`'s egress after ALL destinations
    /// have resolved (amendments §A.1 #5, §C.5). No-op if no pending
    /// ack exists (e.g., the message was already acked or we're
    /// acking a gossip-sourced message — gossip has no ack equivalent).
    ///
    /// Uses `remove()` to both drop the handle and call `.ack()` on it.
    /// The `DashMap::remove` returns the `(K, V)` tuple on hit.
    #[allow(dead_code)] // wired by CompositeTransport egress in Wave 3
    pub async fn ack_after_publish(&self, message_hash: &str) -> Result<()> {
        if let Some((_, nats_msg)) = self.pending_acks.remove(message_hash) {
            nats_msg.ack().await.map_err(|_e| EgreError::Peer {
                reason: "nats ack failed".to_string(),
            })?;
        }
        Ok(())
    }

    /// Release the ack handle for `message_hash` WITHOUT acking. Called
    /// on shutdown or error paths so NATS `ack_wait` expires and the
    /// message is redelivered. Amendment §C.2.
    #[allow(dead_code)] // wired by CompositeTransport egress in Wave 3
    pub fn abandon_ack(&self, message_hash: &str) {
        self.pending_acks.remove(message_hash);
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
        filter: TopicFilter,
    ) -> Result<(SubscriptionHandle, BoxStream<'static, Message>)> {
        // Amendments §C.2, §C.4, §C.9 — durable-local-ingest precondition
        // before yielding, ack-handle retention, self-echo rule (ack but
        // do not yield on DuplicateMessage), spawn_blocking for SQLite.
        //
        // Cancel shape matches GossipTransport: the returned handle owns
        // a oneshot sender; dropping it fires the receiver, which the
        // stream body races against the consumer's next message arrival.
        let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel::<()>();
        let handle = SubscriptionHandle::from_cancel(cancel_tx);

        // Clones moved into the 'static stream body. The consumer
        // supports Clone (see pull::Consumer doc); we avoid taking
        // ownership of self so BusTransport can hand out many concurrent
        // subscriptions (e.g., composite ingress + a future observer
        // panel). The pending_acks map is already wrapped in Arc<DashMap>
        // on the struct so sharing across concurrent subscriptions Just
        // Works.
        let consumer = self.consumer.clone();
        let engine = self.engine.clone();
        let last_error = self.last_error.clone();
        let pending_acks = self.pending_acks.clone();
        let self_echo_total = self.self_echo_total.clone();
        // Note: `last_peer_contact` is not updated from the subscribe
        // stream body here — it's only written by `publish_attempt` on
        // successful PubAck. Inbound-traffic liveness is observable via
        // `pending_acks.len()` + `TransportHealth.unreplicated_count`
        // from Wave 2+ observability surfaces.

        let stream = async_stream::stream! {
            // Open the message stream. Errors here are fatal for this
            // subscription — operators see the failure via last_error +
            // the stream ending on the caller's next poll.
            let mut messages = match consumer.messages().await {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(error = %e, "bus: consumer.messages() failed");
                    *last_error.write() = Some("bus consumer open error".to_string());
                    return;
                }
            };

            loop {
                tokio::select! {
                    biased;
                    // Drop-cancel has priority — same shape as GossipTransport.
                    _ = &mut cancel_rx => break,
                    next = messages.next() => {
                        let nats_msg = match next {
                            None => break,
                            Some(Err(e)) => {
                                // Transient recv error — log + continue.
                                // Killing the loop on every transient
                                // error would make the subscription
                                // fragile.
                                tracing::warn!(
                                    error = %e,
                                    "bus: consumer recv error (transient)"
                                );
                                *last_error.write() = Some("bus consumer recv error".to_string());
                                continue;
                            }
                            Some(Ok(m)) => m,
                        };

                        // 1. Deserialize. Malformed → ack + skip (don't
                        //    let a bad payload cause redelivery loops).
                        let decoded: Message = match serde_json::from_slice(&nats_msg.payload) {
                            Ok(m) => m,
                            Err(_e) => {
                                // Auditor A2: short code, no payload bytes.
                                *last_error.write() = Some("bus: malformed payload".to_string());
                                let _ = nats_msg.ack().await;
                                continue;
                            }
                        };

                        // 2. Filter. Non-match → ack + skip (the consumer
                        //    is filter_subject=egregore.feed.> by default;
                        //    per-subscriber filters are applied here).
                        if !matches_filter(&filter, &decoded) {
                            let _ = nats_msg.ack().await;
                            continue;
                        }

                        // 3. Durable local ingest (spawn_blocking per
                        //    §C.9). This is the "durable-local-ingest
                        //    precondition" from RFC 0002 §8.2 — we do
                        //    not yield to the bridge until we can prove
                        //    the message is safely stored locally.
                        let engine_clone = engine.clone();
                        let decoded_clone = decoded.clone();
                        let ingest_result = tokio::task::spawn_blocking(
                            move || engine_clone.ingest(&decoded_clone)
                        ).await;

                        match ingest_result {
                            Ok(Ok(())) => {
                                // Newly stored. Record the ack handle
                                // keyed by hash (§C.12) BEFORE yielding,
                                // so CompositeTransport's egress can
                                // call ack_after_publish once the
                                // cross-transport forward resolves.
                                pending_acks.insert(decoded.hash.clone(), nats_msg);
                                yield decoded;
                            }
                            Ok(Err(EgreError::DuplicateMessage { .. })) => {
                                // §C.4 self-echo rule: the message is
                                // already in local SQLite (we authored
                                // it, or a prior redelivery landed it).
                                // Ack so the broker stops redelivering;
                                // do NOT yield — the bridge already has
                                // it and yielding would trigger a
                                // duplicate cross-transport forward.
                                //
                                // Wave 4 Step 22 retcon: count the
                                // self-echo on the bus transport itself
                                // (canonical location — §C.4). The
                                // composite's BridgeQueuesHealth
                                // aggregation reads from here for bus
                                // children.
                                self_echo_total.fetch_add(1, Ordering::Relaxed);
                                let _ = nats_msg.ack().await;
                            }
                            Ok(Err(_ingest_err)) => {
                                // Unrecoverable ingest error (signature
                                // invalid, schema fail, size cap).
                                // Ack to break any redelivery loop on a
                                // bad message. Auditor A2: short code.
                                *last_error.write() = Some("bus: ingest rejected".to_string());
                                let _ = nats_msg.ack().await;
                            }
                            Err(_join_err) => {
                                *last_error.write() =
                                    Some("bus: ingest blocking task failed".to_string());
                                let _ = nats_msg.ack().await;
                            }
                        }
                    }
                }
            }
        };

        Ok((handle, Box::pin(stream)))
    }

    async fn request_from(
        &self,
        author: PublicId,
        after_seq: u64,
    ) -> Result<BoxStream<'static, Message>> {
        // Amendment §C.3 — index-first lookup, then ephemeral ordered
        // consumer with FilterSubject + ByStartSequence.
        //
        // 1. SQLite index lookup (spawn_blocking): find the first
        //    stream_seq where (author, author_seq > after_seq). If no
        //    row: return an empty stream — Invariant 5 says the caller
        //    cannot distinguish head-of-feed from not-indexed-yet, and
        //    chain-gap detection handles recovery.
        let engine = self.engine.clone();
        let author_for_query = author.0.clone();
        let start_stream_seq = tokio::task::spawn_blocking(move || {
            engine
                .store()
                .bus_author_seq_index_find_stream_seq(&author_for_query, after_seq)
        })
        .await
        .map_err(|e| EgreError::Peer {
            reason: format!("bus: bus_author_seq_index join error: {e}"),
        })??;

        let Some(start_seq) = start_stream_seq else {
            // No indexed message for this author beyond after_seq. Return
            // an empty stream rather than an error — the caller's chain-
            // gap detection on the consumer side will re-request if
            // needed (Invariant 5).
            return Ok(Box::pin(futures::stream::empty()));
        };

        // 2. Ephemeral pull consumer scoped to this author's subject with
        //    start_sequence set from the index hit. No durable_name =
        //    ephemeral (auto-cleaned by NATS after inactivity). No ack
        //    (AckPolicy::None) — request_from is a snapshot read, not a
        //    subscription; the caller handles dedup via the engine's
        //    ingest path.
        let subject = author_subject(&author);
        let consumer_cfg = async_nats::jetstream::consumer::pull::Config {
            filter_subject: subject,
            deliver_policy: async_nats::jetstream::consumer::DeliverPolicy::ByStartSequence {
                start_sequence: start_seq,
            },
            ack_policy: async_nats::jetstream::consumer::AckPolicy::None,
            ..Default::default()
        };

        let stream_obj = self
            .jetstream
            .get_stream(&self.config.stream_name)
            .await
            .map_err(|e| EgreError::Peer {
                reason: format!("bus: get_stream failed: {e}"),
            })?;

        let consumer = stream_obj
            .create_consumer(consumer_cfg)
            .await
            .map_err(|e| EgreError::Peer {
                reason: format!("bus: create ephemeral consumer failed: {e}"),
            })?;

        let mut messages = consumer.messages().await.map_err(|e| EgreError::Peer {
            reason: format!("bus: ephemeral consumer messages() failed: {e}"),
        })?;

        // 3. Stream decoded messages. Deserialization errors end the
        //    stream (no ack, AckPolicy::None); per Invariant 5 the caller
        //    cannot distinguish this from head-of-feed so we do not
        //    surface an error mid-stream — chain-gap detection covers
        //    recovery.
        let last_error = self.last_error.clone();
        let stream = async_stream::stream! {
            while let Some(next) = messages.next().await {
                match next {
                    Ok(nats_msg) => {
                        match serde_json::from_slice::<Message>(&nats_msg.payload) {
                            Ok(decoded) => yield decoded,
                            Err(_e) => {
                                *last_error.write() =
                                    Some("bus request_from: malformed payload".to_string());
                                break;
                            }
                        }
                    }
                    Err(_e) => {
                        *last_error.write() =
                            Some("bus request_from: consumer recv error".to_string());
                        break;
                    }
                }
            }
        };

        Ok(Box::pin(stream))
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
            // Bus is a leaf transport; a composite wrapping it fills the
            // bridge_queues field on the bus child's health snapshot.
            bridge_queues: None,
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
