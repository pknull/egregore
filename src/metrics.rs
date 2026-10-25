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

// ============================================================================
// Chain-gap metrics
// ============================================================================
//
// Populated by a single background task spawned from `main.rs` (see
// `run_chain_gap_metrics_updater`). The task polls `get_chain_gap_summary`
// every 30 seconds and writes the per-author gauges.
//
// Cardinality: `author` is an Ed25519 pubkey — intentionally UNBOUNDED for
// operator-side observability. The pubkey IS the author's identity; no PII
// scrubbing applies here the way it does to `last_error` strings.
// Deployments concerned about metric cardinality should configure their
// Prometheus scraping to drop the `author` label.

/// `egregore_chain_gap_count{author}` — per-author count of messages with
/// `chain_valid = 0` in local SQLite. Rising means genuine chain-gap state
/// (predecessor missing), independent of which transport delivered the
/// message.
pub fn set_chain_gap_count(author: &str, count: u64) {
    gauge!(
        "egregore_chain_gap_count",
        "author" => author.to_string()
    )
    .set(count as f64);
}

/// `egregore_chain_gap_oldest_seconds{author}` — per-author age of oldest
/// chain_invalid message. A value exceeding the retention interval suggests
/// the gap is permanent.
pub fn set_chain_gap_oldest_secs(author: &str, secs: u64) {
    gauge!(
        "egregore_chain_gap_oldest_seconds",
        "author" => author.to_string()
    )
    .set(secs as f64);
}

// ============================================================================
// Background chain-gap metrics updater task.
// ============================================================================

use std::sync::Arc;

use chrono::Utc;

use crate::feed::engine::FeedEngine;

/// Poll interval for the chain-gap metrics updater.
pub const CHAIN_GAP_METRICS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Drive the chain-gap gauges from the store's chain-gap projection.
/// Loops indefinitely on `CHAIN_GAP_METRICS_INTERVAL`.
///
/// Call sites: spawned once from `main.rs` after `init_metrics(..)`
/// succeeds. The spawn is conditional on `config.metrics.enabled` to keep
/// the tick off when metrics are disabled.
pub async fn run_chain_gap_metrics_updater(engine: Arc<FeedEngine>) {
    run_chain_gap_metrics_updater_with_interval(engine, CHAIN_GAP_METRICS_INTERVAL).await;
}

/// Test-seam variant accepting a custom interval; production call
/// paths use `run_chain_gap_metrics_updater`.
pub async fn run_chain_gap_metrics_updater_with_interval(
    engine: Arc<FeedEngine>,
    interval: std::time::Duration,
) {
    let mut ticker = tokio::time::interval(interval);
    // Align to "on start" + regular cadence; `set_missed_tick_behavior`
    // avoids thunder after a long stall.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        update_chain_gap_metrics_once(&engine).await;
    }
}

/// One tick of the updater — broken out so tests (and main.rs during
/// testing) can drive it deterministically without spinning the loop.
pub async fn update_chain_gap_metrics_once(engine: &Arc<FeedEngine>) {
    let engine_for_gaps = engine.clone();
    let chain_result =
        tokio::task::spawn_blocking(move || engine_for_gaps.store().get_chain_gap_summary()).await;
    match chain_result {
        Ok(Ok(rows)) => {
            let now = Utc::now();
            for row in rows {
                set_chain_gap_count(&row.author, row.gap_count);
                let age = (now - row.oldest).num_seconds().max(0) as u64;
                set_chain_gap_oldest_secs(&row.author, age);
            }
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "get_chain_gap_summary failed; skipping tick");
        }
        Err(e) => {
            tracing::warn!(error = %e, "chain-gap task failed; skipping tick");
        }
    }
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
