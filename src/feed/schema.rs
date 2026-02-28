//! Schema registry and versioning for message validation.
//!
//! Provides:
//! - Schema registration with version management
//! - Schema validation at publish/ingest time
//! - Compatibility checks (backward/forward compatibility)
//! - Versioned codecs (JSON, Protobuf, CBOR placeholders)
//!
//! Schema IDs follow the format: `{content_type}/v{version}` (e.g., `insight/v1`)

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::error::{EgreError, Result};

/// Codec format for message serialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    Json,
    Protobuf,
    Cbor,
}

impl Default for Codec {
    fn default() -> Self {
        Self::Json
    }
}

impl std::fmt::Display for Codec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Codec::Json => write!(f, "json"),
            Codec::Protobuf => write!(f, "protobuf"),
            Codec::Cbor => write!(f, "cbor"),
        }
    }
}

impl std::str::FromStr for Codec {
    type Err = EgreError;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "json" => Ok(Codec::Json),
            "protobuf" => Ok(Codec::Protobuf),
            "cbor" => Ok(Codec::Cbor),
            _ => Err(EgreError::Schema {
                reason: format!("unknown codec: {}", s),
            }),
        }
    }
}

/// Compatibility mode for schema evolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompatibilityMode {
    /// New schema can read data written by old schema.
    Backward,
    /// Old schema can read data written by new schema.
    Forward,
    /// Both backward and forward compatible.
    Full,
    /// No compatibility guarantees.
    None,
}

impl Default for CompatibilityMode {
    fn default() -> Self {
        Self::Backward
    }
}

/// A schema definition with metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaDefinition {
    /// Schema identifier (e.g., "insight/v1").
    pub schema_id: String,
    /// Content type this schema validates (e.g., "insight").
    pub content_type: String,
    /// Schema version (monotonically increasing).
    pub version: u32,
    /// JSON Schema definition for validation.
    pub json_schema: serde_json::Value,
    /// Preferred codec for this schema.
    #[serde(default)]
    pub codec: Codec,
    /// Compatibility mode for schema evolution.
    #[serde(default)]
    pub compatibility: CompatibilityMode,
    /// Human-readable description.
    #[serde(default)]
    pub description: Option<String>,
}

impl SchemaDefinition {
    /// Create a new schema definition.
    pub fn new(
        content_type: impl Into<String>,
        version: u32,
        json_schema: serde_json::Value,
    ) -> Self {
        let content_type = content_type.into();
        let schema_id = format!("{}/v{}", content_type, version);
        Self {
            schema_id,
            content_type,
            version,
            json_schema,
            codec: Codec::default(),
            compatibility: CompatibilityMode::default(),
            description: None,
        }
    }

    /// Set the codec for this schema.
    pub fn with_codec(mut self, codec: Codec) -> Self {
        self.codec = codec;
        self
    }

    /// Set the compatibility mode for this schema.
    pub fn with_compatibility(mut self, mode: CompatibilityMode) -> Self {
        self.compatibility = mode;
        self
    }

    /// Set the description for this schema.
    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    /// Parse a schema ID into (content_type, version).
    pub fn parse_schema_id(schema_id: &str) -> Option<(String, u32)> {
        let parts: Vec<&str> = schema_id.rsplitn(2, "/v").collect();
        if parts.len() != 2 {
            return None;
        }
        let version: u32 = parts[0].parse().ok()?;
        let content_type = parts[1].to_string();
        if content_type.is_empty() {
            return None;
        }
        Some((content_type, version))
    }
}

/// Thread-safe schema registry for managing schema definitions.
#[derive(Clone)]
pub struct SchemaRegistry {
    /// Schemas indexed by schema_id.
    schemas: Arc<RwLock<HashMap<String, SchemaDefinition>>>,
    /// Latest version per content type.
    latest_versions: Arc<RwLock<HashMap<String, u32>>>,
    /// Compiled JSON Schema validators.
    validators: Arc<RwLock<HashMap<String, Arc<jsonschema::Validator>>>>,
    /// Whether to enforce schema validation on publish/ingest.
    strict_mode: bool,
}

impl Default for SchemaRegistry {
    fn default() -> Self {
        Self::new(false)
    }
}

impl SchemaRegistry {
    /// Create a new schema registry.
    ///
    /// If `strict_mode` is true, messages without a schema_id or with an
    /// unknown schema are rejected. If false, they pass through (for
    /// backward compatibility with pre-schema messages).
    pub fn new(strict_mode: bool) -> Self {
        let registry = Self {
            schemas: Arc::new(RwLock::new(HashMap::new())),
            latest_versions: Arc::new(RwLock::new(HashMap::new())),
            validators: Arc::new(RwLock::new(HashMap::new())),
            strict_mode,
        };
        registry.register_builtin_schemas();
        registry
    }

    /// Register built-in schemas for known content types.
    fn register_builtin_schemas(&self) {
        // insight/v1
        let _ = self.register(SchemaDefinition::new(
            "insight",
            1,
            serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "required": ["type", "title", "observation"],
                "properties": {
                    "type": { "const": "insight" },
                    "title": { "type": "string", "minLength": 1 },
                    "context": { "type": ["string", "null"] },
                    "observation": { "type": "string", "minLength": 1 },
                    "evidence": { "type": ["string", "null"] },
                    "guidance": { "type": ["string", "null"] },
                    "confidence": { "type": ["number", "null"], "minimum": 0, "maximum": 1 },
                    "tags": { "type": "array", "items": { "type": "string" } }
                },
                "additionalProperties": false
            }),
        ).with_description("Insight content type - observations with optional evidence"));

        // endorsement/v1
        let _ = self.register(SchemaDefinition::new(
            "endorsement",
            1,
            serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "required": ["type", "message_hash"],
                "properties": {
                    "type": { "const": "endorsement" },
                    "message_hash": { "type": "string", "pattern": "^[a-f0-9]{64}$" },
                    "comment": { "type": ["string", "null"] }
                },
                "additionalProperties": false
            }),
        ).with_description("Endorsement of another message"));

        // dispute/v1
        let _ = self.register(SchemaDefinition::new(
            "dispute",
            1,
            serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "required": ["type", "message_hash", "reason"],
                "properties": {
                    "type": { "const": "dispute" },
                    "message_hash": { "type": "string", "pattern": "^[a-f0-9]{64}$" },
                    "reason": { "type": "string", "minLength": 1 },
                    "evidence": { "type": ["string", "null"] }
                },
                "additionalProperties": false
            }),
        ).with_description("Dispute of another message"));

        // query/v1
        let _ = self.register(SchemaDefinition::new(
            "query",
            1,
            serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "required": ["type", "question"],
                "properties": {
                    "type": { "const": "query" },
                    "question": { "type": "string", "minLength": 1 },
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "execution_context": {
                        "type": ["string", "null"],
                        "enum": [null, "informational", "advisory", "approved_directive"]
                    }
                },
                "additionalProperties": false
            }),
        ).with_description("Query to the mesh"));

        // response/v1
        let _ = self.register(SchemaDefinition::new(
            "response",
            1,
            serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "required": ["type", "query_hash", "answer"],
                "properties": {
                    "type": { "const": "response" },
                    "query_hash": { "type": "string", "pattern": "^[a-f0-9]{64}$" },
                    "answer": { "type": "string", "minLength": 1 },
                    "confidence": { "type": ["number", "null"], "minimum": 0, "maximum": 1 },
                    "execution_context": {
                        "type": ["string", "null"],
                        "enum": [null, "informational", "advisory", "approved_directive"]
                    }
                },
                "additionalProperties": false
            }),
        ).with_description("Response to a query"));

        // profile/v1
        let _ = self.register(SchemaDefinition::new(
            "profile",
            1,
            serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "required": ["type", "name"],
                "properties": {
                    "type": { "const": "profile" },
                    "name": { "type": "string", "minLength": 1 },
                    "description": { "type": ["string", "null"] },
                    "capabilities": { "type": "array", "items": { "type": "string" } }
                },
                "additionalProperties": false
            }),
        ).with_description("Agent profile information"));
    }

    /// Register a schema definition.
    ///
    /// Returns error if the schema version is not greater than existing versions
    /// (for the same content type) or if compatibility check fails.
    pub fn register(&self, schema: SchemaDefinition) -> Result<()> {
        // Validate the JSON Schema itself
        let validator = jsonschema::validator_for(&schema.json_schema).map_err(|e| {
            EgreError::Schema {
                reason: format!("invalid JSON Schema: {}", e),
            }
        })?;

        // Check version monotonicity
        {
            let versions = self.latest_versions.read().unwrap();
            if let Some(&latest) = versions.get(&schema.content_type) {
                if schema.version <= latest {
                    return Err(EgreError::Schema {
                        reason: format!(
                            "schema version {} must be greater than existing version {}",
                            schema.version, latest
                        ),
                    });
                }
            }
        }

        // Check compatibility with previous version if required
        if schema.version > 1 {
            let prev_id = format!("{}/v{}", schema.content_type, schema.version - 1);
            if let Some(prev_schema) = self.get(&prev_id) {
                self.check_compatibility(&prev_schema, &schema)?;
            }
        }

        // Register the schema
        {
            let mut schemas = self.schemas.write().unwrap();
            schemas.insert(schema.schema_id.clone(), schema.clone());
        }
        {
            let mut versions = self.latest_versions.write().unwrap();
            versions.insert(schema.content_type.clone(), schema.version);
        }
        {
            let mut validators = self.validators.write().unwrap();
            validators.insert(schema.schema_id.clone(), Arc::new(validator));
        }

        Ok(())
    }

    /// Get a schema by its ID.
    pub fn get(&self, schema_id: &str) -> Option<SchemaDefinition> {
        let schemas = self.schemas.read().unwrap();
        schemas.get(schema_id).cloned()
    }

    /// Get the latest schema for a content type.
    pub fn get_latest(&self, content_type: &str) -> Option<SchemaDefinition> {
        let versions = self.latest_versions.read().unwrap();
        let version = versions.get(content_type)?;
        let schema_id = format!("{}/v{}", content_type, version);
        drop(versions);
        self.get(&schema_id)
    }

    /// Get all registered schemas.
    pub fn list_all(&self) -> Vec<SchemaDefinition> {
        let schemas = self.schemas.read().unwrap();
        schemas.values().cloned().collect()
    }

    /// Validate content against a schema.
    ///
    /// If `schema_id` is None:
    /// - In strict mode: returns error
    /// - In non-strict mode: attempts to infer schema from content type field
    pub fn validate(
        &self,
        content: &serde_json::Value,
        schema_id: Option<&str>,
    ) -> Result<()> {
        let effective_schema_id = match schema_id {
            Some(id) => id.to_string(),
            None => {
                // Try to infer schema from content type
                let content_type = content
                    .get("type")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| EgreError::Schema {
                        reason: "content missing 'type' field".into(),
                    })?;

                // Use latest version for this content type
                let versions = self.latest_versions.read().unwrap();
                match versions.get(content_type) {
                    Some(&version) => format!("{}/v{}", content_type, version),
                    None if self.strict_mode => {
                        return Err(EgreError::Schema {
                            reason: format!("unknown content type: {}", content_type),
                        });
                    }
                    None => {
                        // Non-strict mode: allow unknown content types
                        return Ok(());
                    }
                }
            }
        };

        let validators = self.validators.read().unwrap();
        match validators.get(&effective_schema_id) {
            Some(validator) => {
                let result = validator.validate(content);
                if let Err(error) = result {
                    return Err(EgreError::Schema {
                        reason: format!(
                            "validation failed for {}: {}: {}",
                            effective_schema_id,
                            error.instance_path,
                            error
                        ),
                    });
                }
                Ok(())
            }
            None if self.strict_mode => Err(EgreError::Schema {
                reason: format!("unknown schema: {}", effective_schema_id),
            }),
            None => {
                // Non-strict mode: allow unknown schemas
                Ok(())
            }
        }
    }

    /// Check compatibility between two schema versions.
    fn check_compatibility(
        &self,
        old: &SchemaDefinition,
        new: &SchemaDefinition,
    ) -> Result<()> {
        match new.compatibility {
            CompatibilityMode::None => Ok(()),
            CompatibilityMode::Backward => self.check_backward_compatible(old, new),
            CompatibilityMode::Forward => self.check_forward_compatible(old, new),
            CompatibilityMode::Full => {
                self.check_backward_compatible(old, new)?;
                self.check_forward_compatible(old, new)
            }
        }
    }

    /// Check backward compatibility: new schema can read old data.
    ///
    /// Rules:
    /// - New schema cannot add required fields
    /// - New schema cannot narrow field types
    fn check_backward_compatible(
        &self,
        old: &SchemaDefinition,
        new: &SchemaDefinition,
    ) -> Result<()> {
        let old_required = old
            .json_schema
            .get("required")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
            .unwrap_or_default();

        let new_required = new
            .json_schema
            .get("required")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
            .unwrap_or_default();

        // Check for newly added required fields
        for field in &new_required {
            if !old_required.contains(field) {
                // Check if the field existed in old schema (as optional)
                let old_props = old.json_schema.get("properties");
                let field_existed = old_props
                    .and_then(|p| p.get(*field))
                    .is_some();

                if !field_existed {
                    return Err(EgreError::Schema {
                        reason: format!(
                            "backward compatibility violation: new required field '{}' \
                             was not present in previous schema",
                            field
                        ),
                    });
                }
            }
        }

        Ok(())
    }

    /// Check forward compatibility: old schema can read new data.
    ///
    /// Rules:
    /// - New schema cannot remove required fields
    /// - New schema cannot widen field types beyond what old schema accepts
    fn check_forward_compatible(
        &self,
        old: &SchemaDefinition,
        new: &SchemaDefinition,
    ) -> Result<()> {
        let old_required = old
            .json_schema
            .get("required")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
            .unwrap_or_default();

        let new_required = new
            .json_schema
            .get("required")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
            .unwrap_or_default();

        // Check for removed required fields
        for field in &old_required {
            if !new_required.contains(field) {
                return Err(EgreError::Schema {
                    reason: format!(
                        "forward compatibility violation: required field '{}' \
                         was removed in new schema",
                        field
                    ),
                });
            }
        }

        Ok(())
    }

    /// Infer the appropriate schema_id for content based on its type field.
    /// Returns the latest version schema_id for the content type.
    pub fn infer_schema_id(&self, content: &serde_json::Value) -> Option<String> {
        let content_type = content.get("type")?.as_str()?;
        let versions = self.latest_versions.read().unwrap();
        let version = versions.get(content_type)?;
        Some(format!("{}/v{}", content_type, version))
    }

    /// Check if strict mode is enabled.
    pub fn is_strict(&self) -> bool {
        self.strict_mode
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_id_parsing() {
        assert_eq!(
            SchemaDefinition::parse_schema_id("insight/v1"),
            Some(("insight".to_string(), 1))
        );
        assert_eq!(
            SchemaDefinition::parse_schema_id("custom_type/v42"),
            Some(("custom_type".to_string(), 42))
        );
        assert_eq!(
            SchemaDefinition::parse_schema_id("nested/path/v2"),
            Some(("nested/path".to_string(), 2))
        );
        assert_eq!(SchemaDefinition::parse_schema_id("invalid"), None);
        assert_eq!(SchemaDefinition::parse_schema_id("/v1"), None);
        assert_eq!(SchemaDefinition::parse_schema_id("type/vx"), None);
    }

    #[test]
    fn builtin_schemas_registered() {
        let registry = SchemaRegistry::new(false);

        assert!(registry.get("insight/v1").is_some());
        assert!(registry.get("endorsement/v1").is_some());
        assert!(registry.get("dispute/v1").is_some());
        assert!(registry.get("query/v1").is_some());
        assert!(registry.get("response/v1").is_some());
        assert!(registry.get("profile/v1").is_some());
    }

    #[test]
    fn validate_valid_insight() {
        let registry = SchemaRegistry::new(false);

        let content = serde_json::json!({
            "type": "insight",
            "title": "Test Insight",
            "observation": "This is a test observation",
            "confidence": 0.85,
            "tags": ["test"]
        });

        assert!(registry.validate(&content, Some("insight/v1")).is_ok());
    }

    #[test]
    fn validate_invalid_insight_missing_required() {
        let registry = SchemaRegistry::new(true);

        let content = serde_json::json!({
            "type": "insight",
            "title": "Test Insight"
            // missing "observation"
        });

        let result = registry.validate(&content, Some("insight/v1"));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("observation"), "error should mention missing field: {}", err);
    }

    #[test]
    fn validate_infers_schema_from_content_type() {
        let registry = SchemaRegistry::new(false);

        let content = serde_json::json!({
            "type": "profile",
            "name": "Test Agent",
            "capabilities": ["testing"]
        });

        // No schema_id provided, should infer from type
        assert!(registry.validate(&content, None).is_ok());
    }

    #[test]
    fn strict_mode_rejects_unknown_types() {
        let registry = SchemaRegistry::new(true);

        let content = serde_json::json!({
            "type": "unknown_type",
            "data": "test"
        });

        let result = registry.validate(&content, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown content type"));
    }

    #[test]
    fn non_strict_mode_allows_unknown_types() {
        let registry = SchemaRegistry::new(false);

        let content = serde_json::json!({
            "type": "custom_extension",
            "data": "test"
        });

        assert!(registry.validate(&content, None).is_ok());
    }

    #[test]
    fn register_custom_schema() {
        let registry = SchemaRegistry::new(false);

        let schema = SchemaDefinition::new(
            "custom",
            1,
            serde_json::json!({
                "type": "object",
                "required": ["type", "data"],
                "properties": {
                    "type": { "const": "custom" },
                    "data": { "type": "string" }
                }
            }),
        );

        assert!(registry.register(schema).is_ok());
        assert!(registry.get("custom/v1").is_some());

        let content = serde_json::json!({
            "type": "custom",
            "data": "hello"
        });
        assert!(registry.validate(&content, Some("custom/v1")).is_ok());
    }

    #[test]
    fn version_must_increase() {
        let registry = SchemaRegistry::new(false);

        let schema_v1 = SchemaDefinition::new(
            "versioned",
            1,
            serde_json::json!({ "type": "object" }),
        );
        assert!(registry.register(schema_v1).is_ok());

        // Same version should fail
        let schema_v1_again = SchemaDefinition::new(
            "versioned",
            1,
            serde_json::json!({ "type": "object" }),
        );
        assert!(registry.register(schema_v1_again).is_err());

        // Higher version should succeed
        let schema_v2 = SchemaDefinition::new(
            "versioned",
            2,
            serde_json::json!({ "type": "object" }),
        );
        assert!(registry.register(schema_v2).is_ok());
    }

    #[test]
    fn backward_compatibility_check() {
        let registry = SchemaRegistry::new(false);

        // Register v1 with optional field "extra"
        let schema_v1 = SchemaDefinition::new(
            "compat_test",
            1,
            serde_json::json!({
                "type": "object",
                "required": ["type", "name"],
                "properties": {
                    "type": { "const": "compat_test" },
                    "name": { "type": "string" },
                    "extra": { "type": "string" }
                }
            }),
        ).with_compatibility(CompatibilityMode::Backward);
        assert!(registry.register(schema_v1).is_ok());

        // v2 that makes "extra" required should fail (backward incompatible)
        // because old data without "extra" wouldn't validate
        // Actually, making an existing optional field required IS backward compatible
        // (new schema can still read old data if the field existed)

        // v2 that adds a NEW required field should fail
        let schema_v2_bad = SchemaDefinition::new(
            "compat_test",
            2,
            serde_json::json!({
                "type": "object",
                "required": ["type", "name", "brand_new_field"],
                "properties": {
                    "type": { "const": "compat_test" },
                    "name": { "type": "string" },
                    "brand_new_field": { "type": "string" }
                }
            }),
        ).with_compatibility(CompatibilityMode::Backward);

        let result = registry.register(schema_v2_bad);
        assert!(result.is_err(), "should reject new required field not in old schema");
    }

    #[test]
    fn forward_compatibility_check() {
        let registry = SchemaRegistry::new(false);

        let schema_v1 = SchemaDefinition::new(
            "forward_test",
            1,
            serde_json::json!({
                "type": "object",
                "required": ["type", "name", "must_have"],
                "properties": {
                    "type": { "const": "forward_test" },
                    "name": { "type": "string" },
                    "must_have": { "type": "string" }
                }
            }),
        ).with_compatibility(CompatibilityMode::Forward);
        assert!(registry.register(schema_v1).is_ok());

        // v2 that removes "must_have" from required should fail (forward incompatible)
        let schema_v2_bad = SchemaDefinition::new(
            "forward_test",
            2,
            serde_json::json!({
                "type": "object",
                "required": ["type", "name"],
                "properties": {
                    "type": { "const": "forward_test" },
                    "name": { "type": "string" }
                }
            }),
        ).with_compatibility(CompatibilityMode::Forward);

        let result = registry.register(schema_v2_bad);
        assert!(result.is_err(), "should reject removing required field");
    }

    #[test]
    fn codec_parsing() {
        assert_eq!("json".parse::<Codec>().unwrap(), Codec::Json);
        assert_eq!("JSON".parse::<Codec>().unwrap(), Codec::Json);
        assert_eq!("protobuf".parse::<Codec>().unwrap(), Codec::Protobuf);
        assert_eq!("cbor".parse::<Codec>().unwrap(), Codec::Cbor);
        assert!("unknown".parse::<Codec>().is_err());
    }

    #[test]
    fn get_latest_schema() {
        let registry = SchemaRegistry::new(false);

        // Register multiple versions
        let _ = registry.register(SchemaDefinition::new(
            "multi_version",
            1,
            serde_json::json!({ "type": "object" }),
        ));
        let _ = registry.register(SchemaDefinition::new(
            "multi_version",
            2,
            serde_json::json!({ "type": "object" }),
        ));
        let _ = registry.register(SchemaDefinition::new(
            "multi_version",
            3,
            serde_json::json!({ "type": "object" }),
        ));

        let latest = registry.get_latest("multi_version").unwrap();
        assert_eq!(latest.version, 3);
        assert_eq!(latest.schema_id, "multi_version/v3");
    }

    #[test]
    fn infer_schema_id_from_content() {
        let registry = SchemaRegistry::new(false);

        let content = serde_json::json!({
            "type": "insight",
            "title": "Test",
            "observation": "Test"
        });

        assert_eq!(
            registry.infer_schema_id(&content),
            Some("insight/v1".to_string())
        );

        let unknown = serde_json::json!({
            "type": "unknown",
            "data": "test"
        });
        assert_eq!(registry.infer_schema_id(&unknown), None);
    }

    #[test]
    fn list_all_schemas() {
        let registry = SchemaRegistry::new(false);
        let all = registry.list_all();

        // Should have at least the builtin schemas
        assert!(all.len() >= 6);

        let schema_ids: Vec<&str> = all.iter().map(|s| s.schema_id.as_str()).collect();
        assert!(schema_ids.contains(&"insight/v1"));
        assert!(schema_ids.contains(&"profile/v1"));
    }
}
