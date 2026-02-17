//! Feed data model — signed messages forming per-author hash chains.
//!
//! Each message is signed by its author's Ed25519 key and linked to its
//! predecessor via `previous` (SHA-256 hash of the prior message). This forms
//! an append-only chain per author. Sequence numbers are monotonically increasing
//! starting at 1.
//!
//! Hash chain integrity is verified at ingest time (see feed/engine.rs).
//! Messages arriving out of order are stored with `chain_valid = false` and
//! promoted when their predecessor arrives (backfill).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::identity::PublicId;

// Re-export Content for backward compatibility
pub use crate::feed::content_types::Content;

/// A signed message in a feed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub author: PublicId,
    pub sequence: u64,
    /// Hash of previous message (None for sequence 1).
    pub previous: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub content: serde_json::Value,
    /// SHA-256 hex of canonical JSON (excluding hash + signature).
    pub hash: String,
    /// Ed25519 signature of hash bytes (base64).
    pub signature: String,
}

/// Unsigned message body — used to compute hash before signing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnsignedMessage {
    pub author: PublicId,
    pub sequence: u64,
    pub previous: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub content: serde_json::Value,
}

impl UnsignedMessage {
    /// Compute SHA-256 hash of canonical JSON representation.
    pub fn compute_hash(&self) -> String {
        use sha2::{Digest, Sha256};
        let json = serde_json::to_string(self).expect("serialization should not fail");
        let hash = Sha256::digest(json.as_bytes());
        hex::encode(hash)
    }
}

/// Helper: hex encode bytes (no external dep needed).
mod hex {
    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        bytes
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

/// Pagination parameters for feed queries.
#[derive(Debug, Clone, Default)]
pub struct FeedQuery {
    pub author: Option<PublicId>,
    /// Exclude messages from this author (used for "others only" queries).
    pub exclude_author: Option<PublicId>,
    pub content_type: Option<String>,
    pub tag: Option<String>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
    pub search: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_value_serialization() {
        let content = serde_json::json!({
            "type": "insight",
            "title": "Test",
            "observation": "Observed",
            "confidence": 0.85,
            "tags": ["test"],
        });
        let json = serde_json::to_string(&content).unwrap();
        assert!(json.contains("\"type\":\"insight\""));

        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["title"], "Test");
        assert_eq!(parsed["confidence"], 0.85);
    }

    #[test]
    fn content_enum_to_value_hash_matches() {
        let content = Content::Profile {
            name: "test".to_string(),
            description: None,
            capabilities: vec![],
        };
        let from_enum = content.to_value();
        let from_json = serde_json::json!({
            "type": "profile",
            "name": "test",
            "description": null,
            "capabilities": [],
        });

        // Both produce identical hashes
        let msg_enum = UnsignedMessage {
            author: PublicId("@test.ed25519".to_string()),
            sequence: 1,
            previous: None,
            timestamp: DateTime::parse_from_rfc3339("2026-02-12T18:30:00Z")
                .unwrap()
                .with_timezone(&Utc),
            content: from_enum,
        };
        let msg_json = UnsignedMessage {
            author: PublicId("@test.ed25519".to_string()),
            sequence: 1,
            previous: None,
            timestamp: DateTime::parse_from_rfc3339("2026-02-12T18:30:00Z")
                .unwrap()
                .with_timezone(&Utc),
            content: from_json,
        };
        assert_eq!(msg_enum.compute_hash(), msg_json.compute_hash());
    }

    #[test]
    fn unsigned_message_hash_deterministic() {
        let msg = UnsignedMessage {
            author: PublicId("@test.ed25519".to_string()),
            sequence: 1,
            previous: None,
            timestamp: DateTime::parse_from_rfc3339("2026-02-12T18:30:00Z")
                .unwrap()
                .with_timezone(&Utc),
            content: serde_json::json!({
                "type": "profile",
                "name": "test",
                "description": null,
                "capabilities": [],
            }),
        };
        let h1 = msg.compute_hash();
        let h2 = msg.compute_hash();
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // SHA-256 hex
    }
}
