//! Retry scheduler — Phase 2 Wave 1 Step 8.
//!
//! Periodically walks the `pending_forwarding` table for a transport and
//! re-attempts delivery of rows whose exponential-backoff window has
//! expired. On success, the row is deleted via
//! `pending_forwarding_complete`; on failure, `record_failure` bumps the
//! attempt counter and stamps `last_attempt_at = now`.
//!
//! **Wave 1 scope:** the loop body + helpers. Wave 4 Step 23 is
//! responsible for spawning `run_retry_scheduler` from `main.rs` with a
//! `publish_callback` wired to `BusTransport::publish_attempt`.
//!
//! Amendment §C.10 binding: retries MUST NOT go through
//! `BusTransport::publish` (which would recursively enqueue a duplicate
//! pending row — the idempotent INSERT OR IGNORE guards against that,
//! but the call is wasted work). The callback parameter type forces the
//! caller to choose `publish_attempt` deliberately.
//!
//! Amendment OQ-P2-4: if the source message has been swept by SQLite
//! retention between enqueue and the retry tick, the scheduler accepts
//! the loss — deletes the pending row, logs at INFO, and continues.
//!
//! The scheduler is generic over a `publish_callback` so it can be
//! driven by a mock in unit tests (Wave 1) and by a real BusTransport
//! in production (Wave 4). This decouples the scheduler from the
//! currently-incomplete `publish_attempt` method (Wave 2 Step 9).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::error::{EgreError, Result};
use crate::feed::engine::FeedEngine;
use crate::feed::models::Message;

/// Type alias for the retry callback — an `Arc<dyn Fn>` returning a
/// boxed future. Wave 4 wires this to a closure that resolves
/// `Weak<BusTransport>` and calls `publish_attempt`; Wave 1 unit tests
/// wire it to a mock that records invocations.
pub type PublishCallback = Arc<
    dyn Fn(&Message) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>
        + Send
        + Sync
        + 'static,
>;

/// Per-tick row limit — small enough that a single tick is bounded in
/// work, large enough that the steady-state queue drains in a handful
/// of ticks. The scheduler's poll interval (caller-supplied) is the
/// primary rate limiter.
const SCHEDULER_BATCH_LIMIT: u32 = 32;

/// Exponential-backoff formula — `min(2^attempt_count × 5s, 5min)` per
/// Phase 2 base plan §8 Step 23.
///
/// Exposed as `pub` for deterministic testing without clock injection.
pub fn exponential_backoff(attempt_count: u32) -> Duration {
    const UNIT_SECS: u64 = 5;
    const MAX_SECS: u64 = 5 * 60;
    // Saturate at 2^30 attempts (a billion retries) — a value that will
    // never occur in practice but keeps the multiplication bounded.
    let shift = attempt_count.min(30);
    let secs = UNIT_SECS.saturating_mul(1u64 << shift).min(MAX_SECS);
    Duration::from_secs(secs)
}

/// Run the retry scheduler loop for a specific transport.
///
/// Blocks until `cancel` is triggered. Each tick:
///
/// 1. List up to `SCHEDULER_BATCH_LIMIT` pending rows for the transport.
/// 2. For each row where the backoff window has expired:
///    - Load the source message from the feed store (via `spawn_blocking`).
///    - If the message is missing (retention swept it): `complete` the
///      row (OQ-P2-4 — accept the loss) and continue.
///    - Otherwise: invoke `publish_callback`. On success: `complete`.
///      On error: `record_failure` with the error's short display form.
/// 3. Sleep for `poll_interval` or until `cancel` is triggered.
///
/// `transport_id` lets future transports share this scheduler.
pub async fn run_retry_scheduler(
    engine: Arc<FeedEngine>,
    transport_id: &'static str,
    publish_callback: PublishCallback,
    poll_interval: Duration,
    cancel: CancellationToken,
) {
    loop {
        if cancel.is_cancelled() {
            tracing::debug!(transport_id, "retry scheduler: cancelled");
            return;
        }

        if let Err(e) = tick(&engine, transport_id, &publish_callback).await {
            tracing::warn!(transport_id, error = %e, "retry scheduler: tick failed");
        }

        tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            _ = sleep(poll_interval) => continue,
        }
    }
}

/// Single tick of the retry scheduler — extracted for unit testing so
/// tests can drive individual iterations without running the outer
/// `run_retry_scheduler` loop.
pub async fn tick(
    engine: &Arc<FeedEngine>,
    transport_id: &'static str,
    publish_callback: &PublishCallback,
) -> Result<()> {
    let engine_list = engine.clone();
    let rows = tokio::task::spawn_blocking(move || {
        engine_list
            .store()
            .pending_forwarding_list(transport_id, SCHEDULER_BATCH_LIMIT)
    })
    .await
    .map_err(|e| EgreError::Config {
        reason: format!("retry scheduler: spawn_blocking list failed: {e}"),
    })??;

    let now = Utc::now();
    for row in rows {
        // Skip rows whose backoff window has not yet expired.
        if let Some(last) = row.last_attempt_at {
            let window = exponential_backoff(row.attempt_count);
            let elapsed = now.signed_duration_since(last);
            let elapsed_duration = elapsed.to_std().unwrap_or(Duration::ZERO);
            if elapsed_duration < window {
                continue;
            }
        }

        // Load the source message from the store (on the blocking pool).
        let engine_load = engine.clone();
        let hash = row.message_hash.clone();
        let msg_opt = tokio::task::spawn_blocking(move || engine_load.store().get_message(&hash))
            .await
            .map_err(|e| EgreError::Config {
                reason: format!("retry scheduler: spawn_blocking load failed: {e}"),
            })??;

        match msg_opt {
            None => {
                // OQ-P2-4: source message has been retention-swept. Accept
                // the loss, drop the pending row, move on.
                tracing::info!(
                    transport_id,
                    message_hash = %row.message_hash,
                    "retry scheduler: source message missing; completing row"
                );
                let engine_complete = engine.clone();
                let hash_complete = row.message_hash.clone();
                tokio::task::spawn_blocking(move || {
                    engine_complete
                        .store()
                        .pending_forwarding_complete(transport_id, &hash_complete)
                })
                .await
                .map_err(|e| EgreError::Config {
                    reason: format!(
                        "retry scheduler: spawn_blocking complete(missing) failed: {e}"
                    ),
                })??;
                continue;
            }
            Some(msg) => {
                match publish_callback(&msg).await {
                    Ok(()) => {
                        let engine_ok = engine.clone();
                        let hash_ok = row.message_hash.clone();
                        tokio::task::spawn_blocking(move || {
                            engine_ok
                                .store()
                                .pending_forwarding_complete(transport_id, &hash_ok)
                        })
                        .await
                        .map_err(|e| EgreError::Config {
                            reason: format!(
                                "retry scheduler: spawn_blocking complete(ok) failed: {e}"
                            ),
                        })??;
                    }
                    Err(e) => {
                        // Caller is responsible for PII scrubbing the
                        // error's Display form. The scheduler itself
                        // only records the already-short error string.
                        let error_str = e.to_string();
                        let engine_err = engine.clone();
                        let hash_err = row.message_hash.clone();
                        tokio::task::spawn_blocking(move || {
                            engine_err.store().pending_forwarding_record_failure(
                                transport_id,
                                &hash_err,
                                &error_str,
                            )
                        })
                        .await
                        .map_err(|e| EgreError::Config {
                            reason: format!(
                                "retry scheduler: spawn_blocking record_failure failed: {e}"
                            ),
                        })??;
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::store::FeedStore;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Backoff formula sanity — verifies the monotonic growth + clamp.
    #[test]
    fn exponential_backoff_grows_and_clamps_at_5min() {
        // attempts 0, 1, 2 → 5s, 10s, 20s
        assert_eq!(exponential_backoff(0), Duration::from_secs(5));
        assert_eq!(exponential_backoff(1), Duration::from_secs(10));
        assert_eq!(exponential_backoff(2), Duration::from_secs(20));
        // Clamped at 300s (= 5 min). 2^6 × 5 = 320s → clamp to 300s.
        assert_eq!(exponential_backoff(6), Duration::from_secs(300));
        // Large attempts still clamp (no overflow).
        assert_eq!(exponential_backoff(100), Duration::from_secs(300));
    }

    /// Build a bare engine + store + a single pending row for `msg`.
    fn setup_engine_with_pending(msg: &Message) -> Arc<FeedEngine> {
        let store = FeedStore::open_memory().unwrap();
        // The retry scheduler loads the source message from the store,
        // so we insert it here. `insert_message` with chain_valid=true
        // is sufficient — we do not exercise chain logic in these tests.
        store.insert_message(msg, true).unwrap();
        let engine = Arc::new(FeedEngine::new(store));
        engine
            .store()
            .pending_forwarding_enqueue("bus", msg)
            .unwrap();
        engine
    }

    /// Happy-path retry: callback returns Ok → pending row is completed
    /// and disappears from the table.
    #[tokio::test]
    async fn tick_completes_row_on_successful_publish() {
        let msg = crate::feed::store::make_test_message("@alice.ed25519", 1, None);
        let engine = setup_engine_with_pending(&msg);

        let invocations = Arc::new(AtomicU32::new(0));
        let invocations_cb = invocations.clone();
        let cb: PublishCallback = Arc::new(move |_msg| {
            let inv = invocations_cb.clone();
            Box::pin(async move {
                inv.fetch_add(1, Ordering::AcqRel);
                Ok(())
            })
        });

        tick(&engine, "bus", &cb).await.unwrap();

        assert_eq!(invocations.load(Ordering::Acquire), 1);
        assert_eq!(engine.store().pending_forwarding_count("bus").unwrap(), 0);
    }

    /// Backoff respected: a row whose `last_attempt_at` was set 1ms ago
    /// MUST NOT be retried (attempt_count=0 backoff window is 5s).
    #[tokio::test]
    async fn tick_skips_row_inside_backoff_window() {
        let msg = crate::feed::store::make_test_message("@alice.ed25519", 1, None);
        let engine = setup_engine_with_pending(&msg);

        // Record a failure to populate last_attempt_at and bump attempt_count.
        engine
            .store()
            .pending_forwarding_record_failure("bus", &msg.hash, "TRANSIENT")
            .unwrap();

        let invocations = Arc::new(AtomicU32::new(0));
        let invocations_cb = invocations.clone();
        let cb: PublishCallback = Arc::new(move |_msg| {
            let inv = invocations_cb.clone();
            Box::pin(async move {
                inv.fetch_add(1, Ordering::AcqRel);
                Ok(())
            })
        });

        tick(&engine, "bus", &cb).await.unwrap();

        // Callback not invoked because backoff window (≥5s) has not expired.
        assert_eq!(
            invocations.load(Ordering::Acquire),
            0,
            "row inside backoff window must be skipped"
        );
        // Row still present.
        assert_eq!(engine.store().pending_forwarding_count("bus").unwrap(), 1);
    }

    /// Poison-pill: pending row references a message that has been
    /// retention-swept. Scheduler completes the row without invoking
    /// the callback (OQ-P2-4).
    #[tokio::test]
    async fn tick_completes_pending_when_source_message_missing() {
        // Build a pending row that references a hash NOT in messages table.
        let store = FeedStore::open_memory().unwrap();
        let ghost_msg = crate::feed::store::make_test_message("@alice.ed25519", 1, None);
        store.pending_forwarding_enqueue("bus", &ghost_msg).unwrap();
        // NOTE: insert_message NOT called — message is "missing".
        let engine = Arc::new(FeedEngine::new(store));

        let invocations = Arc::new(AtomicU32::new(0));
        let invocations_cb = invocations.clone();
        let cb: PublishCallback = Arc::new(move |_msg| {
            let inv = invocations_cb.clone();
            Box::pin(async move {
                inv.fetch_add(1, Ordering::AcqRel);
                Ok(())
            })
        });

        tick(&engine, "bus", &cb).await.unwrap();

        assert_eq!(
            invocations.load(Ordering::Acquire),
            0,
            "callback must not run when source message is missing"
        );
        assert_eq!(
            engine.store().pending_forwarding_count("bus").unwrap(),
            0,
            "missing-source row must be completed (OQ-P2-4 loss-accept)"
        );
    }

    /// Callback error: pending row remains, attempt_count bumps to 1,
    /// last_error stored, last_attempt_at set.
    #[tokio::test]
    async fn tick_records_failure_on_callback_error() {
        let msg = crate::feed::store::make_test_message("@alice.ed25519", 1, None);
        let engine = setup_engine_with_pending(&msg);

        let cb: PublishCallback = Arc::new(|_msg| {
            Box::pin(async {
                Err(EgreError::Peer {
                    reason: "BROKER_UNREACHABLE".into(),
                })
            })
        });

        tick(&engine, "bus", &cb).await.unwrap();

        let rows = engine.store().pending_forwarding_list("bus", 10).unwrap();
        assert_eq!(rows.len(), 1, "row must persist on callback error");
        assert_eq!(rows[0].attempt_count, 1);
        assert!(rows[0].last_attempt_at.is_some());
        let err = rows[0]
            .last_error
            .as_deref()
            .expect("record_failure must store error");
        assert!(
            err.contains("BROKER_UNREACHABLE"),
            "recorded error must carry the callback's error text; got: {err}"
        );
    }
}
