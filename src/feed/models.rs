use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::identity::PublicId;

/// A signed message in a feed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub author: PublicId,
    pub sequence: u64,
    /// Hash of previous message (None for sequence 1).
    pub previous: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub content: Content,
    /// SHA-256 hex of canonical JSON (excluding hash + signature).
    pub hash: String,
    /// Ed25519 signature of hash bytes (base64).
    pub signature: String,
}

/// Content type enum, tagged by "type" field in JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Content {
    Insight {
        title: String,
        #[serde(default)]
        context: Option<String>,
        observation: String,
        #[serde(default)]
        evidence: Option<String>,
        #[serde(default)]
        guidance: Option<String>,
        #[serde(default)]
        confidence: Option<f64>,
        #[serde(default)]
        tags: Vec<String>,
    },
    Endorsement {
        /// Hash of the endorsed message.
        message_hash: String,
        #[serde(default)]
        comment: Option<String>,
    },
    Dispute {
        /// Hash of the disputed message.
        message_hash: String,
        reason: String,
        #[serde(default)]
        evidence: Option<String>,
    },
    Query {
        question: String,
        #[serde(default)]
        tags: Vec<String>,
    },
    Response {
        /// Hash of the query message.
        query_hash: String,
        answer: String,
        #[serde(default)]
        confidence: Option<f64>,
    },
    Profile {
        name: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        capabilities: Vec<String>,
    },
}

/// Unsigned message body — used to compute hash before signing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnsignedMessage {
    pub author: PublicId,
    pub sequence: u64,
    pub previous: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub content: Content,
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
    fn content_insight_serialization() {
        let content = Content::Insight {
            title: "Test".to_string(),
            context: None,
            observation: "Observed".to_string(),
            evidence: None,
            guidance: None,
            confidence: Some(0.85),
            tags: vec!["test".to_string()],
        };
        let json = serde_json::to_string(&content).unwrap();
        assert!(json.contains(r#""type":"insight"#));

        let parsed: Content = serde_json::from_str(&json).unwrap();
        match parsed {
            Content::Insight { title, confidence, .. } => {
                assert_eq!(title, "Test");
                assert_eq!(confidence, Some(0.85));
            }
            _ => panic!("wrong variant"),
        }
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
            content: Content::Profile {
                name: "test".to_string(),
                description: None,
                capabilities: vec![],
            },
        };
        let h1 = msg.compute_hash();
        let h2 = msg.compute_hash();
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // SHA-256 hex
    }
}
