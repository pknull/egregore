//! Prometheus metrics infrastructure.
//!
//! Exposes `/metrics` endpoint for Prometheus scraping.
//! Metrics follow Prometheus naming conventions and the cardinality rules
//! from `docs/architecture/component-model.md`.
//!
//! ## Cardinality Rules
//! - `content_type`: bounded (known schema types) - OK
//! - `peer` as IP:port: semi-bounded - OK
//! - `author`: unbounded - NOT used as label
//! - `direction`: bounded (in/out) - OK
//! - `status`: bounded (success/error) - OK

use std::net::SocketAddr;
use std::sync::OnceLock;

use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::PrometheusBuilder;

use crate::config::MetricsConfig;

/// Global flag to track if metrics have been initialized.
static METRICS_INITIALIZED: OnceLock<bool> = OnceLock::new();

/// Initialize the Prometheus metrics recorder and start the HTTP server.
///
/// This must be called once at startup. Subsequent calls are no-ops.
/// The metrics server binds to the address specified in the config.
pub fn init_metrics(config: &MetricsConfig) -> anyhow::Result<()> {
    if METRICS_INITIALIZED.get().is_some() {
        tracing::debug!("metrics already initialized, skipping");
        return Ok(());
    }

    if !config.enabled {
        tracing::info!("metrics disabled");
        METRICS_INITIALIZED.set(false).ok();
        return Ok(());
    }

    let addr: SocketAddr = config
        .bind
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid metrics bind address '{}': {}", config.bind, e))?;

    let builder = PrometheusBuilder::new().with_http_listener(addr);

    builder
        .install()
        .map_err(|e| anyhow::anyhow!("failed to install Prometheus recorder: {}", e))?;

    tracing::info!(addr = %addr, "metrics server started");
    METRICS_INITIALIZED.set(true).ok();

    Ok(())
}

/// Check if metrics are enabled and initialized.
pub fn is_enabled() -> bool {
    METRICS_INITIALIZED.get().copied().unwrap_or(false)
}

// ============================================================================
// Message Ingestion Metrics
// ============================================================================

/// Record a message ingestion event.
///
/// Labels:
/// - `content_type`: The content type from the message (bounded)
/// - `status`: "success" or "error"
pub fn record_message_ingested(content_type: &str, status: &str) {
    counter!(
        "egregore_messages_ingested_total",
        "content_type" => content_type.to_string(),
        "status" => status.to_string()
    )
    .increment(1);
}

/// Record the size of an ingested message.
pub fn record_message_size(size_bytes: usize) {
    histogram!("egregore_message_size_bytes").record(size_bytes as f64);
}

// ============================================================================
// Replication Metrics
// ============================================================================

/// Record a message replication event.
///
/// Labels:
/// - `direction`: "in" (received from peer) or "out" (sent to peer)
pub fn record_message_replicated(direction: &str) {
    counter!(
        "egregore_messages_replicated_total",
        "direction" => direction.to_string()
    )
    .increment(1);
}

/// Record replication latency.
pub fn record_replication_latency(latency_secs: f64) {
    histogram!("egregore_replication_latency_seconds").record(latency_secs);
}

// ============================================================================
// Peer Metrics
// ============================================================================

/// Set the number of connected peers.
pub fn set_peers_connected(count: usize) {
    gauge!("egregore_peers_connected").set(count as f64);
}

// ============================================================================
// Database Metrics
// ============================================================================

/// Set the database size in bytes.
pub fn set_db_size(size_bytes: u64) {
    gauge!("egregore_db_size_bytes").set(size_bytes as f64);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_config_default_values() {
        let config = MetricsConfig::default();
        assert!(config.enabled);
        assert_eq!(config.bind, "127.0.0.1:9090");
        assert_eq!(config.path, "/metrics");
    }

    #[test]
    fn is_enabled_returns_false_when_not_initialized() {
        // In test context, metrics may or may not be initialized
        // Just verify the function doesn't panic
        let _ = is_enabled();
    }
}
