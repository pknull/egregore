use serde::{Deserialize, Serialize};

/// Convenience content types for structured publishing.
/// These are NOT the wire format — the protocol uses serde_json::Value.
/// Use `to_value()` to convert for publishing.
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
        message_hash: String,
        #[serde(default)]
        comment: Option<String>,
    },
    Dispute {
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

impl Content {
    /// Convert to a serde_json::Value for protocol-level use.
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("Content serialization should not fail")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_to_value_has_type_field() {
        let content = Content::Insight {
            title: "Test".to_string(),
            context: None,
            observation: "obs".to_string(),
            evidence: None,
            guidance: None,
            confidence: Some(0.9),
            tags: vec![],
        };
        let val = content.to_value();
        assert_eq!(val["type"], "insight");
        assert_eq!(val["title"], "Test");
    }

    #[test]
    fn content_value_matches_json_macro() {
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
        assert_eq!(from_enum, from_json);
    }
}
