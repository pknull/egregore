use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Broker operator disclosure (RFC 0001 §11.2). When a Profile advertises
/// broker details, all fields are required — a partial disclosure is
/// meaningless. Phase-1 gossip-only deployments leave `Content::Profile.broker`
/// as `None`; Phase-2 bus/bridge deployments populate it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrokerDetails {
    pub operator_name: String,
    pub jurisdiction: String,
    pub disclosure_policy: String,
    pub tenancy: String,
    pub broker_endpoint: String,
    pub backend: String,
}

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
        // NEW (RFC 0001 §11.2) — all three are #[serde(default)] so
        // pre-upgrade Profiles on live feeds continue to deserialize with
        // these fields set to None. Step 11 (startup self-enforce) treats
        // a missing/expired `valid_until` on the *own* feed as "no valid
        // Profile" and auto-publishes. Step 12 (peer-side soft filter)
        // treats a missing `valid_until` on a *peer* feed as undated/accept.
        #[serde(default)]
        broker: Option<BrokerDetails>,
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
            broker: None,
            valid_from: None,
            valid_until: None,
        };
        let from_enum = content.to_value();
        let from_json = serde_json::json!({
            "type": "profile",
            "name": "test",
            "description": null,
            "capabilities": [],
            "broker": null,
            "valid_from": null,
            "valid_until": null,
        });
        assert_eq!(from_enum, from_json);
    }

    // --- Phase 1 Step 10: Profile schema extension tests ---

    /// Backward-compat: Profile JSON from pre-upgrade feeds (lacking `broker`,
    /// `valid_from`, `valid_until` keys entirely) must deserialize cleanly with
    /// all three new fields as `None`. Load-bearing: guards live feeds.
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
                broker,
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
                    broker.is_none(),
                    "missing broker key must deserialize as None"
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
            broker: Some(BrokerDetails {
                operator_name: "Acme Ops".to_string(),
                jurisdiction: "US-DE".to_string(),
                disclosure_policy: "rfc-0002-disclosure-v1".to_string(),
                tenancy: "dedicated".to_string(),
                broker_endpoint: "nats://broker.example:4222".to_string(),
                backend: "nats".to_string(),
            }),
            valid_from: Some(now),
            valid_until: Some(later),
        };

        let value = original.to_value();
        // Tag is at "type" (outer enum tag from #[serde(tag = "type")]).
        assert_eq!(value["type"], "profile");
        assert_eq!(value["name"], "full-node");
        assert_eq!(value["broker"]["operator_name"], "Acme Ops");
        assert_eq!(value["broker"]["backend"], "nats");

        let parsed: Content =
            serde_json::from_value(value).expect("forward-compat roundtrip must succeed");
        match parsed {
            Content::Profile {
                name,
                description,
                capabilities,
                broker,
                valid_from,
                valid_until,
            } => {
                assert_eq!(name, "full-node");
                assert_eq!(description.as_deref(), Some("phase-2 capable"));
                assert_eq!(capabilities, vec!["insight".to_string()]);
                let b = broker.expect("broker must survive roundtrip");
                assert_eq!(b.operator_name, "Acme Ops");
                assert_eq!(b.jurisdiction, "US-DE");
                assert_eq!(b.disclosure_policy, "rfc-0002-disclosure-v1");
                assert_eq!(b.tenancy, "dedicated");
                assert_eq!(b.broker_endpoint, "nats://broker.example:4222");
                assert_eq!(b.backend, "nats");
                assert_eq!(valid_from, Some(now));
                assert_eq!(valid_until, Some(later));
            }
            other => panic!("expected Profile variant, got {:?}", other),
        }
    }

    /// Missing-broker serialization: a Profile with `broker: None` but
    /// populated `valid_from`/`valid_until` emits `"broker": null` in JSON
    /// (matching the existing `description: None` behavior at line 118 above —
    /// Option<T> with #[serde(default)] serializes as JSON null, not absent).
    #[test]
    fn profile_with_none_broker_serializes_as_json_null() {
        let ts1 = Utc::now();
        let ts2 = ts1 + chrono::Duration::days(90);
        let content = Content::Profile {
            name: "dated-no-broker".to_string(),
            description: None,
            capabilities: vec![],
            broker: None,
            valid_from: Some(ts1),
            valid_until: Some(ts2),
        };
        let val = content.to_value();
        assert_eq!(
            val["broker"],
            serde_json::Value::Null,
            "None broker must serialize as JSON null (consistent with Option<String> description)"
        );
        assert!(
            val["valid_from"].is_string(),
            "valid_from must serialize as ISO-8601 string"
        );
        assert!(
            val["valid_until"].is_string(),
            "valid_until must serialize as ISO-8601 string"
        );
    }

    /// BrokerDetails: standalone struct roundtrip — every field survives
    /// serialize + deserialize with byte-identical values.
    #[test]
    fn broker_details_roundtrips_all_fields() {
        let original = BrokerDetails {
            operator_name: "Operator X".to_string(),
            jurisdiction: "EU-DE".to_string(),
            disclosure_policy: "public-v2".to_string(),
            tenancy: "shared".to_string(),
            broker_endpoint: "wss://bus.example.net/nats".to_string(),
            backend: "nats".to_string(),
        };
        let value = serde_json::to_value(&original).expect("serialize BrokerDetails");
        assert_eq!(value["operator_name"], "Operator X");
        assert_eq!(value["jurisdiction"], "EU-DE");
        assert_eq!(value["disclosure_policy"], "public-v2");
        assert_eq!(value["tenancy"], "shared");
        assert_eq!(value["broker_endpoint"], "wss://bus.example.net/nats");
        assert_eq!(value["backend"], "nats");

        let parsed: BrokerDetails = serde_json::from_value(value).expect("BrokerDetails roundtrip");
        assert_eq!(parsed.operator_name, original.operator_name);
        assert_eq!(parsed.jurisdiction, original.jurisdiction);
        assert_eq!(parsed.disclosure_policy, original.disclosure_policy);
        assert_eq!(parsed.tenancy, original.tenancy);
        assert_eq!(parsed.broker_endpoint, original.broker_endpoint);
        assert_eq!(parsed.backend, original.backend);
    }
}
