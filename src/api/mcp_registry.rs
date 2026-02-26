//! MCP Tool Registry with JSON Schema validation.
//!
//! Compiles input schemas at startup for O(1) validation. Provides detailed
//! error messages when inputs fail validation.

use jsonschema::Validator;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

use super::mcp_tools::{tool_definitions, ToolDefinition};

/// Compiled tool registry with pre-built schema validators.
pub struct ToolRegistry {
    tools: HashMap<&'static str, RegisteredTool>,
}

struct RegisteredTool {
    definition: ToolDefinition,
    validator: Validator,
}

/// Tool validation error with context.
#[derive(Debug)]
pub struct ToolValidationError {
    #[allow(dead_code)]
    pub tool: String,
    pub message: String,
}

impl std::fmt::Display for ToolValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ToolValidationError {}

impl ToolRegistry {
    /// Build registry, compiling all tool schemas.
    pub fn new() -> Self {
        let definitions = tool_definitions();
        let mut tools = HashMap::with_capacity(definitions.len());

        for def in definitions {
            let validator = jsonschema::validator_for(&def.input_schema)
                .expect("tool schema must be valid JSON Schema");

            tools.insert(
                def.name,
                RegisteredTool {
                    definition: def,
                    validator,
                },
            );
        }

        Self { tools }
    }

    /// Get tool definition by name.
    #[allow(dead_code)]
    pub fn get(&self, name: &str) -> Option<&ToolDefinition> {
        self.tools.get(name).map(|t| &t.definition)
    }

    /// List all tool definitions.
    pub fn list(&self) -> Vec<&ToolDefinition> {
        self.tools.values().map(|t| &t.definition).collect()
    }

    /// Validate arguments against tool's input schema.
    /// Returns Ok(()) if valid, Err with detailed message if not.
    pub fn validate(&self, name: &str, args: &Value) -> Result<(), ToolValidationError> {
        let tool = self.tools.get(name).ok_or_else(|| ToolValidationError {
            tool: name.to_string(),
            message: format!("Unknown tool: {name}"),
        })?;

        let errors: Vec<String> = tool
            .validator
            .iter_errors(args)
            .map(|e| {
                let path = e.instance_path.to_string();
                let path_str = if path.is_empty() { "root" } else { &path };
                format!("{}: {}", path_str, e)
            })
            .collect();

        if errors.is_empty() {
            Ok(())
        } else {
            Err(ToolValidationError {
                tool: name.to_string(),
                message: errors.join("; "),
            })
        }
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe registry handle.
pub type SharedRegistry = Arc<ToolRegistry>;

pub fn create_registry() -> SharedRegistry {
    Arc::new(ToolRegistry::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_registry() {
        let registry = ToolRegistry::new();
        assert!(registry.get("egregore_status").is_some());
        assert!(registry.get("egregore_publish").is_some());
        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn validates_required_fields() {
        let registry = ToolRegistry::new();

        // publish requires content
        let result = registry.validate("egregore_publish", &serde_json::json!({}));
        assert!(result.is_err());

        let result = registry.validate(
            "egregore_publish",
            &serde_json::json!({ "content": { "type": "test" } }),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn validates_type_constraints() {
        let registry = ToolRegistry::new();

        // limit must be integer
        let result = registry.validate(
            "egregore_query",
            &serde_json::json!({ "limit": "not a number" }),
        );
        assert!(result.is_err());

        let result = registry.validate("egregore_query", &serde_json::json!({ "limit": 10 }));
        assert!(result.is_ok());
    }

    #[test]
    fn empty_args_valid_for_no_required() {
        let registry = ToolRegistry::new();

        // status has no required fields
        let result = registry.validate("egregore_status", &serde_json::json!({}));
        assert!(result.is_ok());
    }

    #[test]
    fn error_message_includes_path() {
        let registry = ToolRegistry::new();

        let result = registry.validate("egregore_publish", &serde_json::json!({}));
        let err = result.unwrap_err();
        assert!(err.message.contains("content"), "Should mention missing field");
    }
}
