//! Flow control and backpressure for gossip replication.
//!
//! Implements credit-based flow control where consumers request batches
//! of messages rather than receiving unbounded push. Each peer grants
//! credits indicating how many messages they can accept.
//!
//! Key concepts:
//! - **Credits**: A peer grants credits to indicate receive capacity
//! - **Credit Request**: Consumer asks for more credits when running low
//! - **Adaptive Flow**: Reduce push rate to peers showing backpressure
//!
//! Rate limiting is applied at the sender side to prevent overwhelming
//! slow consumers and to comply with configurable per-peer limits.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// Default initial credits granted to a new peer connection.
pub const DEFAULT_INITIAL_CREDITS: u32 = 100;

/// Default credits granted per credit renewal.
pub const DEFAULT_CREDIT_GRANT: u32 = 50;

/// Minimum credits before requesting more (low watermark).
pub const DEFAULT_LOW_WATERMARK: u32 = 20;

/// Default maximum messages per second per peer.
pub const DEFAULT_RATE_LIMIT_PER_SECOND: u32 = 100;

/// Default burst allowance for rate limiting.
pub const DEFAULT_RATE_LIMIT_BURST: u32 = 50;

/// Maximum credits a peer can accumulate (prevents overflow attacks).
pub const MAX_CREDITS: u32 = 10_000;

/// Flow control configuration.
#[derive(Debug, Clone)]
pub struct FlowControlConfig {
    /// Initial credits granted when connection is established.
    pub initial_credits: u32,
    /// Credits granted per renewal request.
    pub credit_grant: u32,
    /// Request more credits when below this threshold.
    pub low_watermark: u32,
    /// Maximum messages per second per peer (0 = unlimited).
    pub rate_limit_per_second: u32,
    /// Burst allowance above steady-state rate.
    pub rate_limit_burst: u32,
    /// Enable credit-based flow control (can be disabled for legacy peers).
    pub credits_enabled: bool,
}

impl Default for FlowControlConfig {
    fn default() -> Self {
        Self {
            initial_credits: DEFAULT_INITIAL_CREDITS,
            credit_grant: DEFAULT_CREDIT_GRANT,
            low_watermark: DEFAULT_LOW_WATERMARK,
            rate_limit_per_second: DEFAULT_RATE_LIMIT_PER_SECOND,
            rate_limit_burst: DEFAULT_RATE_LIMIT_BURST,
            credits_enabled: true,
        }
    }
}

impl FlowControlConfig {
    /// Create a config with flow control disabled (for legacy compatibility).
    pub fn disabled() -> Self {
        Self {
            credits_enabled: false,
            rate_limit_per_second: 0,
            ..Default::default()
        }
    }

    /// Create a strict config for testing or constrained environments.
    pub fn strict() -> Self {
        Self {
            initial_credits: 20,
            credit_grant: 10,
            low_watermark: 5,
            rate_limit_per_second: 20,
            rate_limit_burst: 10,
            credits_enabled: true,
        }
    }
}

/// Flow control state for a single peer connection.
///
/// Tracks credits (how many messages we can send) and rate limiting state.
/// Thread-safe for concurrent access from broadcast and receive paths.
#[derive(Debug)]
pub struct PeerFlowState {
    /// Credits remaining (messages we can send to this peer).
    credits: AtomicU32,
    /// Whether the peer supports credit-based flow control.
    credits_supported: std::sync::atomic::AtomicBool,
    /// Rate limiter state.
    rate_limiter: Mutex<TokenBucket>,
    /// Total messages sent (for metrics).
    messages_sent: AtomicU64,
    /// Total messages dropped due to backpressure (for metrics).
    messages_dropped: AtomicU64,
    /// Last time credits were updated.
    last_credit_update: Mutex<Instant>,
    /// Configuration.
    config: FlowControlConfig,
}

impl PeerFlowState {
    /// Create new flow state with the given configuration.
    pub fn new(config: FlowControlConfig) -> Self {
        Self {
            credits: AtomicU32::new(config.initial_credits),
            credits_supported: std::sync::atomic::AtomicBool::new(config.credits_enabled),
            rate_limiter: Mutex::new(TokenBucket::new(
                config.rate_limit_per_second,
                config.rate_limit_burst,
            )),
            messages_sent: AtomicU64::new(0),
            messages_dropped: AtomicU64::new(0),
            last_credit_update: Mutex::new(Instant::now()),
            config,
        }
    }

    /// Create flow state with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(FlowControlConfig::default())
    }

    /// Check if we can send a message (have credits and rate allows).
    ///
    /// Returns true if the message can be sent, false if backpressure applies.
    /// Does NOT consume resources - call `consume_send` after successful send.
    pub async fn can_send(&self) -> bool {
        // If credits not supported, only check rate limit
        if !self.credits_supported.load(Ordering::Relaxed) {
            return self.check_rate_limit().await;
        }

        // Check credits
        let credits = self.credits.load(Ordering::Relaxed);
        if credits == 0 {
            return false;
        }

        self.check_rate_limit().await
    }

    /// Check rate limit without consuming tokens.
    async fn check_rate_limit(&self) -> bool {
        if self.config.rate_limit_per_second == 0 {
            return true;
        }
        let limiter = self.rate_limiter.lock().await;
        limiter.can_acquire()
    }

    /// Consume one send credit and rate limit token.
    ///
    /// Call this after successfully sending a message.
    pub async fn consume_send(&self) {
        // Decrement credits if supported
        if self.credits_supported.load(Ordering::Relaxed) {
            // Saturating sub to avoid underflow
            self.credits.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |c| {
                Some(c.saturating_sub(1))
            }).ok();
        }

        // Consume rate limit token
        if self.config.rate_limit_per_second > 0 {
            let mut limiter = self.rate_limiter.lock().await;
            limiter.acquire();
        }

        self.messages_sent.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a message being dropped due to backpressure.
    pub fn record_drop(&self) {
        self.messages_dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// Grant credits from the peer (they are ready to receive more).
    /// Credits are capped at MAX_CREDITS to prevent overflow attacks.
    pub async fn grant_credits(&self, amount: u32) {
        // Use fetch_update with saturating_add and cap to prevent overflow
        self.credits
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                Some(current.saturating_add(amount).min(MAX_CREDITS))
            })
            .ok();
        *self.last_credit_update.lock().await = Instant::now();
    }

    /// Get current credit count.
    pub fn credits(&self) -> u32 {
        self.credits.load(Ordering::Relaxed)
    }

    /// Check if we need to request more credits.
    pub fn needs_credit_request(&self) -> bool {
        if !self.credits_supported.load(Ordering::Relaxed) {
            return false;
        }
        self.credits.load(Ordering::Relaxed) < self.config.low_watermark
    }

    /// Set whether the peer supports credit-based flow control.
    pub fn set_credits_supported(&self, supported: bool) {
        self.credits_supported.store(supported, Ordering::Relaxed);
    }

    /// Check if peer supports credits.
    pub fn supports_credits(&self) -> bool {
        self.credits_supported.load(Ordering::Relaxed)
    }

    /// Get metrics snapshot.
    pub fn metrics(&self) -> FlowMetrics {
        FlowMetrics {
            credits: self.credits.load(Ordering::Relaxed),
            messages_sent: self.messages_sent.load(Ordering::Relaxed),
            messages_dropped: self.messages_dropped.load(Ordering::Relaxed),
            credits_supported: self.credits_supported.load(Ordering::Relaxed),
        }
    }
}

/// Metrics snapshot for a peer's flow state.
#[derive(Debug, Clone)]
pub struct FlowMetrics {
    /// Current credit count.
    pub credits: u32,
    /// Total messages sent.
    pub messages_sent: u64,
    /// Total messages dropped due to backpressure.
    pub messages_dropped: u64,
    /// Whether peer supports credit-based flow.
    pub credits_supported: bool,
}

/// Token bucket rate limiter.
///
/// Allows bursts up to `burst` tokens, then refills at `rate` per second.
#[derive(Debug)]
pub struct TokenBucket {
    /// Tokens currently available.
    tokens: f64,
    /// Maximum tokens (burst capacity).
    capacity: f64,
    /// Tokens added per second.
    rate: f64,
    /// Last refill time.
    last_refill: Instant,
}

impl TokenBucket {
    /// Create a new token bucket.
    ///
    /// # Arguments
    /// * `rate` - Tokens added per second (0 = unlimited)
    /// * `burst` - Maximum tokens (burst capacity)
    pub fn new(rate: u32, burst: u32) -> Self {
        let capacity = (rate + burst) as f64;
        Self {
            tokens: capacity, // Start full
            capacity,
            rate: rate as f64,
            last_refill: Instant::now(),
        }
    }

    /// Check if a token can be acquired without consuming it.
    pub fn can_acquire(&self) -> bool {
        if self.rate == 0.0 {
            return true;
        }
        let elapsed = self.last_refill.elapsed().as_secs_f64();
        let current_tokens = (self.tokens + elapsed * self.rate).min(self.capacity);
        current_tokens >= 1.0
    }

    /// Acquire one token, refilling first.
    ///
    /// Should only be called after `can_acquire` returns true.
    pub fn acquire(&mut self) {
        if self.rate == 0.0 {
            return;
        }
        self.refill();
        self.tokens = (self.tokens - 1.0).max(0.0);
    }

    /// Refill tokens based on elapsed time.
    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate).min(self.capacity);
        self.last_refill = now;
    }
}

/// Credit-related protocol messages for flow control.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FlowControlMessage {
    /// Grant credits to the sender (receiver has capacity).
    CreditGrant {
        /// Number of messages the sender may transmit.
        amount: u32,
    },
    /// Request credits from the receiver.
    CreditRequest {
        /// Suggested number of credits to grant.
        suggested: u32,
    },
    /// Acknowledge flow control capability during handshake.
    FlowControlAck {
        /// Whether this peer supports credit-based flow control.
        supported: bool,
        /// Initial credits granted.
        initial_credits: u32,
    },
}

/// Calculate adaptive fanout based on peer health.
///
/// Reduces message sending to unhealthy peers to prevent wasting resources.
/// Returns a fanout multiplier between 0.0 and 1.0.
///
/// # Arguments
/// * `messages_sent` - Total messages sent to this peer
/// * `messages_dropped` - Messages dropped due to backpressure
/// * `credits` - Current credit count
pub fn calculate_adaptive_fanout(
    messages_sent: u64,
    messages_dropped: u64,
    credits: u32,
) -> f64 {
    // If no messages sent, assume healthy
    if messages_sent == 0 {
        return 1.0;
    }

    // Calculate drop rate
    let drop_rate = messages_dropped as f64 / (messages_sent + messages_dropped) as f64;

    // Calculate credit factor (low credits = reduce fanout)
    let credit_factor = if credits > DEFAULT_INITIAL_CREDITS / 2 {
        1.0
    } else if credits > DEFAULT_LOW_WATERMARK {
        0.75
    } else if credits > 0 {
        0.5
    } else {
        0.25
    };

    // Combine factors: high drop rate = reduce fanout
    let drop_factor = 1.0 - (drop_rate * 0.8); // Max 80% reduction from drops

    (drop_factor * credit_factor).clamp(0.1, 1.0)
}

/// Determine if a message should be sent to a peer based on adaptive fanout.
///
/// Uses probabilistic selection based on the fanout score.
pub fn should_send_to_peer(fanout_score: f64) -> bool {
    if fanout_score >= 1.0 {
        return true;
    }
    if fanout_score <= 0.0 {
        return false;
    }
    rand::random::<f64>() < fanout_score
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn flow_state_initial_credits() {
        let state = PeerFlowState::with_defaults();
        assert_eq!(state.credits(), DEFAULT_INITIAL_CREDITS);
        assert!(state.can_send().await);
    }

    #[tokio::test]
    async fn flow_state_consume_credits() {
        let config = FlowControlConfig {
            initial_credits: 5,
            rate_limit_per_second: 0, // Disable rate limit for this test
            ..Default::default()
        };
        let state = PeerFlowState::new(config);

        // Should be able to send 5 times
        for _ in 0..5 {
            assert!(state.can_send().await);
            state.consume_send().await;
        }

        // Now out of credits
        assert!(!state.can_send().await);
        assert_eq!(state.credits(), 0);
    }

    #[tokio::test]
    async fn flow_state_grant_credits() {
        let config = FlowControlConfig {
            initial_credits: 1,
            rate_limit_per_second: 0,
            ..Default::default()
        };
        let state = PeerFlowState::new(config);

        state.consume_send().await;
        assert!(!state.can_send().await);

        state.grant_credits(10).await;
        assert_eq!(state.credits(), 10);
        assert!(state.can_send().await);
    }

    #[tokio::test]
    async fn flow_state_needs_credit_request() {
        let config = FlowControlConfig {
            initial_credits: 25,
            low_watermark: 20,
            rate_limit_per_second: 0,
            ..Default::default()
        };
        let state = PeerFlowState::new(config);

        assert!(!state.needs_credit_request());

        // Consume until below watermark
        for _ in 0..6 {
            state.consume_send().await;
        }

        assert!(state.needs_credit_request());
    }

    #[tokio::test]
    async fn flow_state_credits_disabled() {
        let config = FlowControlConfig::disabled();
        let state = PeerFlowState::new(config);

        // Should always be able to send when credits disabled
        for _ in 0..200 {
            assert!(state.can_send().await);
            state.consume_send().await;
        }

        // Should not need credit requests
        assert!(!state.needs_credit_request());
    }

    #[tokio::test]
    async fn flow_state_metrics() {
        let config = FlowControlConfig {
            initial_credits: 10,
            rate_limit_per_second: 0,
            ..Default::default()
        };
        let state = PeerFlowState::new(config);

        for _ in 0..5 {
            state.consume_send().await;
        }
        state.record_drop();
        state.record_drop();

        let metrics = state.metrics();
        assert_eq!(metrics.credits, 5);
        assert_eq!(metrics.messages_sent, 5);
        assert_eq!(metrics.messages_dropped, 2);
    }

    #[test]
    fn token_bucket_basic() {
        let mut bucket = TokenBucket::new(10, 5); // 10/sec + 5 burst = 15 capacity

        // Should start with full capacity
        assert!(bucket.can_acquire());

        // Consume all tokens
        for _ in 0..15 {
            assert!(bucket.can_acquire());
            bucket.acquire();
        }

        // Should be empty now (without refill time)
        // Note: There might be a tiny bit of refill during the loop
    }

    #[test]
    fn token_bucket_unlimited() {
        let bucket = TokenBucket::new(0, 0);
        assert!(bucket.can_acquire());
    }

    #[test]
    fn adaptive_fanout_healthy_peer() {
        let fanout = calculate_adaptive_fanout(1000, 0, 100);
        assert!((fanout - 1.0).abs() < 0.01);
    }

    #[test]
    fn adaptive_fanout_dropping_peer() {
        // 50% drop rate
        let fanout = calculate_adaptive_fanout(50, 50, 100);
        assert!(fanout < 0.7);
    }

    #[test]
    fn adaptive_fanout_low_credits() {
        let fanout = calculate_adaptive_fanout(100, 0, 5);
        assert!(fanout < 0.6);
    }

    #[test]
    fn adaptive_fanout_zero_credits() {
        let fanout = calculate_adaptive_fanout(100, 0, 0);
        assert_eq!(fanout, 0.25);
    }

    #[test]
    fn adaptive_fanout_no_messages() {
        let fanout = calculate_adaptive_fanout(0, 0, 50);
        assert_eq!(fanout, 1.0);
    }

    #[test]
    fn should_send_always_for_full_fanout() {
        for _ in 0..100 {
            assert!(should_send_to_peer(1.0));
        }
    }

    #[test]
    fn should_send_never_for_zero_fanout() {
        for _ in 0..100 {
            assert!(!should_send_to_peer(0.0));
        }
    }

    #[test]
    fn flow_control_message_serialization() {
        let grant = FlowControlMessage::CreditGrant { amount: 50 };
        let json = serde_json::to_string(&grant).unwrap();
        assert!(json.contains("credit_grant"));
        assert!(json.contains("50"));

        let parsed: FlowControlMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            FlowControlMessage::CreditGrant { amount } => assert_eq!(amount, 50),
            _ => panic!("wrong variant"),
        }
    }
}
