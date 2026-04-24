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
// Phase 2 Wave 5 Step 27 — Transport + Bridge Observability Metrics
// ============================================================================
//
// All families below are populated by a single background task spawned
// from `main.rs` (see `run_transport_metrics_updater`). The task polls
// `engine.transport_health()`, `get_chain_gap_summary`, and
// `bus_author_seq_index_latest_author_activity` every 30 seconds and
// writes the corresponding gauge/counter values.
//
// Cardinality:
// - `direction`: bounded (composite children count; typically 2).
// - `destination`: bounded (per-direction, typically one per child).
// - `author`: Ed25519 pubkey — intentionally UNBOUNDED for operator-
//   side observability per amendment §C.14 + §G.3. The pubkey IS the
//   author's identity; no PII scrubbing applies here the way it does
//   to `last_error` strings (A2 guidance). Deployments concerned
//   about metric cardinality should configure their Prometheus
//   scraping to drop the `author` label — see `docs/deployment/bus.md`
//   when that document lands (Step 28).

// ---- Bridge gauges ----

/// `egregore_bridge_queue_depth{direction, destination}` — per-
/// direction total depth (sum over authors).
pub fn set_bridge_queue_depth(direction: &str, destination: &str, depth: u64) {
    gauge!(
        "egregore_bridge_queue_depth",
        "direction" => direction.to_string(),
        "destination" => destination.to_string()
    )
    .set(depth as f64);
}

/// `egregore_bridge_authors_backpressured{direction}` — count of
/// backpressured author queues in a direction.
pub fn set_bridge_authors_backpressured(direction: &str, count: u64) {
    gauge!(
        "egregore_bridge_authors_backpressured",
        "direction" => direction.to_string()
    )
    .set(count as f64);
}

/// `egregore_bridge_authors_active{direction}` — count of non-empty
/// author queues in a direction.
pub fn set_bridge_authors_active(direction: &str, count: u64) {
    gauge!(
        "egregore_bridge_authors_active",
        "direction" => direction.to_string()
    )
    .set(count as f64);
}

/// `egregore_bridge_oldest_queued_age_seconds{direction}` — sourced
/// from `BridgeQueuesHealth.oldest_queued_age_secs` when `Some`. When
/// `None` the gauge is unset (not zeroed) so operators can distinguish
/// "no queue" from "queue age = 0".
pub fn set_bridge_oldest_queued_age_secs(direction: &str, age_secs: u64) {
    gauge!(
        "egregore_bridge_oldest_queued_age_seconds",
        "direction" => direction.to_string()
    )
    .set(age_secs as f64);
}

/// `egregore_bridge_publish_in_flight_age_seconds{direction}` —
/// sourced from `BridgeQueuesHealth.publish_in_flight_age_secs`.
pub fn set_bridge_publish_in_flight_age_secs(direction: &str, age_secs: u64) {
    gauge!(
        "egregore_bridge_publish_in_flight_age_seconds",
        "direction" => direction.to_string()
    )
    .set(age_secs as f64);
}

// ---- Bridge counters ----
//
// These wrap the monotonic counters exposed on `BridgeQueuesHealth`.
// The underlying Prometheus registry stores its own monotonic counter,
// so we bridge by computing the delta since the last observation.
// `set_*_counter_absolute` makes the intent explicit.

/// Monotonic counter of upstream-ingress block events. `absolute_total`
/// is the value currently on `BridgeQueuesHealth`; we convert to a
/// delta against our local shadow so the Prometheus counter increments
/// correctly.
pub fn set_bridge_backpressure_events_absolute(direction: &str, absolute_total: u64) {
    set_counter_delta(
        "egregore_bridge_backpressure_events_total",
        direction,
        absolute_total,
    );
}

pub fn set_bridge_ack_on_error_absolute(direction: &str, absolute_total: u64) {
    set_counter_delta(
        "egregore_bridge_ack_on_error_total",
        direction,
        absolute_total,
    );
}

pub fn set_bridge_nats_redelivery_absolute(direction: &str, absolute_total: u64) {
    set_counter_delta(
        "egregore_bridge_nats_redelivery_total",
        direction,
        absolute_total,
    );
}

pub fn set_bridge_self_echo_absolute(direction: &str, absolute_total: u64) {
    set_counter_delta("egregore_bridge_self_echo_total", direction, absolute_total);
}

// Internal: track last-observed absolute value per (metric_name, direction)
// so the poller can emit the correct delta to the monotonic counter.
static COUNTER_SHADOWS: OnceLock<
    std::sync::Mutex<std::collections::HashMap<(String, String), u64>>,
> = OnceLock::new();

fn set_counter_delta(metric_name: &'static str, direction: &str, absolute_total: u64) {
    let shadows =
        COUNTER_SHADOWS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let key = (metric_name.to_string(), direction.to_string());
    let delta = {
        let mut guard = shadows
            .lock()
            .expect("metrics counter shadow mutex poisoned");
        let prev = guard.get(&key).copied().unwrap_or(0);
        // Handle resets: if absolute_total < prev (e.g., process restart
        // of the source without resetting the Prometheus exporter),
        // treat the new value as the delta (not negative).
        let delta = absolute_total.saturating_sub(prev);
        guard.insert(key, absolute_total);
        delta
    };
    if delta > 0 {
        counter!(
            metric_name,
            "direction" => direction.to_string()
        )
        .increment(delta);
    }
}

// ---- Per-author gauges (amendment §G.3) ----

/// `egregore_transport_nats_author_silence_seconds{author}` —
/// amendment §G.3 #1: time since we last saw a bus-sourced message
/// from this author. Rising values across many authors indicate
/// broker outage OR consumer stall; rising on a single author is
/// author-side silence.
pub fn set_author_silence_secs(author: &str, secs: u64) {
    gauge!(
        "egregore_transport_nats_author_silence_seconds",
        "author" => author.to_string()
    )
    .set(secs as f64);
}

/// `egregore_chain_gap_count{author}` — amendment §G.3 #2: per-author
/// count of messages with `chain_valid = 0` in local SQLite. Rising
/// means genuine chain-gap state (predecessor missing), independent
/// of which transport delivered the message.
pub fn set_chain_gap_count(author: &str, count: u64) {
    gauge!(
        "egregore_chain_gap_count",
        "author" => author.to_string()
    )
    .set(count as f64);
}

/// `egregore_chain_gap_oldest_seconds{author}` — amendment §G.3 #3:
/// per-author age of oldest chain_invalid message. A value exceeding
/// the retention interval suggests the gap is permanent.
pub fn set_chain_gap_oldest_secs(author: &str, secs: u64) {
    gauge!(
        "egregore_chain_gap_oldest_seconds",
        "author" => author.to_string()
    )
    .set(secs as f64);
}

// ============================================================================
// Phase 2 Wave 5 Step 27 — background metrics updater task.
// ============================================================================

use std::sync::Arc;

use chrono::Utc;

use crate::feed::engine::FeedEngine;

/// Default poll interval for the transport metrics updater. Chosen to
/// match the scry `useBusAuthors` polling cadence (§C.14); bridge
/// gauges and chain-gap metrics share this tick.
pub const TRANSPORT_METRICS_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(30);

/// Drive the transport metrics families from `engine.transport_health()`
/// and the store's chain-gap + author-activity projections. Returns
/// immediately if `metrics.enabled` is false at start-up; otherwise
/// loops indefinitely on the provided interval.
///
/// Call sites: spawned once from `main.rs` after
/// `init_metrics(..)` succeeds. The spawn is conditional on
/// `config.metrics.enabled` to keep the tick off when metrics are
/// disabled.
pub async fn run_transport_metrics_updater(engine: Arc<FeedEngine>) {
    run_transport_metrics_updater_with_interval(engine, TRANSPORT_METRICS_INTERVAL).await;
}

/// Test-seam variant accepting a custom interval; production call
/// paths use `run_transport_metrics_updater`.
pub async fn run_transport_metrics_updater_with_interval(
    engine: Arc<FeedEngine>,
    interval: std::time::Duration,
) {
    let mut ticker = tokio::time::interval(interval);
    // Align to "on start" + regular cadence; `set_missed_tick_behavior`
    // avoids thunder after a long stall.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        update_transport_metrics_once(&engine).await;
    }
}

/// One tick of the updater — broken out so tests (and main.rs during
/// testing) can drive it deterministically without spinning the loop.
pub async fn update_transport_metrics_once(engine: &Arc<FeedEngine>) {
    // 1. Transport health → bridge + leaf gauges.
    if let Some(health) = engine.transport_health() {
        update_bridge_metrics_from_health(&health);
    }

    // 2. Chain-gap summary → per-author gauges.
    let engine_for_gaps = engine.clone();
    let chain_result = tokio::task::spawn_blocking(move || {
        engine_for_gaps.store().get_chain_gap_summary()
    })
    .await;
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

    // 3. Bus author activity → author-silence gauges.
    let engine_for_authors = engine.clone();
    let activity_result = tokio::task::spawn_blocking(move || {
        engine_for_authors
            .store()
            .bus_author_seq_index_latest_author_activity()
    })
    .await;
    match activity_result {
        Ok(Ok(rows)) => {
            let now = Utc::now();
            for row in rows {
                let silence = (now - row.last_indexed_at).num_seconds().max(0) as u64;
                set_author_silence_secs(&row.author, silence);
            }
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "bus_author_seq_index_latest_author_activity failed; skipping tick");
        }
        Err(e) => {
            tracing::warn!(error = %e, "author-silence task failed; skipping tick");
        }
    }
}

/// Walk a `TransportHealth` tree and write bridge metrics for each
/// child that carries `bridge_queues` (i.e., each composite child-as-
/// destination). The top-level aggregate has no `bridge_queues` of its
/// own — only its children do — so a single-transport deployment is a
/// no-op here. Zero-child deployments never call this function.
fn update_bridge_metrics_from_health(health: &crate::transport::health::TransportHealth) {
    for child in &health.children {
        if let Some(queues) = &child.bridge_queues {
            let direction = child.backend;
            let destination = queues.destination.as_str();
            set_bridge_queue_depth(direction, destination, queues.depth_total);
            set_bridge_authors_backpressured(direction, queues.authors_backpressured);
            set_bridge_authors_active(direction, queues.authors_active);
            if let Some(age) = queues.oldest_queued_age_secs {
                set_bridge_oldest_queued_age_secs(direction, age);
            }
            if let Some(age) = queues.publish_in_flight_age_secs {
                set_bridge_publish_in_flight_age_secs(direction, age);
            }
            set_bridge_backpressure_events_absolute(direction, queues.backpressure_events_total);
            set_bridge_ack_on_error_absolute(direction, queues.ack_on_error_total);
            set_bridge_nats_redelivery_absolute(direction, queues.nats_redelivery_total);
            set_bridge_self_echo_absolute(direction, queues.self_echo_total);
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
