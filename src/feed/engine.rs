use std::path::Path;
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use chrono::Utc;
use parking_lot::RwLock;
use tokio::sync::{broadcast, mpsc};

use crate::error::{EgreError, Result};
use crate::feed::models::{FeedQuery, Message, UnsignedMessage};
use crate::feed::private_box;
use crate::feed::schema::SchemaRegistry;
use crate::feed::store::FeedStore;
use crate::identity::{sign_bytes, verify_signature, Identity};
use crate::metrics;
use crate::transport::{Transport, TransportHealth};

/// Maximum serialized content size in bytes (64 KB).
const MAX_CONTENT_SIZE: usize = 64 * 1024;

/// Phase 2 Wave 4 amendment §C.8: ticket describing a locally-authored
/// publish that the `publish_dispatcher` task will hand to the effective
/// transport. The `pending_transports` list is populated by `publish_full`
/// when it synchronously enqueued rows under those transport ids before
/// sending this ticket — the dispatcher itself does not re-enqueue.
///
/// Keeping `Arc<Message>` matches the broadcast channel's payload shape
/// used elsewhere in the engine; the ticket is cheap to clone and the
/// dispatcher consumes it once.
#[derive(Clone)]
pub struct DispatchTicket {
    /// The locally-authored, signed message to dispatch.
    pub message: Arc<Message>,
    /// Transport ids (currently only `"bus"`) for which `publish_full`
    /// has already written a durable pending-forwarding row. The
    /// dispatcher calls `effective_transport.publish` which — for bus —
    /// will idempotently re-enqueue (INSERT OR IGNORE per amendment §G.2)
    /// and then attempt the JetStream round trip. If the dispatcher is
    /// saturated and the ticket was dropped, the pending rows cover
    /// recovery via the retry scheduler.
    pub pending_transports: Vec<&'static str>,
}

/// Feed engine: append with chain validation, read, verify, search.
///
/// Emits events via broadcast channel when messages are published or ingested.
/// Subscribe with `engine.subscribe()` to receive real-time notifications.
pub struct FeedEngine {
    store: FeedStore,
    event_tx: broadcast::Sender<Arc<Message>>,
    schema_registry: SchemaRegistry,
    /// Transports attached to this engine. Phase 1 stores but does not
    /// dispatch through this vec on the publish hot path (see plan §7.2);
    /// Phase 2's multi-transport fan-out consumer iterates it.
    ///
    /// Uses `parking_lot::RwLock` (non-poisoning, egregore convention) to
    /// permit attach-after-`Arc::new` construction: `GossipTransport` holds
    /// `Arc<FeedEngine>`, so the engine cannot take transports by value at
    /// construction time without a chicken-and-egg dep cycle (plan §11 OQ-1).
    /// Read paths are low-frequency (startup log, health snapshot); RwLock
    /// overhead is irrelevant.
    transports: RwLock<Vec<Arc<dyn Transport>>>,
    /// Phase 2 Wave 4 Step 21: bounded mpsc to the `publish_dispatcher` task.
    ///
    /// `publish_full` writes pending-forwarding rows synchronously BEFORE
    /// sending on this channel (amendment §C.8). On
    /// `TrySendError::Full` the call logs WARN and continues: durability
    /// is preserved via the pending row and the retry scheduler drains
    /// it on its own interval. On `TrySendError::Closed` (dispatcher
    /// task exited) the same recovery applies.
    ///
    /// Capacity 64 is tuned for typical publish rates — lower than
    /// `event_tx`'s 1024 broadcast capacity because (a) publishes are
    /// durably stored in `messages` + `pending_forwarding` before we
    /// touch this channel and (b) backpressure via `Full` is observable
    /// (WARN log + pending rows climbing) rather than silent.
    ///
    /// Wrapped in `RwLock<Option<_>>` for interior-mutability attachment
    /// after `Arc::new(engine)`: main.rs creates the channel, wires the
    /// sender via `set_dispatch_sender`, and owns the Receiver for the
    /// dispatcher task. When `None`, `publish_full` still runs the
    /// durable pre-enqueue but skips the ticket send (gossip-only
    /// deployments hit this path with no pending rows created).
    local_publish_tx: RwLock<Option<mpsc::Sender<DispatchTicket>>>,
}

impl FeedEngine {
    /// Create a new feed engine with default (non-strict) schema validation.
    pub fn new(store: FeedStore) -> Self {
        Self::with_schema_registry(store, SchemaRegistry::new(false))
    }

    /// Create a new feed engine with a custom schema registry.
    pub fn with_schema_registry(store: FeedStore, schema_registry: SchemaRegistry) -> Self {
        let (event_tx, _) = broadcast::channel(1024);
        Self {
            store,
            event_tx,
            schema_registry,
            transports: RwLock::new(Vec::new()),
            local_publish_tx: RwLock::new(None),
        }
    }

    /// Create a new feed engine with strict schema validation.
    /// In strict mode, messages without valid schemas are rejected.
    pub fn new_strict(store: FeedStore) -> Self {
        Self::with_schema_registry(store, SchemaRegistry::new(true))
    }

    /// Create a new feed engine with custom schemas from a directory.
    /// Loads all `.json` schema files from the directory.
    pub fn with_schemas_dir(store: FeedStore, schemas_dir: &Path) -> Self {
        Self::with_schema_registry(store, SchemaRegistry::with_schemas_dir(false, schemas_dir))
    }

    /// Create a new feed engine with strict validation and custom schemas.
    pub fn with_schemas_dir_strict(store: FeedStore, schemas_dir: &Path) -> Self {
        Self::with_schema_registry(store, SchemaRegistry::with_schemas_dir(true, schemas_dir))
    }

    pub fn store(&self) -> &FeedStore {
        &self.store
    }

    /// Get a reference to the schema registry.
    pub fn schema_registry(&self) -> &SchemaRegistry {
        &self.schema_registry
    }

    /// Subscribe to message events. Returns a receiver that will receive
    /// Arc<Message> for each published or ingested message.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Message>> {
        self.event_tx.subscribe()
    }

    /// Emit a message event to all subscribers.
    fn emit(&self, msg: &Message) {
        // Ignore send errors (no subscribers)
        let _ = self.event_tx.send(Arc::new(msg.clone()));
    }

    /// Publish a new message to the local identity's feed.
    /// Handles sequencing, hashing, and signing automatically.
    ///
    /// Uses atomic sequence allocation to prevent race conditions where
    /// concurrent publish calls could read the same sequence number.
    ///
    /// - `content`: The message payload (any JSON).
    /// - `relates`: Optional hash of a related message (for threading).
    /// - `tags`: Optional categorization tags.
    /// - `trace_id`: Optional distributed tracing identifier.
    /// - `span_id`: Optional distributed tracing span identifier.
    pub fn publish(
        &self,
        identity: &Identity,
        content: serde_json::Value,
        relates: Option<String>,
        tags: Vec<String>,
    ) -> Result<Message> {
        self.publish_with_trace(identity, content, relates, tags, None, None)
    }

    /// Publish a new message with optional distributed tracing context.
    pub fn publish_with_trace(
        &self,
        identity: &Identity,
        content: serde_json::Value,
        relates: Option<String>,
        tags: Vec<String>,
        trace_id: Option<String>,
        span_id: Option<String>,
    ) -> Result<Message> {
        self.publish_full(identity, content, None, relates, tags, trace_id, span_id)
    }

    /// Publish a new message with explicit schema_id.
    ///
    /// If `schema_id` is None, the schema is inferred from the content's "type" field.
    /// The content is validated against the schema before publishing.
    pub fn publish_with_schema(
        &self,
        identity: &Identity,
        content: serde_json::Value,
        schema_id: Option<String>,
        relates: Option<String>,
        tags: Vec<String>,
    ) -> Result<Message> {
        self.publish_full(identity, content, schema_id, relates, tags, None, None)
    }

    /// Full publish method with all optional parameters.
    #[allow(clippy::too_many_arguments)]
    pub fn publish_full(
        &self,
        identity: &Identity,
        content: serde_json::Value,
        schema_id: Option<String>,
        relates: Option<String>,
        tags: Vec<String>,
        trace_id: Option<String>,
        span_id: Option<String>,
    ) -> Result<Message> {
        let mut content = content;
        let original_schema_id = schema_id
            .clone()
            .or_else(|| self.schema_registry.infer_schema_id(&content));

        let (content, effective_schema_id) = if let Some(prepared) =
            private_box::prepare_for_publish(identity, content.clone(), original_schema_id.clone())?
        {
            self.schema_registry
                .validate(&prepared.plaintext_content, original_schema_id.as_deref())?;
            (prepared.encrypted_content, prepared.schema_id)
        } else {
            (std::mem::take(&mut content), original_schema_id)
        };

        // Validate content against schema
        self.schema_registry
            .validate(&content, effective_schema_id.as_deref())?;
        let author = identity.public_id();

        // Enforce same size limit as ingest — oversized messages won't replicate
        let content_size = serde_json::to_string(&content)
            .map(|s| s.len())
            .unwrap_or(MAX_CONTENT_SIZE + 1);
        if content_size > MAX_CONTENT_SIZE {
            return Err(EgreError::FeedIntegrity {
                reason: format!(
                    "content too large: {} bytes (max {})",
                    content_size, MAX_CONTENT_SIZE
                ),
            });
        }

        // Clone values needed in closure
        let relates_clone = relates.clone();
        let tags_clone = tags.clone();
        let trace_id_clone = trace_id.clone();
        let span_id_clone = span_id.clone();
        let schema_id_clone = effective_schema_id.clone();

        let message = self
            .store
            .publish_message_atomic(&author, |new_seq, previous| {
                let unsigned = UnsignedMessage {
                    author: author.clone(),
                    sequence: new_seq,
                    previous,
                    timestamp: Utc::now(),
                    content: content.clone(),
                    schema_id: schema_id_clone.clone(),
                    relates: relates_clone.clone(),
                    tags: tags_clone.clone(),
                    trace_id: trace_id_clone.clone(),
                    span_id: span_id_clone.clone(),
                    expires_at: None, // TTL set by caller if needed
                };

                let hash = unsigned.compute_hash();
                let sig = sign_bytes(identity, hash.as_bytes());
                let signature = B64.encode(sig.to_bytes());

                Ok(Message {
                    author: unsigned.author,
                    sequence: unsigned.sequence,
                    previous: unsigned.previous,
                    timestamp: unsigned.timestamp,
                    content: unsigned.content,
                    schema_id: effective_schema_id.clone(),
                    relates: relates.clone(),
                    tags: tags.clone(),
                    trace_id: trace_id.clone(),
                    span_id: span_id.clone(),
                    expires_at: unsigned.expires_at,
                    hash,
                    signature,
                })
            })?;

        self.emit(&message);

        // Phase 2 Wave 4 Step 21 (amendment §C.8 + §G.2):
        //
        // 1. Durable pending enqueue — synchronously write a
        //    `pending_forwarding` row for every transport that requires
        //    durable retry. Currently only `"bus"`; gossip has implicit
        //    Have/Want recovery so no pending row is needed.
        //
        //    The idempotent `INSERT OR IGNORE` in
        //    `pending_forwarding_enqueue` (amendment §G.2) means
        //    `BusTransport::publish_internal`'s own enqueue is a no-op
        //    when the dispatcher later handles this ticket — no
        //    duplicate-key errors, no race.
        //
        // 2. Ticket send — hand a `DispatchTicket` to the dispatcher
        //    via the bounded mpsc. `try_send` is intentional: on `Full`
        //    we log WARN and return the stored `Message` to the caller.
        //    Durability is preserved because the pending row exists and
        //    the retry scheduler will drain it on its own interval.
        let pending_transports = self.collect_pending_transports();
        if pending_transports.contains(&"bus") {
            // Logged at trace level; audit-scrubbed (A2): the hash is a
            // content-address, not PII, matching the gossip log posture.
            if let Err(e) = self.store.pending_forwarding_enqueue("bus", &message) {
                tracing::warn!(
                    hash = %message.hash,
                    error = %e,
                    "publish_full: pending_forwarding_enqueue failed; \
                     dispatcher will still attempt delivery"
                );
            }
        }
        if let Some(tx) = self.local_publish_tx.read().as_ref() {
            let ticket = DispatchTicket {
                message: Arc::new(message.clone()),
                pending_transports,
            };
            match tx.try_send(ticket) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    tracing::warn!(
                        hash = %message.hash,
                        "publish_dispatcher mpsc full; relying on pending_forwarding retry"
                    );
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    tracing::warn!(
                        hash = %message.hash,
                        "publish_dispatcher mpsc closed; publish durably stored but won't dispatch \
                         until restart"
                    );
                }
            }
        }
        // No dispatcher wired = test harness or gossip-only deployment
        // where `main.rs` did not wire a dispatch channel. The publish
        // is still durably stored in `messages` and visible on the SSE
        // event_tx; tests that don't care about wire fan-out leave the
        // dispatcher unwired.
        //
        // Production single-transport gossip deployments DO wire the
        // dispatcher (Wave 4 Step 22); PushManager was retired in Step 23.

        Ok(message)
    }

    /// Determine which attached transport backends require durable pending
    /// tracking for a freshly-published message. Wave 4 Step 21 only
    /// recognizes `"bus"`; future backends can extend the match here.
    ///
    /// Implementation note: we query `health().backend` on each attached
    /// transport. The call is cheap (a handful of atomic loads per
    /// `GossipTransport`/`BusTransport::health`) and the transports lock
    /// is read-only.
    fn collect_pending_transports(&self) -> Vec<&'static str> {
        let guard = self.transports.read();
        let mut out: Vec<&'static str> = Vec::new();
        for t in guard.iter() {
            match t.health().backend {
                "bus" if !out.contains(&"bus") => {
                    out.push("bus");
                }
                "composite" => {
                    // Composite surfaces its children's backends via
                    // `health().children`. If any child is bus, the
                    // composite is a bus-bearing transport and we need a
                    // pending row.
                    for child in t.health().children {
                        if child.backend == "bus" && !out.contains(&"bus") {
                            out.push("bus");
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// Verify and insert a message received from another feed.
    ///
    /// Hard rejects: invalid signature, hash mismatch, duplicate, structural
    /// violations (seq 0, seq 1 with previous, seq >1 without previous),
    /// forward fork (predecessor exists, hash doesn't match), backward fork
    /// (successor exists, points to different hash).
    ///
    /// Soft accept: messages with missing predecessors (gaps) are stored with
    /// `chain_valid = false`. The flag means "immediate backward link to
    /// predecessor was verified at ingest time" — it does NOT guarantee the
    /// entire chain from sequence 1 is validated. When backfill closes a gap,
    /// the successor's flag is promoted to true.
    pub fn ingest(&self, msg: &Message) -> Result<()> {
        // Extract content type for metrics (bounded label)
        let content_type = msg
            .content
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        // Log trace context if present (enables cross-component correlation)
        if let Some(ref trace_id) = msg.trace_id {
            tracing::debug!(
                trace_id = %trace_id,
                span_id = msg.span_id.as_deref().unwrap_or("-"),
                author = %msg.author.0,
                sequence = msg.sequence,
                content_type = %content_type,
                "ingesting message with trace context"
            );
        }

        if msg.sequence == 0 {
            metrics::record_message_ingested(content_type, "error");
            return Err(EgreError::FeedIntegrity {
                reason: "sequence must be >= 1".into(),
            });
        }

        // Reject oversized content (prevents amplification via gossip)
        let content_size = serde_json::to_string(&msg.content)
            .map(|s| s.len())
            .unwrap_or(MAX_CONTENT_SIZE + 1);
        if content_size > MAX_CONTENT_SIZE {
            metrics::record_message_ingested(content_type, "error");
            return Err(EgreError::FeedIntegrity {
                reason: format!(
                    "content too large: {} bytes (max {})",
                    content_size, MAX_CONTENT_SIZE
                ),
            });
        }

        // Cheap duplicate check before expensive Ed25519 verification
        if self
            .store
            .get_message_at_sequence(&msg.author, msg.sequence)?
            .is_some()
        {
            // Duplicates are expected during replication, not errors
            return Err(EgreError::DuplicateMessage {
                author: msg.author.0.clone(),
                sequence: msg.sequence,
            });
        }

        // Validate content against schema (before expensive signature check)
        if let Err(e) = self
            .schema_registry
            .validate(&msg.content, msg.schema_id.as_deref())
        {
            metrics::record_message_ingested(content_type, "error");
            return Err(e);
        }

        if let Err(e) = Self::verify_signature_and_hash(msg) {
            metrics::record_message_ingested(content_type, "error");
            return Err(e);
        }

        // Reject messages signed by a superseded key (key rotation enforcement).
        // Key rotation messages themselves are exempt (they must be signed by the old key).
        if content_type != "key_rotation" {
            if let Err(e) = self.check_key_not_rotated(&msg.author) {
                metrics::record_message_ingested(content_type, "error");
                return Err(e);
            }
        }

        if let Err(e) = Self::validate_previous_field(msg) {
            metrics::record_message_ingested(content_type, "error");
            return Err(e);
        }

        let chain_valid = match self.validate_chain_links(msg) {
            Ok(valid) => valid,
            Err(e) => {
                metrics::record_message_ingested(content_type, "error");
                return Err(e);
            }
        };

        self.store.insert_message(msg, chain_valid)?;
        self.emit(msg);

        // Record successful ingestion metrics
        metrics::record_message_ingested(content_type, "success");
        metrics::record_message_size(content_size);

        Ok(())
    }

    /// Verify Ed25519 signature and content hash integrity.
    fn verify_signature_and_hash(msg: &Message) -> Result<()> {
        let vk = msg.author.to_verifying_key()?;
        let sig_bytes = B64.decode(&msg.signature).map_err(|e| EgreError::Crypto {
            reason: format!("invalid signature encoding: {e}"),
        })?;
        let sig_array: [u8; 64] = sig_bytes.try_into().map_err(|_| EgreError::Crypto {
            reason: "signature must be 64 bytes".into(),
        })?;
        let sig = ed25519_dalek::Signature::from_bytes(&sig_array);
        verify_signature(&vk, msg.hash.as_bytes(), &sig)?;

        let unsigned = UnsignedMessage {
            author: msg.author.clone(),
            sequence: msg.sequence,
            previous: msg.previous.clone(),
            timestamp: msg.timestamp,
            content: msg.content.clone(),
            schema_id: msg.schema_id.clone(),
            relates: msg.relates.clone(),
            tags: msg.tags.clone(),
            trace_id: msg.trace_id.clone(),
            span_id: msg.span_id.clone(),
            expires_at: msg.expires_at,
        };
        let computed_hash = unsigned.compute_hash();
        if computed_hash != msg.hash {
            return Err(EgreError::FeedIntegrity {
                reason: format!(
                    "hash mismatch: computed={computed_hash}, claimed={}",
                    msg.hash
                ),
            });
        }
        Ok(())
    }

    /// Check if a signing key has been superseded by a key_rotation message.
    ///
    /// Queries for key_rotation messages where `old_key` matches the author.
    /// If any exist with an `effective_at` that has passed (or no effective_at,
    /// meaning immediately effective), the key is considered superseded.
    fn check_key_not_rotated(&self, author: &crate::identity::PublicId) -> Result<()> {
        let query = FeedQuery {
            content_type: Some("key_rotation".to_string()),
            limit: Some(100),
            ..Default::default()
        };

        let messages = self.store.query_messages(&query)?;

        let now = Utc::now();
        for msg in &messages {
            let old_key = msg.content.get("old_key").and_then(|v| v.as_str());
            if old_key != Some(&author.0) {
                continue;
            }

            // Check if effective_at has passed (or if absent, immediately effective)
            let effective_at = msg
                .content
                .get("effective_at")
                .and_then(|v| v.as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&Utc));

            let is_effective = match effective_at {
                Some(dt) => now >= dt,
                None => true, // No effective_at means immediately effective
            };

            if is_effective {
                return Err(EgreError::FeedIntegrity {
                    reason: format!(
                        "signing key {} has been superseded by key rotation; \
                         use the new key to publish",
                        author.0
                    ),
                });
            }
        }

        Ok(())
    }

    /// Validate structural rules on the `previous` field.
    fn validate_previous_field(msg: &Message) -> Result<()> {
        if msg.sequence == 1 && msg.previous.is_some() {
            return Err(EgreError::FeedIntegrity {
                reason: "first message must have no previous hash".into(),
            });
        }
        if msg.sequence > 1 && msg.previous.is_none() {
            return Err(EgreError::FeedIntegrity {
                reason: format!(
                    "message at sequence {} must reference previous hash",
                    msg.sequence
                ),
            });
        }
        Ok(())
    }

    /// Check forward and backward chain links. Returns whether the forward
    /// link was verified (chain_valid flag for storage).
    fn validate_chain_links(&self, msg: &Message) -> Result<bool> {
        // Forward link: do we have the predecessor?
        let chain_valid = if msg.sequence == 1 {
            true
        } else if let Some(prev_msg) = self
            .store
            .get_message_at_sequence(&msg.author, msg.sequence - 1)?
        {
            match &msg.previous {
                Some(claimed) if claimed != &prev_msg.hash => {
                    return Err(EgreError::FeedIntegrity {
                        reason: format!(
                            "previous hash mismatch at sequence {}: claimed={}, expected={}",
                            msg.sequence, claimed, prev_msg.hash
                        ),
                    });
                }
                _ => true,
            }
        } else {
            false
        };

        // Backward link: does a successor already exist?
        if let Some(next_seq) = msg.sequence.checked_add(1) {
            if let Some(next_msg) = self.store.get_message_at_sequence(&msg.author, next_seq)? {
                match &next_msg.previous {
                    Some(next_prev) if next_prev != &msg.hash => {
                        return Err(EgreError::FeedIntegrity {
                            reason: format!(
                                "backward chain mismatch: message {} successor claims previous={}, but this message hash={}",
                                msg.sequence, next_prev, msg.hash
                            ),
                        });
                    }
                    Some(_) => {
                        self.store.set_chain_valid(&next_msg.hash, true)?;
                    }
                    None => {}
                }
            }
        }

        Ok(chain_valid)
    }

    /// Query messages from the store.
    pub fn query(&self, query: &FeedQuery) -> Result<Vec<Message>> {
        self.store.query_messages(query)
    }

    /// Full-text search.
    pub fn search(&self, query_text: &str, limit: u32) -> Result<Vec<Message>> {
        self.store.search_messages(query_text, limit)
    }

    /// Get a single message by hash.
    pub fn get_message(&self, hash: &str) -> Result<Option<Message>> {
        self.store.get_message(hash)
    }

    /// Wire a sender to the `publish_dispatcher` task (Phase 2 Wave 4 Step 21).
    ///
    /// Called once by `main.rs` after the dispatcher task has been spawned
    /// with the matching receiver. Subsequent calls overwrite the sender —
    /// use with care (production wiring is once-at-boot).
    ///
    /// Publishers that arrive before this setter call (e.g. first-boot
    /// profile self-publish) do NOT block: `publish_full` still writes
    /// pending rows and returns `Ok(Message)`; the dispatcher simply
    /// doesn't see a ticket for them. Startup ordering in `main.rs` puts
    /// `set_dispatch_sender` before the effective transport's `start()`
    /// call so steady-state publishes flow through the dispatcher.
    pub fn set_dispatch_sender(&self, tx: mpsc::Sender<DispatchTicket>) {
        *self.local_publish_tx.write() = Some(tx);
    }

    /// Attach a transport to this engine. Appends to the transport list.
    ///
    /// This method exists so that transports holding `Arc<FeedEngine>` (e.g.
    /// `GossipTransport`) can be constructed after the engine itself is
    /// wrapped in an `Arc` — the alternative (passing transports to the
    /// constructor) creates a circular construction dependency (plan §11
    /// OQ-1).
    ///
    /// **Idempotency is not enforced.** Calling `attach_transport` twice with
    /// the same `Arc<dyn Transport>` (or two distinct transports of the same
    /// backend) results in two entries in the internal `Vec`. Callers
    /// (today: `main.rs` once at boot) are responsible for not double-
    /// attaching; a double-attach is a boot-sequence bug, not a library
    /// concern.
    pub fn attach_transport(&self, transport: Arc<dyn Transport>) {
        self.transports.write().push(transport);
    }

    /// Current number of attached transports. Consumed by the multi-transport
    /// WARN log in `main.rs` (plan §4 Step 8) and by Phase 2 dispatch logic.
    pub fn transport_count(&self) -> usize {
        self.transports.read().len()
    }

    /// Aggregate transport health for `/v1/status` consumers.
    ///
    /// - No transports attached → `None`.
    /// - One transport → that transport's `health()` verbatim (no wrapping).
    /// - Two or more transports → `TransportHealth::aggregate("composite", ..)`
    ///   per RFC 0001 §5.2.
    ///
    /// Phase 1 does **not** wire this to the `/v1/status` HTTP response (that
    /// would be an observable behavior change on the Phase 1 zero-behavior-
    /// change budget — plan §3.4). Exposed now so Phase 2 can wire it without
    /// re-touching this file.
    pub fn transport_health(&self) -> Option<TransportHealth> {
        let guard = self.transports.read();
        match guard.as_slice() {
            [] => None,
            [single] => Some(single.health()),
            many => Some(TransportHealth::aggregate(
                "composite",
                many.iter().map(|t| t.health()).collect(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::content_types::Content;

    fn setup() -> (FeedEngine, Identity) {
        let store = FeedStore::open_memory().unwrap();
        let engine = FeedEngine::new(store);
        let identity = Identity::generate();
        (engine, identity)
    }

    fn setup_with_schemas() -> (FeedEngine, Identity, std::path::PathBuf) {
        let temp_dir = std::env::temp_dir().join(format!(
            "egregore_engine_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&temp_dir);
        let store = FeedStore::open_memory().unwrap();
        let engine = FeedEngine::with_schemas_dir(store, &temp_dir);
        let identity = Identity::generate();
        (engine, identity, temp_dir)
    }

    #[test]
    fn publish_first_message() {
        let (engine, identity) = setup();
        let content = Content::Profile {
            name: "test-agent".to_string(),
            description: Some("A test agent".to_string()),
            capabilities: vec!["testing".to_string()],
            broker: None,
            valid_from: None,
            valid_until: None,
        };

        let msg = engine
            .publish(&identity, content.to_value(), None, vec![])
            .unwrap();
        assert_eq!(msg.sequence, 1);
        assert!(msg.previous.is_none());
        assert_eq!(msg.author, identity.public_id());
    }

    #[test]
    fn publish_chain_links() {
        let (engine, identity) = setup();

        let m1 = engine
            .publish(
                &identity,
                Content::Insight {
                    title: "First".to_string(),
                    context: None,
                    observation: "obs1".to_string(),
                    evidence: None,
                    guidance: None,
                    confidence: None,
                    tags: vec![],
                }
                .to_value(),
                None,
                vec![],
            )
            .unwrap();

        let m2 = engine
            .publish(
                &identity,
                Content::Insight {
                    title: "Second".to_string(),
                    context: None,
                    observation: "obs2".to_string(),
                    evidence: None,
                    guidance: None,
                    confidence: None,
                    tags: vec![],
                }
                .to_value(),
                None,
                vec![],
            )
            .unwrap();

        assert_eq!(m2.sequence, 2);
        assert_eq!(m2.previous.as_deref(), Some(m1.hash.as_str()));
    }

    #[test]
    fn publish_freeform_json() {
        let (engine, identity) = setup();
        let content = serde_json::json!({
            "type": "custom",
            "data": { "key": "value" },
            "numbers": [1, 2, 3],
        });

        let msg = engine.publish(&identity, content, None, vec![]).unwrap();
        assert_eq!(msg.sequence, 1);
        assert_eq!(msg.content["type"], "custom");
        assert_eq!(msg.content["data"]["key"], "value");
    }

    #[test]
    fn ingest_valid_message() {
        let (engine, identity) = setup();

        // Publish on "remote" engine, ingest on this one
        let remote_store = FeedStore::open_memory().unwrap();
        let remote_engine = FeedEngine::new(remote_store);

        let msg = remote_engine
            .publish(
                &identity,
                Content::Profile {
                    name: "remote".to_string(),
                    description: None,
                    capabilities: vec![],
                    broker: None,
                    valid_from: None,
                    valid_until: None,
                }
                .to_value(),
                None,
                vec![],
            )
            .unwrap();

        engine.ingest(&msg).unwrap();
        let retrieved = engine.get_message(&msg.hash).unwrap().unwrap();
        assert_eq!(retrieved.sequence, 1);
    }

    #[test]
    fn ingest_tampered_hash_rejected() {
        let (engine, identity) = setup();
        let remote_store = FeedStore::open_memory().unwrap();
        let remote_engine = FeedEngine::new(remote_store);

        let mut msg = remote_engine
            .publish(
                &identity,
                Content::Profile {
                    name: "remote".to_string(),
                    description: None,
                    capabilities: vec![],
                    broker: None,
                    valid_from: None,
                    valid_until: None,
                }
                .to_value(),
                None,
                vec![],
            )
            .unwrap();

        // Tamper with hash
        msg.hash = "0000000000000000000000000000000000000000000000000000000000000000".to_string();
        let result = engine.ingest(&msg);
        assert!(result.is_err());
    }

    #[test]
    fn ingest_gap_accepted_but_flagged() {
        let (engine, identity) = setup();
        let remote_store = FeedStore::open_memory().unwrap();
        let remote_engine = FeedEngine::new(remote_store);

        // Publish 3 messages on remote
        let _m1 = remote_engine
            .publish(
                &identity,
                serde_json::json!({"type": "test", "n": 1}),
                None,
                vec![],
            )
            .unwrap();
        let _m2 = remote_engine
            .publish(
                &identity,
                serde_json::json!({"type": "test", "n": 2}),
                None,
                vec![],
            )
            .unwrap();
        let m3 = remote_engine
            .publish(
                &identity,
                serde_json::json!({"type": "test", "n": 3}),
                None,
                vec![],
            )
            .unwrap();

        // Ingesting sequence 3 without 1 and 2 — accepted but flagged
        engine.ingest(&m3).unwrap();
        let retrieved = engine.get_message(&m3.hash).unwrap().unwrap();
        assert_eq!(retrieved.sequence, 3);
        assert!(
            !engine.store().is_chain_valid(&m3.hash).unwrap(),
            "message with gap should be flagged as chain_valid = false"
        );
    }

    #[test]
    fn ingest_fork_attack_rejected() {
        let (engine, identity) = setup();
        let remote_store = FeedStore::open_memory().unwrap();
        let remote_engine = FeedEngine::new(remote_store);

        // Publish m1 on remote, ingest it
        let m1 = remote_engine
            .publish(
                &identity,
                serde_json::json!({"type": "test", "n": 1}),
                None,
                vec![],
            )
            .unwrap();
        engine.ingest(&m1).unwrap();

        // Publish m2 on remote (legitimate)
        let mut m2 = remote_engine
            .publish(
                &identity,
                serde_json::json!({"type": "test", "n": 2}),
                None,
                vec![],
            )
            .unwrap();

        // Tamper: change previous to a fake hash (fork attack)
        m2.previous = Some("aaaa_fake_previous_hash".to_string());
        // Recompute hash and re-sign so signature checks pass
        let unsigned = UnsignedMessage {
            author: m2.author.clone(),
            sequence: m2.sequence,
            previous: m2.previous.clone(),
            timestamp: m2.timestamp,
            content: m2.content.clone(),
            schema_id: m2.schema_id.clone(),
            relates: m2.relates.clone(),
            tags: m2.tags.clone(),
            trace_id: m2.trace_id.clone(),
            span_id: m2.span_id.clone(),
            expires_at: m2.expires_at,
        };
        m2.hash = unsigned.compute_hash();
        let sig = sign_bytes(&identity, m2.hash.as_bytes());
        m2.signature = B64.encode(sig.to_bytes());

        let result = engine.ingest(&m2);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("previous hash mismatch"),
            "expected previous hash mismatch error, got: {err}"
        );
    }

    #[test]
    fn ingest_first_message_with_previous_rejected() {
        let (engine, identity) = setup();

        // Craft a sequence-1 message that claims a previous hash
        let unsigned = UnsignedMessage {
            author: identity.public_id(),
            sequence: 1,
            previous: Some("fake_previous_hash".to_string()),
            timestamp: Utc::now(),
            content: serde_json::json!({"type": "test"}),
            schema_id: None,
            relates: None,
            tags: vec![],
            trace_id: None,
            span_id: None,
            expires_at: None,
        };
        let hash = unsigned.compute_hash();
        let sig = sign_bytes(&identity, hash.as_bytes());
        let msg = Message {
            author: unsigned.author,
            sequence: unsigned.sequence,
            previous: unsigned.previous,
            timestamp: unsigned.timestamp,
            content: unsigned.content,
            schema_id: None,
            relates: None,
            tags: vec![],
            trace_id: None,
            span_id: None,
            expires_at: None,
            hash,
            signature: B64.encode(sig.to_bytes()),
        };

        let result = engine.ingest(&msg);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("first message must have no previous hash"),
            "expected no-previous error, got: {err}"
        );
    }

    #[test]
    fn ingest_missing_previous_at_sequence_2_rejected() {
        let (engine, identity) = setup();
        let remote_store = FeedStore::open_memory().unwrap();
        let remote_engine = FeedEngine::new(remote_store);

        // Ingest legitimate message 1
        let m1 = remote_engine
            .publish(
                &identity,
                serde_json::json!({"type": "test", "n": 1}),
                None,
                vec![],
            )
            .unwrap();
        engine.ingest(&m1).unwrap();

        // Craft sequence 2 with previous: None (invalid for seq > 1)
        let unsigned = UnsignedMessage {
            author: identity.public_id(),
            sequence: 2,
            previous: None,
            timestamp: Utc::now(),
            content: serde_json::json!({"type": "test", "n": 2}),
            schema_id: None,
            relates: None,
            tags: vec![],
            trace_id: None,
            span_id: None,
            expires_at: None,
        };
        let hash = unsigned.compute_hash();
        let sig = sign_bytes(&identity, hash.as_bytes());
        let msg = Message {
            author: unsigned.author,
            sequence: unsigned.sequence,
            previous: unsigned.previous,
            timestamp: unsigned.timestamp,
            content: unsigned.content,
            schema_id: None,
            relates: None,
            tags: vec![],
            trace_id: None,
            span_id: None,
            expires_at: None,
            hash,
            signature: B64.encode(sig.to_bytes()),
        };

        let result = engine.ingest(&msg);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("must reference previous hash"),
            "expected missing-previous error, got: {err}"
        );
    }

    #[test]
    fn ingest_backfill_promotes_chain_valid() {
        let (engine, identity) = setup();
        let remote_store = FeedStore::open_memory().unwrap();
        let remote_engine = FeedEngine::new(remote_store);

        let m1 = remote_engine
            .publish(
                &identity,
                serde_json::json!({"type": "test", "n": 1}),
                None,
                vec![],
            )
            .unwrap();
        let m2 = remote_engine
            .publish(
                &identity,
                serde_json::json!({"type": "test", "n": 2}),
                None,
                vec![],
            )
            .unwrap();
        let m3 = remote_engine
            .publish(
                &identity,
                serde_json::json!({"type": "test", "n": 3}),
                None,
                vec![],
            )
            .unwrap();

        // Late join: receive messages 2 and 3 (gap at 1)
        engine.ingest(&m2).unwrap();
        engine.ingest(&m3).unwrap();

        // m2 has no predecessor — flagged
        assert!(!engine.store().is_chain_valid(&m2.hash).unwrap());
        // m3 has predecessor m2 — chain_valid (forward link verified)
        assert!(engine.store().is_chain_valid(&m3.hash).unwrap());

        // Backfill: receive message 1
        engine.ingest(&m1).unwrap();

        // m1 is sequence 1 — always chain_valid
        assert!(engine.store().is_chain_valid(&m1.hash).unwrap());
        // m2 should now be promoted (backward link from m1 validates m2)
        assert!(
            engine.store().is_chain_valid(&m2.hash).unwrap(),
            "backfill should promote m2 to chain_valid"
        );
    }

    #[test]
    fn ingest_backward_fork_rejected() {
        let (engine, identity) = setup();
        let remote_store = FeedStore::open_memory().unwrap();
        let remote_engine = FeedEngine::new(remote_store);

        // Ingest message 2 first (gap at 1)
        let _m1 = remote_engine
            .publish(
                &identity,
                serde_json::json!({"type": "test", "n": 1}),
                None,
                vec![],
            )
            .unwrap();
        let m2 = remote_engine
            .publish(
                &identity,
                serde_json::json!({"type": "test", "n": 2}),
                None,
                vec![],
            )
            .unwrap();
        engine.ingest(&m2).unwrap();

        // Craft a fake message 1 whose hash doesn't match m2.previous
        let unsigned = UnsignedMessage {
            author: identity.public_id(),
            sequence: 1,
            previous: None,
            timestamp: Utc::now(),
            content: serde_json::json!({"type": "fake", "n": 999}),
            schema_id: None,
            relates: None,
            tags: vec![],
            trace_id: None,
            span_id: None,
            expires_at: None,
        };
        let hash = unsigned.compute_hash();
        let sig = sign_bytes(&identity, hash.as_bytes());
        let fake_m1 = Message {
            author: unsigned.author,
            sequence: unsigned.sequence,
            previous: unsigned.previous,
            timestamp: unsigned.timestamp,
            content: unsigned.content,
            schema_id: None,
            relates: None,
            tags: vec![],
            trace_id: None,
            span_id: None,
            expires_at: None,
            hash,
            signature: B64.encode(sig.to_bytes()),
        };

        // m2.previous points to the real m1's hash, not this fake
        let result = engine.ingest(&fake_m1);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("backward chain mismatch"),
            "expected backward chain mismatch, got: {err}"
        );
    }

    #[test]
    fn ingest_valid_chain_accepted() {
        let (engine, identity) = setup();
        let remote_store = FeedStore::open_memory().unwrap();
        let remote_engine = FeedEngine::new(remote_store);

        let m1 = remote_engine
            .publish(
                &identity,
                serde_json::json!({"type": "test", "n": 1}),
                None,
                vec![],
            )
            .unwrap();
        let m2 = remote_engine
            .publish(
                &identity,
                serde_json::json!({"type": "test", "n": 2}),
                None,
                vec![],
            )
            .unwrap();
        let m3 = remote_engine
            .publish(
                &identity,
                serde_json::json!({"type": "test", "n": 3}),
                None,
                vec![],
            )
            .unwrap();

        // Ingest in order — should all succeed
        engine.ingest(&m1).unwrap();
        engine.ingest(&m2).unwrap();
        engine.ingest(&m3).unwrap();

        assert_eq!(
            engine
                .store()
                .get_latest_sequence(&identity.public_id())
                .unwrap(),
            3
        );
    }

    #[test]
    fn search_published_content() {
        let (engine, identity) = setup();
        engine
            .publish(
                &identity,
                Content::Insight {
                    title: "Decentralized Knowledge".to_string(),
                    context: None,
                    observation: "LLMs benefit from shared episodic memory".to_string(),
                    evidence: None,
                    guidance: None,
                    confidence: Some(0.8),
                    tags: vec!["knowledge".to_string()],
                }
                .to_value(),
                None,
                vec!["knowledge".to_string()],
            )
            .unwrap();

        let results = engine.search("episodic", 10).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn concurrent_publish_no_sequence_conflict() {
        use std::sync::Arc;
        use std::thread;

        // Use a file-based database to enable true concurrent access
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");
        let store = FeedStore::open(&db_path).unwrap();
        let engine = Arc::new(FeedEngine::new(store));
        let identity = Arc::new(Identity::generate());

        const NUM_THREADS: usize = 10;
        const PUBLISHES_PER_THREAD: usize = 5;

        let mut handles = Vec::with_capacity(NUM_THREADS);

        for thread_id in 0..NUM_THREADS {
            let engine = Arc::clone(&engine);
            let identity = Arc::clone(&identity);

            handles.push(thread::spawn(move || {
                let mut results = Vec::new();
                for i in 0..PUBLISHES_PER_THREAD {
                    let content = serde_json::json!({
                        "type": "test",
                        "thread": thread_id,
                        "index": i,
                    });
                    match engine.publish(&identity, content, None, vec![]) {
                        Ok(msg) => results.push(msg.sequence),
                        Err(e) => panic!("Thread {thread_id} publish {i} failed: {e}"),
                    }
                }
                results
            }));
        }

        // Collect all sequence numbers from all threads
        let mut all_sequences: Vec<u64> = Vec::new();
        for handle in handles {
            let sequences = handle.join().expect("thread panicked");
            all_sequences.extend(sequences);
        }

        // Verify all sequences are unique
        all_sequences.sort();
        let expected_count = NUM_THREADS * PUBLISHES_PER_THREAD;
        assert_eq!(
            all_sequences.len(),
            expected_count,
            "should have {} messages",
            expected_count
        );

        // Check no duplicates: each sequence from 1 to N should appear exactly once
        for (i, &seq) in all_sequences.iter().enumerate() {
            assert_eq!(
                seq,
                (i + 1) as u64,
                "sequence {} missing or duplicate; got {:?}",
                i + 1,
                all_sequences
            );
        }

        // Verify final sequence in store
        let final_seq = engine
            .store()
            .get_latest_sequence(&identity.public_id())
            .unwrap();
        assert_eq!(final_seq, expected_count as u64);

        // Verify hash chain integrity
        let mut prev_hash: Option<String> = None;
        for seq in 1..=expected_count {
            let msg = engine
                .store()
                .get_message_at_sequence(&identity.public_id(), seq as u64)
                .unwrap()
                .unwrap_or_else(|| panic!("message {} should exist", seq));

            assert_eq!(msg.previous, prev_hash, "chain broken at sequence {}", seq);
            prev_hash = Some(msg.hash.clone());
        }
    }

    #[test]
    fn publish_with_trace_context() {
        let (engine, identity) = setup();

        let msg = engine
            .publish_with_trace(
                &identity,
                serde_json::json!({"type": "test", "data": "traced"}),
                None,
                vec![],
                Some("trace-123".to_string()),
                Some("span-456".to_string()),
            )
            .unwrap();

        assert_eq!(msg.trace_id, Some("trace-123".to_string()));
        assert_eq!(msg.span_id, Some("span-456".to_string()));

        // Verify trace context is part of the hash (via round-trip)
        let retrieved = engine
            .store()
            .get_message(&msg.hash)
            .unwrap()
            .expect("message should exist");
        assert_eq!(retrieved.trace_id, Some("trace-123".to_string()));
        assert_eq!(retrieved.span_id, Some("span-456".to_string()));
    }

    #[test]
    fn ingest_message_with_trace_context() {
        let (engine, identity) = setup();
        let remote_store = FeedStore::open_memory().unwrap();
        let remote_engine = FeedEngine::new(remote_store);

        // Publish on remote with trace context
        let m1 = remote_engine
            .publish_with_trace(
                &identity,
                serde_json::json!({"type": "test"}),
                None,
                vec![],
                Some("ingest-trace".to_string()),
                Some("ingest-span".to_string()),
            )
            .unwrap();

        // Ingest on local
        engine.ingest(&m1).unwrap();

        // Verify trace context preserved
        let retrieved = engine
            .store()
            .get_message(&m1.hash)
            .unwrap()
            .expect("message should exist");
        assert_eq!(retrieved.trace_id, Some("ingest-trace".to_string()));
        assert_eq!(retrieved.span_id, Some("ingest-span".to_string()));
    }

    #[test]
    fn publish_with_schema_id() {
        let (engine, identity) = setup();

        let msg = engine
            .publish_with_schema(
                &identity,
                Content::Insight {
                    title: "Schema Test".to_string(),
                    context: None,
                    observation: "Testing explicit schema_id".to_string(),
                    evidence: None,
                    guidance: None,
                    confidence: Some(0.9),
                    tags: vec![],
                }
                .to_value(),
                Some("insight/v1".to_string()),
                None,
                vec![],
            )
            .unwrap();

        assert_eq!(msg.schema_id, Some("insight/v1".to_string()));
    }

    #[test]
    fn publish_infers_schema_id() {
        let (engine, identity, temp_dir) = setup_with_schemas();

        let msg = engine
            .publish(
                &identity,
                Content::Profile {
                    name: "Inferred Schema".to_string(),
                    description: None,
                    capabilities: vec![],
                    broker: None,
                    valid_from: None,
                    valid_until: None,
                }
                .to_value(),
                None,
                vec![],
            )
            .unwrap();

        // Should infer profile/v1 from the content type
        assert_eq!(msg.schema_id, Some("profile/v1".to_string()));
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn publish_custom_type_no_schema() {
        let (engine, identity) = setup();

        // Custom types without registered schema should still work (non-strict mode)
        let msg = engine
            .publish(
                &identity,
                serde_json::json!({
                    "type": "custom_extension",
                    "data": "test"
                }),
                None,
                vec![],
            )
            .unwrap();

        // No schema inferred for unknown types
        assert!(msg.schema_id.is_none());
    }

    #[test]
    fn strict_mode_rejects_invalid_content() {
        let store = FeedStore::open_memory().unwrap();
        let engine = FeedEngine::new_strict(store);
        let identity = Identity::generate();

        // Invalid insight (missing required "observation" field)
        let result = engine.publish(
            &identity,
            serde_json::json!({
                "type": "insight",
                "title": "Invalid"
                // missing "observation"
            }),
            None,
            vec![],
        );

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("schema") || err.contains("observation"));
    }

    #[test]
    fn strict_mode_rejects_unknown_content_type() {
        let store = FeedStore::open_memory().unwrap();
        let engine = FeedEngine::new_strict(store);
        let identity = Identity::generate();

        let result = engine.publish(
            &identity,
            serde_json::json!({
                "type": "unknown_type",
                "data": "test"
            }),
            None,
            vec![],
        );

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("unknown"));
    }

    #[test]
    fn ingest_validates_schema() {
        let temp_dir = std::env::temp_dir().join(format!(
            "egregore_ingest_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&temp_dir);

        let store = FeedStore::open_memory().unwrap();
        let strict_engine = FeedEngine::with_schemas_dir_strict(store, &temp_dir);

        let remote_store = FeedStore::open_memory().unwrap();
        let remote_engine = FeedEngine::with_schemas_dir(remote_store, &temp_dir);
        let identity = Identity::generate();

        // Publish a valid message on remote (non-strict)
        let valid_msg = remote_engine
            .publish(
                &identity,
                Content::Insight {
                    title: "Valid".to_string(),
                    context: None,
                    observation: "Valid observation".to_string(),
                    evidence: None,
                    guidance: None,
                    confidence: Some(0.9),
                    tags: vec![],
                }
                .to_value(),
                None,
                vec![],
            )
            .unwrap();

        // Should ingest successfully on strict engine
        strict_engine.ingest(&valid_msg).unwrap();
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn schema_id_included_in_message_hash() {
        let (engine, identity) = setup();

        // Publish same content with different schema_ids
        let msg1 = engine
            .publish_with_schema(
                &identity,
                Content::Profile {
                    name: "Test".to_string(),
                    description: None,
                    capabilities: vec![],
                    broker: None,
                    valid_from: None,
                    valid_until: None,
                }
                .to_value(),
                Some("profile/v1".to_string()),
                None,
                vec![],
            )
            .unwrap();

        // Create a second engine to avoid sequence collision
        let store2 = FeedStore::open_memory().unwrap();
        let engine2 = FeedEngine::new(store2);

        let msg2 = engine2
            .publish_with_schema(
                &identity,
                Content::Profile {
                    name: "Test".to_string(),
                    description: None,
                    capabilities: vec![],
                    broker: None,
                    valid_from: None,
                    valid_until: None,
                }
                .to_value(),
                Some("profile/v2".to_string()),
                None,
                vec![],
            )
            .unwrap();

        // Same content but different schema_id should produce different hash
        // (timestamps also differ, but schema_id is part of the signed data)
        assert_ne!(msg1.hash, msg2.hash);
        assert_eq!(msg1.schema_id, Some("profile/v1".to_string()));
        assert_eq!(msg2.schema_id, Some("profile/v2".to_string()));
    }

    // ------------------------------------------------------------------
    // Step 5 — `transports` field, `attach_transport`, `transport_count`,
    // `transport_health`. Tests use an inline test-only mock transport; the
    // full `MockTransport` test harness lives in `tests/common/mock_transport.rs`
    // (arrives in Step 9).
    // ------------------------------------------------------------------

    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use std::time::Duration;

    use crate::feed::models::Message;
    use crate::identity::PublicId;
    use crate::transport::{
        filter::TopicFilter, subscription::SubscriptionHandle, Transport, TransportHealth,
    };

    /// Inline test-only mock. Canned `health()` response; all other trait
    /// methods `unimplemented!()` because the engine's transport-surface tests
    /// only exercise `health()` + counting. The fuller `MockTransport` with
    /// publish/subscribe/request_from fidelity lives in
    /// `tests/common/mock_transport.rs` and arrives in Step 9.
    struct InlineMock {
        health: TransportHealth,
    }

    impl InlineMock {
        fn new(connected: bool, backend: &'static str) -> Arc<Self> {
            Arc::new(Self {
                health: TransportHealth {
                    connected,
                    backend,
                    last_successful_publish: None,
                    last_peer_contact: None,
                    unreplicated_count: 0,
                    inflight_publishes: 0,
                    last_error: None,
                    children: vec![],
                    bridge_queues: None,
                },
            })
        }
    }

    #[async_trait]
    impl Transport for InlineMock {
        async fn publish(&self, _msg: &Message) -> Result<()> {
            unimplemented!("InlineMock::publish — not exercised by engine.rs Step 5 tests")
        }

        async fn subscribe(
            &self,
            _filter: TopicFilter,
        ) -> Result<(SubscriptionHandle, BoxStream<'static, Message>)> {
            unimplemented!("InlineMock::subscribe — not exercised by engine.rs Step 5 tests")
        }

        async fn request_from(
            &self,
            _author: PublicId,
            _after_seq: u64,
        ) -> Result<BoxStream<'static, Message>> {
            unimplemented!("InlineMock::request_from — not exercised by engine.rs Step 5 tests")
        }

        async fn start(&self) -> Result<()> {
            Ok(())
        }

        async fn shutdown(&self, _deadline: Duration) -> Result<()> {
            Ok(())
        }

        fn health(&self) -> TransportHealth {
            self.health.clone()
        }
    }

    #[test]
    fn feed_engine_default_has_no_transports() {
        let store = FeedStore::open_memory().unwrap();
        let engine = FeedEngine::new(store);
        assert_eq!(
            engine.transport_count(),
            0,
            "fresh engine must have zero transports"
        );
        assert!(
            engine.transport_health().is_none(),
            "fresh engine with no transports reports no health"
        );
    }

    #[test]
    fn attach_transport_appends_and_counts() {
        let store = FeedStore::open_memory().unwrap();
        let engine = FeedEngine::new(store);

        engine.attach_transport(InlineMock::new(true, "mock-a"));
        assert_eq!(engine.transport_count(), 1);
        assert!(
            engine.transport_health().is_some(),
            "single-transport engine surfaces that transport's health"
        );

        engine.attach_transport(InlineMock::new(true, "mock-b"));
        assert_eq!(engine.transport_count(), 2);

        let agg = engine
            .transport_health()
            .expect("two-transport engine produces aggregate health");
        assert_eq!(
            agg.backend, "composite",
            "multi-transport aggregate uses 'composite' backend label"
        );
        assert_eq!(
            agg.children.len(),
            2,
            "aggregate retains per-transport children for operator visibility"
        );
    }

    #[test]
    fn transport_health_single_returns_child_health() {
        // With one transport attached, transport_health() returns that
        // transport's health verbatim (no wrapping). The backend label passes
        // through to the returned health — this is the observable contract
        // plan §7.1 specifies.
        let store = FeedStore::open_memory().unwrap();
        let engine = FeedEngine::new(store);

        engine.attach_transport(InlineMock::new(true, "gossip"));
        let h = engine
            .transport_health()
            .expect("single-transport health exposed");
        assert_eq!(
            h.backend, "gossip",
            "single-transport path must NOT relabel; must pass child backend through"
        );
        assert!(h.connected, "child's connected flag passes through");
        assert!(
            h.children.is_empty(),
            "single-transport path does not wrap the child in a composite"
        );
    }

    #[test]
    fn transport_health_multi_aggregates() {
        // With two transports whose `connected` flags differ, the top-level
        // health's `connected` follows AND semantics (RFC 0001 §5.2 /
        // TransportHealth::aggregate). Any disconnected child forces the
        // aggregate to report disconnected.
        let store = FeedStore::open_memory().unwrap();
        let engine = FeedEngine::new(store);

        engine.attach_transport(InlineMock::new(true, "mock-a"));
        engine.attach_transport(InlineMock::new(false, "mock-b"));

        let agg = engine
            .transport_health()
            .expect("two-transport aggregate present");
        assert!(
            !agg.connected,
            "AND across children: one disconnected child forces aggregate disconnected"
        );
        assert_eq!(agg.backend, "composite");
        assert_eq!(agg.children.len(), 2);
    }

    // ------------------------------------------------------------------
    // Step 21 — `local_publish_tx` bounded mpsc + synchronous pending
    // enqueue in `publish_full`. These exercise the behavior described in
    // amendment §C.8 + §G.2: a bus-bearing deployment writes a durable
    // pending row before the ticket send; on mpsc Full the publish still
    // returns Ok and the pending row is authoritative.
    // ------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn publish_full_enqueues_pending_when_bus_transport_attached() {
        let store = FeedStore::open_memory().unwrap();
        let engine = FeedEngine::new(store);
        let identity = Identity::generate();

        engine.attach_transport(InlineMock::new(true, "bus"));

        let msg = engine
            .publish(
                &identity,
                serde_json::json!({"type": "test", "n": 1}),
                None,
                vec![],
            )
            .unwrap();

        // Pending row must exist for "bus" because a bus-backend
        // transport is attached.
        let pending = engine.store().pending_forwarding_list("bus", 10).unwrap();
        assert_eq!(
            pending.len(),
            1,
            "bus-attached publish must create a pending_forwarding row"
        );
        assert_eq!(pending[0].message_hash, msg.hash);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn publish_full_does_not_enqueue_pending_when_no_bus() {
        let store = FeedStore::open_memory().unwrap();
        let engine = FeedEngine::new(store);
        let identity = Identity::generate();

        // Only gossip attached — no pending row should be created.
        engine.attach_transport(InlineMock::new(true, "gossip"));

        engine
            .publish(
                &identity,
                serde_json::json!({"type": "test", "n": 1}),
                None,
                vec![],
            )
            .unwrap();

        let count = engine.store().pending_forwarding_count("bus").unwrap();
        assert_eq!(
            count, 0,
            "gossip-only deployment must not enqueue pending rows; got {count}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn publish_full_sends_dispatch_ticket_when_channel_attached() {
        let store = FeedStore::open_memory().unwrap();
        let engine = FeedEngine::new(store);
        let identity = Identity::generate();

        engine.attach_transport(InlineMock::new(true, "bus"));

        let (tx, mut rx) = mpsc::channel::<DispatchTicket>(4);
        engine.set_dispatch_sender(tx);

        let msg = engine
            .publish(
                &identity,
                serde_json::json!({"type": "test", "n": 1}),
                None,
                vec![],
            )
            .unwrap();

        // Dispatcher receives exactly one ticket whose message is the
        // freshly-published one and whose pending_transports contains bus.
        let ticket = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("dispatcher recv must not time out")
            .expect("dispatcher channel must not close");
        assert_eq!(ticket.message.hash, msg.hash);
        assert!(
            ticket.pending_transports.contains(&"bus"),
            "ticket must name 'bus' as a durable-pending transport; got: {:?}",
            ticket.pending_transports
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn publish_full_continues_on_dispatch_mpsc_full() {
        let store = FeedStore::open_memory().unwrap();
        let engine = FeedEngine::new(store);
        let identity = Identity::generate();

        engine.attach_transport(InlineMock::new(true, "bus"));

        // Capacity 1 and we never recv: the first publish fills the
        // channel; the second hits TrySendError::Full and must still
        // return Ok (pending row is authoritative — amendment §C.8).
        let (tx, _rx) = mpsc::channel::<DispatchTicket>(1);
        engine.set_dispatch_sender(tx);

        let _m1 = engine
            .publish(
                &identity,
                serde_json::json!({"type": "t", "n": 1}),
                None,
                vec![],
            )
            .expect("first publish fills the mpsc");
        let _m2 = engine
            .publish(
                &identity,
                serde_json::json!({"type": "t", "n": 2}),
                None,
                vec![],
            )
            .expect("second publish must return Ok even when mpsc is Full");

        // Both publishes created durable pending rows — that's the
        // recovery channel when the dispatcher is saturated.
        let count = engine.store().pending_forwarding_count("bus").unwrap();
        assert_eq!(
            count, 2,
            "both publishes must have durable pending rows; got {count}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn publish_full_continues_on_dispatch_mpsc_closed() {
        let store = FeedStore::open_memory().unwrap();
        let engine = FeedEngine::new(store);
        let identity = Identity::generate();

        engine.attach_transport(InlineMock::new(true, "bus"));

        // Create the channel, wire the sender, then drop the receiver —
        // any subsequent `try_send` returns `Closed`. publish_full must
        // still return Ok (same recovery posture as Full).
        let (tx, rx) = mpsc::channel::<DispatchTicket>(4);
        engine.set_dispatch_sender(tx);
        drop(rx);

        let msg = engine
            .publish(
                &identity,
                serde_json::json!({"type": "test", "n": 1}),
                None,
                vec![],
            )
            .expect("publish must return Ok even when dispatcher channel is closed");

        // Pending row still exists — the retry scheduler will pick it up
        // on next tick.
        let pending = engine.store().pending_forwarding_list("bus", 10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].message_hash, msg.hash);
    }
}
