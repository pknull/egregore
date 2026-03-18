//! Time-windowed log deduplication to prevent log flooding.
//!
//! Provides a cache that suppresses identical warnings within a configurable
//! time window. Used to prevent routine gossip rejects from flooding syslog.
//!
//! See: docs/architecture/component-model.md (Log Level Policy, Rate Limiting)

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Default deduplication window (60 seconds per component-model.md).
pub const DEFAULT_DEDUP_WINDOW: Duration = Duration::from_secs(60);

/// Maximum entries in the deduplication cache to bound memory.
const MAX_CACHE_ENTRIES: usize = 1000;

/// A time-windowed cache for log deduplication.
///
/// Thread-safe wrapper around a HashMap that tracks when each
/// (error_type, context) pair was last logged.
pub struct LogDeduplicator {
    cache: Mutex<HashMap<String, Instant>>,
    window: Duration,
}

impl LogDeduplicator {
    /// Create a new deduplicator with the default 60-second window.
    pub fn new() -> Self {
        Self::with_window(DEFAULT_DEDUP_WINDOW)
    }

    /// Create a new deduplicator with a custom window.
    pub fn with_window(window: Duration) -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
            window,
        }
    }

    /// Check if a log message should be emitted.
    ///
    /// Returns `true` if this (error_type, context) pair hasn't been seen
    /// within the deduplication window, and marks it as seen.
    ///
    /// Returns `false` if this is a duplicate that should be suppressed.
    ///
    /// # Arguments
    /// * `error_type` - Category of error (e.g., "ingest_failed", "parse_error")
    /// * `context` - Additional context (e.g., peer address, message hash)
    pub fn should_log(&self, error_type: &str, context: &str) -> bool {
        let key = format!("{}:{}", error_type, context);
        let now = Instant::now();

        let mut cache = self.cache.lock().unwrap();

        // Check if we've seen this recently
        if let Some(last_seen) = cache.get(&key) {
            if now.duration_since(*last_seen) < self.window {
                return false;
            }
        }

        // Evict stale entries if at capacity
        if cache.len() >= MAX_CACHE_ENTRIES {
            cache.retain(|_, ts| now.duration_since(*ts) < self.window);
            // If still at capacity after cleanup, allow the log but don't cache
            if cache.len() >= MAX_CACHE_ENTRIES {
                return true;
            }
        }

        cache.insert(key, now);
        true
    }

    /// Clear all cached entries.
    #[cfg(test)]
    pub fn clear(&self) {
        self.cache.lock().unwrap().clear();
    }
}

impl Default for LogDeduplicator {
    fn default() -> Self {
        Self::new()
    }
}

/// Global deduplicator for gossip warnings.
///
/// Uses `std::sync::LazyLock` for zero-cost initialization.
pub static GOSSIP_DEDUP: std::sync::LazyLock<LogDeduplicator> =
    std::sync::LazyLock::new(LogDeduplicator::new);

/// Macro for deduplicated warning logs.
///
/// Emits a warning only if the (error_type, context) pair hasn't been
/// logged within the deduplication window.
///
/// # Usage
/// ```ignore
/// dedup_warn!("ingest_failed", peer_addr, "failed to ingest: {}", error);
/// ```
#[macro_export]
macro_rules! dedup_warn {
    ($error_type:expr, $context:expr, $($arg:tt)*) => {
        if $crate::gossip::log_dedup::GOSSIP_DEDUP.should_log($error_type, &$context.to_string()) {
            tracing::warn!($($arg)*);
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suppresses_duplicates_within_window() {
        let dedup = LogDeduplicator::with_window(Duration::from_secs(60));

        // First log should pass
        assert!(dedup.should_log("test_error", "peer1"));

        // Same error/context should be suppressed
        assert!(!dedup.should_log("test_error", "peer1"));

        // Different context should pass
        assert!(dedup.should_log("test_error", "peer2"));

        // Different error type should pass
        assert!(dedup.should_log("other_error", "peer1"));
    }

    #[test]
    fn allows_after_window_expires() {
        let dedup = LogDeduplicator::with_window(Duration::from_millis(10));

        assert!(dedup.should_log("test_error", "peer1"));
        assert!(!dedup.should_log("test_error", "peer1"));

        // Wait for window to expire
        std::thread::sleep(Duration::from_millis(15));

        // Should be allowed again
        assert!(dedup.should_log("test_error", "peer1"));
    }

    #[test]
    fn handles_capacity_limit() {
        let dedup = LogDeduplicator::with_window(Duration::from_secs(60));

        // Fill up the cache
        for i in 0..MAX_CACHE_ENTRIES {
            assert!(dedup.should_log("test", &format!("ctx{}", i)));
        }

        // New entry should still work (triggers eviction)
        assert!(dedup.should_log("test", "new_context"));
    }
}
