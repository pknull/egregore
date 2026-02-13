use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use chrono::Utc;

use crate::error::{EgreError, Result};
use crate::feed::models::{Content, FeedQuery, Message, UnsignedMessage};
use crate::feed::store::FeedStore;
use crate::identity::{sign_bytes, verify_signature, Identity, PublicId};

/// Feed engine: append with chain validation, read, verify, search.
pub struct FeedEngine {
    store: FeedStore,
}

impl FeedEngine {
    pub fn new(store: FeedStore) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &FeedStore {
        &self.store
    }

    /// Publish a new message to the local identity's feed.
    /// Handles sequencing, hashing, and signing automatically.
    pub fn publish(&self, identity: &Identity, content: Content) -> Result<Message> {
        let author = identity.public_id();
        let latest_seq = self.store.get_latest_sequence(&author)?;
        let new_seq = latest_seq + 1;

        // Get previous message hash
        let previous = if new_seq > 1 {
            let prev_msgs = self.store.get_messages_after(&author, latest_seq - 1, 1)?;
            Some(
                prev_msgs
                    .first()
                    .ok_or_else(|| EgreError::FeedIntegrity {
                        reason: format!("missing message at sequence {latest_seq}"),
                    })?
                    .hash
                    .clone(),
            )
        } else {
            None
        };

        let unsigned = UnsignedMessage {
            author: author.clone(),
            sequence: new_seq,
            previous,
            timestamp: Utc::now(),
            content,
        };

        let hash = unsigned.compute_hash();
        let sig = sign_bytes(identity, hash.as_bytes());
        let signature = B64.encode(sig.to_bytes());

        let message = Message {
            author: unsigned.author,
            sequence: unsigned.sequence,
            previous: unsigned.previous,
            timestamp: unsigned.timestamp,
            content: unsigned.content,
            hash,
            signature,
        };

        self.store.insert_message(&message)?;
        Ok(message)
    }

    /// Verify and insert a message received from another feed.
    /// Validates signature and chain integrity.
    pub fn ingest(&self, msg: &Message) -> Result<()> {
        // Verify signature
        let vk = msg.author.to_verifying_key()?;
        let sig_bytes = B64.decode(&msg.signature).map_err(|e| EgreError::Crypto {
            reason: format!("invalid signature encoding: {e}"),
        })?;
        let sig_array: [u8; 64] = sig_bytes.try_into().map_err(|_| EgreError::Crypto {
            reason: "signature must be 64 bytes".into(),
        })?;
        let sig = ed25519_dalek::Signature::from_bytes(&sig_array);
        verify_signature(&vk, msg.hash.as_bytes(), &sig)?;

        // Verify hash matches content
        let unsigned = UnsignedMessage {
            author: msg.author.clone(),
            sequence: msg.sequence,
            previous: msg.previous.clone(),
            timestamp: msg.timestamp,
            content: msg.content.clone(),
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

        // Verify chain linkage
        let expected_seq = self.store.get_latest_sequence(&msg.author)? + 1;
        if msg.sequence != expected_seq {
            // Allow out-of-order for now (gossip may deliver non-sequentially)
            // but reject if we already have this sequence
            if msg.sequence <= self.store.get_latest_sequence(&msg.author)? {
                return Err(EgreError::DuplicateMessage {
                    author: msg.author.0.clone(),
                    sequence: msg.sequence,
                });
            }
        }

        self.store.insert_message(msg)
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

        let msg = engine.publish(&identity, content).unwrap();
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
                },
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
                },
            )
            .unwrap();

        assert_eq!(m2.sequence, 2);
        assert_eq!(m2.previous.as_deref(), Some(m1.hash.as_str()));
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
                },
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
                },
            )
            .unwrap();

        // Tamper with hash
        msg.hash = "0000000000000000000000000000000000000000000000000000000000000000".to_string();
        let result = engine.ingest(&msg);
        assert!(result.is_err());
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
                },
            )
            .unwrap();

        let results = engine.search("episodic", 10).unwrap();
        assert_eq!(results.len(), 1);
    }
}
