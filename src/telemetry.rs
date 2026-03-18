//! Telemetry initialization and trace context utilities.
//!
//! Provides configurable logging format (JSON or pretty) with trace correlation.
//! JSON format is recommended for production (machine-readable, searchable).
//! Pretty format is useful for local development (human-readable).
//!
//! ## Trace Context
//!
//! Trace IDs and span IDs enable cross-component correlation:
//! - `trace_id`: Unique identifier for an end-to-end request flow
//! - `span_id`: Identifier for a specific operation within the trace
//!
//! ## OTLP Export (Optional)
//!
//! When built with the `otlp` feature, traces can be exported to an OpenTelemetry
//! collector via OTLP/gRPC. Enable via config:
//!
//! ```yaml
//! telemetry:
//!   otlp:
//!     enabled: true
//!     endpoint: "http://localhost:4317"
//!     service_name: "egregore"
//! ```
//!
//! ## Security
//!
//! This module follows the sanitization rules from component-model.md:
//! - Never log secrets (API keys, passwords, tokens, private keys)
//! - Redact fields matching `*key*`, `*secret*`, `*password*`, `*token*`, `*credential*`
//! - Truncate large payloads (>1KB)

use serde::{Deserialize, Serialize};
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::EnvFilter;

#[cfg(feature = "otlp")]
use tracing_subscriber::layer::SubscriberExt;
#[cfg(feature = "otlp")]
use tracing_subscriber::util::SubscriberInitExt;

/// Logging output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// JSON format for machine consumption (production).
    #[default]
    Json,
    /// Pretty format for human consumption (development).
    Pretty,
}

impl std::fmt::Display for LogFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogFormat::Json => write!(f, "json"),
            LogFormat::Pretty => write!(f, "pretty"),
        }
    }
}

impl std::str::FromStr for LogFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "json" => Ok(LogFormat::Json),
            "pretty" => Ok(LogFormat::Pretty),
            other => Err(format!("unknown log format: {}", other)),
        }
    }
}

/// Logging configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Output format (json or pretty).
    #[serde(default)]
    pub format: LogFormat,
    /// Log level filter (trace, debug, info, warn, error).
    #[serde(default = "default_level")]
    pub level: String,
    /// Include trace_id and span_id in log output.
    #[serde(default = "default_include_trace_ids")]
    pub include_trace_ids: bool,
}

fn default_level() -> String {
    "info".to_string()
}

fn default_include_trace_ids() -> bool {
    true
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            format: LogFormat::Json,
            level: default_level(),
            include_trace_ids: default_include_trace_ids(),
        }
    }
}

/// OTLP trace export configuration.
///
/// When enabled, spans are exported to an OpenTelemetry collector via OTLP/gRPC.
/// Requires the `otlp` feature to be enabled at compile time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtlpConfig {
    /// Enable OTLP trace export.
    #[serde(default)]
    pub enabled: bool,
    /// OTLP collector endpoint (gRPC).
    /// Must be a valid URL with http, https, or grpc scheme.
    #[serde(default = "default_otlp_endpoint")]
    pub endpoint: String,
    /// Service name for OpenTelemetry semantic conventions.
    #[serde(default = "default_service_name")]
    pub service_name: String,
    /// Trace sampling rate (0.0 to 1.0). Default: 1.0 (all traces).
    /// Set lower for high-throughput production deployments.
    #[serde(default = "default_sample_rate")]
    pub sample_rate: f64,
}

fn default_otlp_endpoint() -> String {
    "http://localhost:4317".to_string()
}

fn default_service_name() -> String {
    "egregore".to_string()
}

fn default_sample_rate() -> f64 {
    1.0
}

impl Default for OtlpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: default_otlp_endpoint(),
            service_name: default_service_name(),
            sample_rate: default_sample_rate(),
        }
    }
}

/// Validate OTLP endpoint URL for security.
/// Returns Ok(()) if valid, Err with description if invalid.
#[cfg(feature = "otlp")]
fn validate_otlp_endpoint(endpoint: &str) -> Result<(), String> {
    let url = url::Url::parse(endpoint).map_err(|e| format!("Invalid OTLP endpoint URL: {}", e))?;

    // Validate scheme
    match url.scheme() {
        "http" | "https" | "grpc" => {}
        other => {
            return Err(format!(
                "Unsupported OTLP scheme '{}'. Use http, https, or grpc.",
                other
            ))
        }
    }

    // Validate host exists
    if url.host_str().is_none() {
        return Err("OTLP endpoint must have a host".to_string());
    }

    Ok(())
}

/// Combined telemetry configuration (logging + OTLP).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TelemetryConfig {
    /// Logging configuration.
    #[serde(default)]
    pub logging: LoggingConfig,
    /// OTLP trace export configuration.
    #[serde(default)]
    pub otlp: OtlpConfig,
}

/// Initialize the tracing subscriber with the given configuration.
///
/// This should be called early in main() before any tracing macros are used.
/// The subscriber is installed globally; calling this multiple times will panic.
///
/// Note: If OTLP export is enabled, use `init_with_otlp()` instead.
pub fn init(config: &LoggingConfig) {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("egregore={}", config.level)));

    match config.format {
        LogFormat::Json => {
            let subscriber = tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .json()
                .with_span_events(FmtSpan::CLOSE)
                .with_current_span(config.include_trace_ids)
                .with_span_list(false)
                .flatten_event(true)
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .expect("failed to set tracing subscriber");
        }
        LogFormat::Pretty => {
            let subscriber = tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_span_events(FmtSpan::CLOSE)
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .expect("failed to set tracing subscriber");
        }
    }
}

/// Initialize the tracing subscriber with OTLP export.
///
/// This sets up both the logging subscriber and an OpenTelemetry layer for
/// exporting traces to an OTLP collector. Requires the `otlp` feature.
///
/// The tracer uses these OpenTelemetry semantic conventions:
/// - `service.name` = configured service_name (default: "egregore")
/// - `service.version` = crate version from Cargo.toml
/// - `messaging.system` = "egregore"
///
/// # Errors
///
/// Returns an error if the OTLP endpoint is invalid or exporter fails to initialize.
#[cfg(feature = "otlp")]
pub fn init_with_otlp(
    logging_config: &LoggingConfig,
    otlp_config: &OtlpConfig,
) -> Result<(), OtlpInitError> {
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry::{global, KeyValue};
    use opentelemetry_otlp::WithExportConfig;
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use opentelemetry_sdk::trace::{RandomIdGenerator, Sampler, TracerProvider};
    use opentelemetry_sdk::Resource;

    // Validate endpoint URL before connecting (SSRF protection)
    validate_otlp_endpoint(&otlp_config.endpoint).map_err(OtlpInitError::InvalidEndpoint)?;

    // Set up trace context propagation
    global::set_text_map_propagator(TraceContextPropagator::new());

    // Build the OTLP exporter with the configured endpoint
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(&otlp_config.endpoint)
        .build()
        .map_err(|e| OtlpInitError::TracerInit(e.to_string()))?;

    // Configure sampler based on sample_rate
    let sampler = if otlp_config.sample_rate >= 1.0 {
        Sampler::AlwaysOn
    } else if otlp_config.sample_rate <= 0.0 {
        Sampler::AlwaysOff
    } else {
        Sampler::TraceIdRatioBased(otlp_config.sample_rate)
    };

    // Build the tracer provider with resource attributes
    let resource = Resource::new(vec![
        KeyValue::new("service.name", otlp_config.service_name.clone()),
        KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
        KeyValue::new("messaging.system", "egregore"),
    ]);

    // Build the tracer provider
    let provider = TracerProvider::builder()
        .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
        .with_sampler(sampler)
        .with_id_generator(RandomIdGenerator::default())
        .with_resource(resource)
        .build();

    // Get tracer from provider before setting it globally
    let tracer = provider.tracer("egregore");

    // Set the provider globally for shutdown handling
    global::set_tracer_provider(provider);

    // Build the env filter
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("egregore={}", logging_config.level)));

    // Combine logging layer with OTLP layer
    // Note: The otel layer must be added last since it's typed to Registry
    match logging_config.format {
        LogFormat::Json => {
            let fmt_layer = tracing_subscriber::fmt::layer()
                .json()
                .with_span_events(FmtSpan::CLOSE)
                .with_current_span(logging_config.include_trace_ids)
                .with_span_list(false)
                .flatten_event(true);

            // Create the OpenTelemetry layer typed to the registry
            let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

            tracing_subscriber::registry()
                .with(env_filter)
                .with(otel_layer)
                .with(fmt_layer)
                .init();
        }
        LogFormat::Pretty => {
            let fmt_layer = tracing_subscriber::fmt::layer().with_span_events(FmtSpan::CLOSE);

            // Create the OpenTelemetry layer typed to the registry
            let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

            tracing_subscriber::registry()
                .with(env_filter)
                .with(otel_layer)
                .with(fmt_layer)
                .init();
        }
    }

    Ok(())
}

/// Error type for OTLP initialization failures.
#[cfg(feature = "otlp")]
#[derive(Debug, thiserror::Error)]
pub enum OtlpInitError {
    #[error("invalid OTLP endpoint: {0}")]
    InvalidEndpoint(String),
    #[error("failed to initialize OTLP tracer: {0}")]
    TracerInit(String),
}

/// Shutdown the OpenTelemetry tracer provider.
///
/// Call this during application shutdown to flush any pending spans.
/// This is a no-op if OTLP is not enabled or not initialized.
#[cfg(feature = "otlp")]
pub fn shutdown_otlp() {
    opentelemetry::global::shutdown_tracer_provider();
}

/// Generate a new trace ID (UUID v4).
///
/// Use this when starting a new top-level operation that should be traceable.
pub fn generate_trace_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Generate a new span ID (UUID v4, truncated to 16 chars for brevity).
///
/// Use this for individual operations within a trace.
pub fn generate_span_id() -> String {
    uuid::Uuid::new_v4().to_string()[..16].to_string()
}

/// Trace context for propagation across components.
///
/// This is used when publishing messages or making cross-component calls
/// to maintain trace correlation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TraceContext {
    /// Unique identifier for the end-to-end request flow.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Identifier for this specific operation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span_id: Option<String>,
    /// Parent span ID (if this is a child span).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
}

impl TraceContext {
    /// Create a new trace context with fresh IDs.
    pub fn new() -> Self {
        Self {
            trace_id: Some(generate_trace_id()),
            span_id: Some(generate_span_id()),
            parent_span_id: None,
        }
    }

    /// Create a child context from this one.
    ///
    /// The child inherits the trace_id, uses a new span_id,
    /// and records this context's span_id as parent.
    pub fn child(&self) -> Self {
        Self {
            trace_id: self.trace_id.clone(),
            span_id: Some(generate_span_id()),
            parent_span_id: self.span_id.clone(),
        }
    }

    /// Create from optional trace_id and span_id.
    pub fn from_ids(trace_id: Option<String>, span_id: Option<String>) -> Self {
        Self {
            trace_id,
            span_id,
            parent_span_id: None,
        }
    }

    /// Check if this context has tracing information.
    pub fn is_traced(&self) -> bool {
        self.trace_id.is_some()
    }
}

/// Sanitize a value for logging by redacting sensitive fields.
///
/// Per component-model.md, redacts fields matching:
/// - `*key*`, `*secret*`, `*password*`, `*token*`, `*credential*`
///
/// Returns the sanitized JSON value.
pub fn sanitize_for_logging(value: &serde_json::Value) -> serde_json::Value {
    sanitize_value(value, 0)
}

const MAX_STRING_LENGTH: usize = 1024;
const MAX_DEPTH: usize = 10;

fn sanitize_value(value: &serde_json::Value, depth: usize) -> serde_json::Value {
    if depth > MAX_DEPTH {
        return serde_json::Value::String("[TRUNCATED: max depth exceeded]".to_string());
    }

    match value {
        serde_json::Value::Object(map) => {
            let mut sanitized = serde_json::Map::new();
            for (key, val) in map {
                let key_lower = key.to_lowercase();
                let should_redact = key_lower.contains("key")
                    || key_lower.contains("secret")
                    || key_lower.contains("password")
                    || key_lower.contains("token")
                    || key_lower.contains("credential")
                    || key_lower.contains("private");

                if should_redact {
                    sanitized.insert(
                        key.clone(),
                        serde_json::Value::String("[REDACTED]".to_string()),
                    );
                } else {
                    sanitized.insert(key.clone(), sanitize_value(val, depth + 1));
                }
            }
            serde_json::Value::Object(sanitized)
        }
        serde_json::Value::Array(arr) => {
            let sanitized: Vec<serde_json::Value> = arr
                .iter()
                .take(100) // Limit array size
                .map(|v| sanitize_value(v, depth + 1))
                .collect();
            serde_json::Value::Array(sanitized)
        }
        serde_json::Value::String(s) => {
            if s.len() > MAX_STRING_LENGTH {
                // Find safe UTF-8 boundary for truncation
                let mut end = MAX_STRING_LENGTH;
                while end > 0 && !s.is_char_boundary(end) {
                    end -= 1;
                }
                serde_json::Value::String(format!(
                    "{}... [TRUNCATED: {} bytes]",
                    &s[..end],
                    s.len()
                ))
            } else {
                value.clone()
            }
        }
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_format_parsing() {
        assert_eq!("json".parse::<LogFormat>().unwrap(), LogFormat::Json);
        assert_eq!("JSON".parse::<LogFormat>().unwrap(), LogFormat::Json);
        assert_eq!("pretty".parse::<LogFormat>().unwrap(), LogFormat::Pretty);
        assert_eq!("Pretty".parse::<LogFormat>().unwrap(), LogFormat::Pretty);
        assert!("invalid".parse::<LogFormat>().is_err());
    }

    #[test]
    fn log_format_display() {
        assert_eq!(LogFormat::Json.to_string(), "json");
        assert_eq!(LogFormat::Pretty.to_string(), "pretty");
    }

    #[test]
    fn trace_context_new() {
        let ctx = TraceContext::new();
        assert!(ctx.trace_id.is_some());
        assert!(ctx.span_id.is_some());
        assert!(ctx.parent_span_id.is_none());
        assert!(ctx.is_traced());
    }

    #[test]
    fn trace_context_child() {
        let parent = TraceContext::new();
        let child = parent.child();

        assert_eq!(child.trace_id, parent.trace_id);
        assert_ne!(child.span_id, parent.span_id);
        assert_eq!(child.parent_span_id, parent.span_id);
    }

    #[test]
    fn trace_context_from_ids() {
        let ctx = TraceContext::from_ids(Some("trace-123".into()), Some("span-456".into()));
        assert_eq!(ctx.trace_id, Some("trace-123".into()));
        assert_eq!(ctx.span_id, Some("span-456".into()));
        assert!(ctx.is_traced());

        let empty = TraceContext::from_ids(None, None);
        assert!(!empty.is_traced());
    }

    #[test]
    fn generate_unique_ids() {
        let id1 = generate_trace_id();
        let id2 = generate_trace_id();
        assert_ne!(id1, id2);

        let span1 = generate_span_id();
        let span2 = generate_span_id();
        assert_ne!(span1, span2);
        assert_eq!(span1.len(), 16);
    }

    #[test]
    fn sanitize_redacts_sensitive_keys() {
        let input = serde_json::json!({
            "api_key": "sk-secret-123",
            "API_KEY": "sk-secret-456",
            "password": "hunter2",
            "secret_value": "shh",
            "token": "bearer-xyz",
            "private_key": "-----BEGIN PRIVATE KEY-----",
            "credential": "aws-creds",
            "normal_field": "visible",
            "nested": {
                "inner_token": "nested-secret",
                "inner_data": "visible"
            }
        });

        let sanitized = sanitize_for_logging(&input);

        assert_eq!(sanitized["api_key"], "[REDACTED]");
        assert_eq!(sanitized["API_KEY"], "[REDACTED]");
        assert_eq!(sanitized["password"], "[REDACTED]");
        assert_eq!(sanitized["secret_value"], "[REDACTED]");
        assert_eq!(sanitized["token"], "[REDACTED]");
        assert_eq!(sanitized["private_key"], "[REDACTED]");
        assert_eq!(sanitized["credential"], "[REDACTED]");
        assert_eq!(sanitized["normal_field"], "visible");
        assert_eq!(sanitized["nested"]["inner_token"], "[REDACTED]");
        assert_eq!(sanitized["nested"]["inner_data"], "visible");
    }

    #[test]
    fn sanitize_truncates_long_strings() {
        let long_string = "x".repeat(2000);
        let input = serde_json::json!({
            "data": long_string
        });

        let sanitized = sanitize_for_logging(&input);
        let data = sanitized["data"].as_str().unwrap();

        assert!(data.len() < 2000);
        assert!(data.contains("[TRUNCATED:"));
    }

    #[test]
    fn sanitize_truncates_utf8_safely() {
        // Create a string with multi-byte UTF-8 characters that would panic
        // if truncated at an arbitrary byte boundary
        let emoji_string = "🎉".repeat(500); // Each emoji is 4 bytes = 2000 bytes
        let input = serde_json::json!({
            "data": emoji_string
        });

        // This should not panic
        let sanitized = sanitize_for_logging(&input);
        let data = sanitized["data"].as_str().unwrap();

        assert!(data.contains("[TRUNCATED:"));
        // Verify the truncated portion is valid UTF-8 (would panic if not)
        let _ = data.to_string();
    }

    #[test]
    fn sanitize_handles_arrays() {
        let input = serde_json::json!({
            "items": [
                {"key": "secret1", "value": "visible1"},
                {"key": "secret2", "value": "visible2"}
            ]
        });

        let sanitized = sanitize_for_logging(&input);

        assert_eq!(sanitized["items"][0]["key"], "[REDACTED]");
        assert_eq!(sanitized["items"][0]["value"], "visible1");
        assert_eq!(sanitized["items"][1]["key"], "[REDACTED]");
        assert_eq!(sanitized["items"][1]["value"], "visible2");
    }

    #[test]
    fn logging_config_defaults() {
        let config = LoggingConfig::default();
        assert_eq!(config.format, LogFormat::Json);
        assert_eq!(config.level, "info");
        assert!(config.include_trace_ids);
    }

    #[test]
    fn logging_config_serde() {
        let yaml = r#"
format: pretty
level: debug
include_trace_ids: false
"#;
        let config: LoggingConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.format, LogFormat::Pretty);
        assert_eq!(config.level, "debug");
        assert!(!config.include_trace_ids);
    }

    #[test]
    fn otlp_config_defaults() {
        let config = OtlpConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.endpoint, "http://localhost:4317");
        assert_eq!(config.service_name, "egregore");
    }

    #[test]
    fn otlp_config_serde() {
        let yaml = r#"
enabled: true
endpoint: "http://collector.example.com:4317"
service_name: "my-egregore-node"
"#;
        let config: OtlpConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.enabled);
        assert_eq!(config.endpoint, "http://collector.example.com:4317");
        assert_eq!(config.service_name, "my-egregore-node");
    }

    #[test]
    fn otlp_config_partial_serde() {
        // Only enabled is set, others use defaults
        let yaml = r#"
enabled: true
"#;
        let config: OtlpConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.enabled);
        assert_eq!(config.endpoint, "http://localhost:4317");
        assert_eq!(config.service_name, "egregore");
    }

    #[test]
    fn telemetry_config_defaults() {
        let config = TelemetryConfig::default();
        assert_eq!(config.logging.format, LogFormat::Json);
        assert_eq!(config.logging.level, "info");
        assert!(config.logging.include_trace_ids);
        assert!(!config.otlp.enabled);
    }

    #[test]
    fn telemetry_config_nested_serde() {
        let yaml = r#"
logging:
  format: pretty
  level: debug
otlp:
  enabled: true
  endpoint: "http://localhost:4317"
"#;
        let config: TelemetryConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.logging.format, LogFormat::Pretty);
        assert_eq!(config.logging.level, "debug");
        assert!(config.otlp.enabled);
        assert_eq!(config.otlp.endpoint, "http://localhost:4317");
    }
}
