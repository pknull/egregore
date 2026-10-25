use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Convenience content types for structured publishing.
/// These are NOT the wire format — the protocol uses serde_json::Value.
/// Use `to_value()` to convert for publishing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionContext {
    Informational,
    Advisory,
    ApprovedDirective,
}

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
        #[serde(default)]
        execution_context: Option<ExecutionContext>,
    },
    Response {
        query_hash: String,
        answer: String,
        #[serde(default)]
        confidence: Option<f64>,
        #[serde(default)]
        execution_context: Option<ExecutionContext>,
    },
    Profile {
        name: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        capabilities: Vec<String>,
        // RFC 0001 §11.2 — both are #[serde(default)] so pre-upgrade
        // Profiles on live feeds continue to deserialize with these fields
        // set to None. Step 11 (startup self-enforce) treats a
        // missing/expired `valid_until` on the *own* feed as "no valid
        // Profile" and auto-publishes. Step 12 (peer-side soft filter)
        // treats a missing `valid_until` on a *peer* feed as undated/accept.
        #[serde(default)]
        valid_from: Option<DateTime<Utc>>,
        #[serde(default)]
        valid_until: Option<DateTime<Utc>>,
    },
    #[serde(rename = "key_rotation")]
    KeyRotation {
        old_key: String,
        new_key: String,
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        effective_at: Option<String>,
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
    fn key_rotation_to_value_has_type_field() {
        let content = Content::KeyRotation {
            old_key: "@old.ed25519".to_string(),
            new_key: "@new.ed25519".to_string(),
            reason: Some("scheduled rotation".to_string()),
            effective_at: Some("2026-04-01T00:00:00Z".to_string()),
        };
        let val = content.to_value();
        assert_eq!(val["type"], "key_rotation");
        assert_eq!(val["old_key"], "@old.ed25519");
        assert_eq!(val["new_key"], "@new.ed25519");
        assert_eq!(val["reason"], "scheduled rotation");
    }

    #[test]
    fn content_value_matches_json_macro() {
        let content = Content::Profile {
            name: "test".to_string(),
            description: None,
            capabilities: vec![],
            valid_from: None,
            valid_until: None,
        };
        let from_enum = content.to_value();
        let from_json = serde_json::json!({
            "type": "profile",
            "name": "test",
            "description": null,
            "capabilities": [],
            "valid_from": null,
            "valid_until": null,
        });
        assert_eq!(from_enum, from_json);
    }

    // --- Phase 1 Step 10: Profile schema extension tests ---

    /// Backward-compat: Profile JSON from pre-upgrade feeds (lacking the
    /// `valid_from` and `valid_until` keys entirely) must deserialize cleanly
    /// with both fields as `None`. Load-bearing: guards live feeds.
    #[test]
    fn profile_backward_compat_parses_old_wire_format() {
        let old_wire = serde_json::json!({
            "type": "profile",
            "name": "legacy-node",
            "description": "pre-upgrade peer",
            "capabilities": ["insight", "endorsement"],
        });
        let parsed: Content = serde_json::from_value(old_wire)
            .expect("pre-upgrade Profile JSON must still deserialize");
        match parsed {
            Content::Profile {
                name,
                description,
                capabilities,
                valid_from,
                valid_until,
            } => {
                assert_eq!(name, "legacy-node");
                assert_eq!(description.as_deref(), Some("pre-upgrade peer"));
                assert_eq!(
                    capabilities,
                    vec!["insight".to_string(), "endorsement".to_string()]
                );
                assert!(
                    valid_from.is_none(),
                    "missing valid_from key must deserialize as None"
                );
                assert!(
                    valid_until.is_none(),
                    "missing valid_until key must deserialize as None"
                );
            }
            other => panic!("expected Profile variant, got {:?}", other),
        }
    }

    /// Forward-compat: a Profile with all new fields populated roundtrips
    /// cleanly through to_value() -> from_value().
    #[test]
    fn profile_forward_compat_roundtrips_with_all_fields() {
        let now = Utc::now();
        let later = now + chrono::Duration::days(90);
        let original = Content::Profile {
            name: "full-node".to_string(),
            description: Some("phase-2 capable".to_string()),
            capabilities: vec!["insight".to_string()],
            valid_from: Some(now),
            valid_until: Some(later),
        };

        let value = original.to_value();
        // Tag is at "type" (outer enum tag from #[serde(tag = "type")]).
        assert_eq!(value["type"], "profile");
        assert_eq!(value["name"], "full-node");

        let parsed: Content =
            serde_json::from_value(value).expect("forward-compat roundtrip must succeed");
        match parsed {
            Content::Profile {
                name,
                description,
                capabilities,
                valid_from,
                valid_until,
            } => {
                assert_eq!(name, "full-node");
                assert_eq!(description.as_deref(), Some("phase-2 capable"));
                assert_eq!(capabilities, vec!["insight".to_string()]);
                assert_eq!(valid_from, Some(now));
                assert_eq!(valid_until, Some(later));
            }
            other => panic!("expected Profile variant, got {:?}", other),
        }
    }
}
