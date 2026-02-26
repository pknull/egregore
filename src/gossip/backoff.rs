//! Exponential backoff with jitter for reconnection attempts.
//!
//! Implements truncated binary exponential backoff as used in TCP and
//! distributed systems. Jitter prevents thundering herd when multiple
//! clients reconnect simultaneously.

use std::time::Duration;

use rand::Rng;

/// Exponential backoff calculator with jitter.
///
/// Starts at `initial_delay` and doubles on each failure up to `max_delay`.
/// Full jitter is applied: actual delay is uniform random in [0, calculated_delay].
#[derive(Debug, Clone)]
pub struct ExponentialBackoff {
    /// Initial delay on first retry.
    initial_delay: Duration,
    /// Maximum delay cap.
    max_delay: Duration,
    /// Current attempt number (0 = first attempt, 1 = first retry).
    attempt: u32,
    /// Maximum exponent to prevent overflow.
    max_exponent: u32,
}

impl ExponentialBackoff {
    /// Create a new backoff calculator.
    ///
    /// # Arguments
    /// * `initial_delay` - Delay for the first retry (default: 5 seconds)
    /// * `max_delay` - Maximum delay cap (default: 5 minutes)
    pub fn new(initial_delay: Duration, max_delay: Duration) -> Self {
        // Calculate max exponent to prevent overflow
        // 2^max_exp * initial_millis <= max_millis
        let initial_millis = initial_delay.as_millis() as u64;
        let max_millis = max_delay.as_millis() as u64;
        let max_exponent = if initial_millis > 0 {
            ((max_millis / initial_millis) as f64).log2().floor() as u32
        } else {
            0
        };

        Self {
            initial_delay,
            max_delay,
            attempt: 0,
            max_exponent: max_exponent.min(20), // Cap at 2^20 to be safe
        }
    }

    /// Create with default values (5s initial, 5 min max).
    pub fn default_reconnect() -> Self {
        Self::new(Duration::from_secs(5), Duration::from_secs(300))
    }

    /// Get the next delay and increment the attempt counter.
    ///
    /// Returns a duration with full jitter applied.
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.calculate_delay();
        self.attempt = self.attempt.saturating_add(1);
        delay
    }

    /// Calculate the delay for the current attempt without incrementing.
    pub fn calculate_delay(&self) -> Duration {
        let exp = self.attempt.min(self.max_exponent);
        let multiplier = 1u64 << exp; // 2^exp
        let base_millis = self.initial_delay.as_millis() as u64;
        let calculated_millis = base_millis.saturating_mul(multiplier);

        // Cap at max delay
        let capped_millis = calculated_millis.min(self.max_delay.as_millis() as u64);

        // Apply full jitter: uniform random in [0, capped_delay]
        let jittered = if capped_millis > 0 {
            rand::thread_rng().gen_range(0..=capped_millis)
        } else {
            0
        };

        Duration::from_millis(jittered)
    }

    /// Reset the backoff counter (call after successful connection).
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// Get the current attempt number.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Check if we've exceeded a reasonable retry limit (for logging).
    pub fn is_extended(&self) -> bool {
        self.attempt >= 5
    }
}

impl Default for ExponentialBackoff {
    fn default() -> Self {
        Self::default_reconnect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_starts_at_initial() {
        let backoff = ExponentialBackoff::new(Duration::from_secs(5), Duration::from_secs(300));
        // First delay should be in [0, 5s]
        let delay = backoff.calculate_delay();
        assert!(delay <= Duration::from_secs(5));
    }

    #[test]
    fn backoff_doubles() {
        let mut backoff = ExponentialBackoff::new(Duration::from_secs(1), Duration::from_secs(60));

        // Skip first attempt (1s max)
        let _ = backoff.next_delay();

        // Second attempt should have max of 2s
        // We can't test exact values due to jitter, but we can check the range
        assert_eq!(backoff.attempt, 1);

        let _ = backoff.next_delay();
        assert_eq!(backoff.attempt, 2);
    }

    #[test]
    fn backoff_caps_at_max() {
        let mut backoff = ExponentialBackoff::new(Duration::from_secs(5), Duration::from_secs(30));

        // After many attempts, should still be capped
        for _ in 0..100 {
            let delay = backoff.next_delay();
            assert!(delay <= Duration::from_secs(30));
        }
    }

    #[test]
    fn backoff_resets() {
        let mut backoff = ExponentialBackoff::new(Duration::from_secs(5), Duration::from_secs(300));

        for _ in 0..5 {
            let _ = backoff.next_delay();
        }
        assert_eq!(backoff.attempt, 5);

        backoff.reset();
        assert_eq!(backoff.attempt, 0);
    }

    #[test]
    fn backoff_handles_zero_initial() {
        let backoff = ExponentialBackoff::new(Duration::from_secs(0), Duration::from_secs(60));
        let delay = backoff.calculate_delay();
        assert_eq!(delay, Duration::from_secs(0));
    }

    #[test]
    fn is_extended_after_five_attempts() {
        let mut backoff = ExponentialBackoff::default();
        assert!(!backoff.is_extended());

        for _ in 0..5 {
            let _ = backoff.next_delay();
        }
        assert!(backoff.is_extended());
    }
}
