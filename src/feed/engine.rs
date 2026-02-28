use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use chrono::Utc;
use tokio::sync::broadcast;

use crate::error::{EgreError, Result};
use crate::feed::models::{FeedQuery, Message, UnsignedMessage};
use crate::feed::schema::SchemaRegistry;
use crate::feed::store::FeedStore;
use crate::identity::{sign_bytes, verify_signature, Identity};

/// Maximum serialized content size in bytes (64 KB).
const MAX_CONTENT_SIZE: usize = 64 * 1024;

/// Feed engine: append with chain validation, read, verify, search.
///
/// Emits events via broadcast channel when messages are published or ingested.
/// Subscribe with `engine.subscribe()` to receive real-time notifications.
pub struct FeedEngine {
    store: FeedStore,
    event_tx: broadcast::Sender<Arc<Message>>,
    schema_registry: SchemaRegistry,
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
        }
    }

    /// Create a new feed engine with strict schema validation.
    /// In strict mode, messages without valid schemas are rejected.
    pub fn new_strict(store: FeedStore) -> Self {
        Self::with_schema_registry(store, SchemaRegistry::new(true))
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
        // Determine effective schema_id
        let effective_schema_id = schema_id.or_else(|| self.schema_registry.infer_schema_id(&content));

        // Validate content against schema
        self.schema_registry.validate(&content, effective_schema_id.as_deref())?;
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

        let message = self.store.publish_message_atomic(&author, |new_seq, previous| {
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
                hash,
                signature,
            })
        })?;

        self.emit(&message);
        Ok(message)
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
        if msg.sequence == 0 {
            return Err(EgreError::FeedIntegrity {
                reason: "sequence must be >= 1".into(),
            });
        }

        // Reject oversized content (prevents amplification via gossip)
        let content_size = serde_json::to_string(&msg.content)
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

        // Cheap duplicate check before expensive Ed25519 verification
        if self.store.get_message_at_sequence(&msg.author, msg.sequence)?.is_some() {
            return Err(EgreError::DuplicateMessage {
                author: msg.author.0.clone(),
                sequence: msg.sequence,
            });
        }

        // Validate content against schema (before expensive signature check)
        self.schema_registry.validate(&msg.content, msg.schema_id.as_deref())?;

        Self::verify_signature_and_hash(msg)?;
        Self::validate_previous_field(msg)?;

        let chain_valid = self.validate_chain_links(msg)?;
        self.store.insert_message(msg, chain_valid)?;
        self.emit(msg);
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
        } else if let Some(prev_msg) =
            self.store.get_message_at_sequence(&msg.author, msg.sequence - 1)?
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
            if let Some(next_msg) =
                self.store.get_message_at_sequence(&msg.author, next_seq)?
            {
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

    #[test]
    fn publish_first_message() {
        let (engine, identity) = setup();
        let content = Content::Profile {
            name: "test-agent".to_string(),
            description: Some("A test agent".to_string()),
            capabilities: vec!["testing".to_string()],
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
            .publish(&identity, serde_json::json!({"type": "test", "n": 1}), None, vec![])
            .unwrap();
        let _m2 = remote_engine
            .publish(&identity, serde_json::json!({"type": "test", "n": 2}), None, vec![])
            .unwrap();
        let m3 = remote_engine
            .publish(&identity, serde_json::json!({"type": "test", "n": 3}), None, vec![])
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
            .publish(&identity, serde_json::json!({"type": "test", "n": 1}), None, vec![])
            .unwrap();
        engine.ingest(&m1).unwrap();

        // Publish m2 on remote (legitimate)
        let mut m2 = remote_engine
            .publish(&identity, serde_json::json!({"type": "test", "n": 2}), None, vec![])
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
            .publish(&identity, serde_json::json!({"type": "test", "n": 1}), None, vec![])
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
            .publish(&identity, serde_json::json!({"type": "test", "n": 1}), None, vec![])
            .unwrap();
        let m2 = remote_engine
            .publish(&identity, serde_json::json!({"type": "test", "n": 2}), None, vec![])
            .unwrap();
        let m3 = remote_engine
            .publish(&identity, serde_json::json!({"type": "test", "n": 3}), None, vec![])
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
            .publish(&identity, serde_json::json!({"type": "test", "n": 1}), None, vec![])
            .unwrap();
        let m2 = remote_engine
            .publish(&identity, serde_json::json!({"type": "test", "n": 2}), None, vec![])
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
            .publish(&identity, serde_json::json!({"type": "test", "n": 1}), None, vec![])
            .unwrap();
        let m2 = remote_engine
            .publish(&identity, serde_json::json!({"type": "test", "n": 2}), None, vec![])
            .unwrap();
        let m3 = remote_engine
            .publish(&identity, serde_json::json!({"type": "test", "n": 3}), None, vec![])
            .unwrap();

        // Ingest in order — should all succeed
        engine.ingest(&m1).unwrap();
        engine.ingest(&m2).unwrap();
        engine.ingest(&m3).unwrap();

        assert_eq!(
            engine.store().get_latest_sequence(&identity.public_id()).unwrap(),
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

            assert_eq!(
                msg.previous, prev_hash,
                "chain broken at sequence {}",
                seq
            );
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
        let (engine, identity) = setup();

        let msg = engine
            .publish(
                &identity,
                Content::Profile {
                    name: "Inferred Schema".to_string(),
                    description: None,
                    capabilities: vec![],
                }
                .to_value(),
                None,
                vec![],
            )
            .unwrap();

        // Should infer profile/v1 from the content type
        assert_eq!(msg.schema_id, Some("profile/v1".to_string()));
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
        let store = FeedStore::open_memory().unwrap();
        let strict_engine = FeedEngine::new_strict(store);

        let remote_store = FeedStore::open_memory().unwrap();
        let remote_engine = FeedEngine::new(remote_store);
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
}
