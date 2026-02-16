//! TTL-based message eviction — relay-only garbage collection.
//!
//! Runs hourly, deletes messages older than the configured TTL. After eviction,
//! chain_valid flags are updated and orphaned feed entries cleaned up. This is
//! why relays are ephemeral stores — nodes should connect directly to publishers
//! for authoritative history.

use std::sync::Arc;
use std::time::Duration;

use egregore::feed::engine::FeedEngine;

const EVICTION_INTERVAL_SECS: u64 = 3600; // hourly

pub async fn run_eviction_loop(engine: Arc<FeedEngine>, ttl_days: u32) {
    tracing::info!(ttl_days, "eviction loop started");

    loop {
        tokio::time::sleep(Duration::from_secs(EVICTION_INTERVAL_SECS)).await;

        let eng = engine.clone();
        let result = tokio::task::spawn_blocking(move || {
            let cutoff = chrono::Utc::now() - chrono::Duration::days(i64::from(ttl_days));
            eng.store().evict_before(&cutoff)
        })
        .await;

        match result {
            Ok(Ok(count)) => {
                if count > 0 {
                    tracing::info!(evicted = count, "TTL eviction complete");
                }
            }
            Ok(Err(e)) => tracing::warn!(error = %e, "eviction failed"),
            Err(e) => tracing::warn!(error = %e, "eviction task panicked"),
        }
    }
}
