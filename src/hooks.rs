//! Hook executor — spawns subprocesses and webhooks for event-driven message handling.
//!
//! When configured, the hook executor receives messages from the FeedEngine's
//! broadcast channel and executes all matching hooks in parallel.
//!
//! Each hook entry can define:
//! - A subprocess (on_message) — message JSON piped to stdin
//! - A webhook URL — message JSON POSTed
//! - A content type filter — only fire for matching messages
//! - A timeout — per-hook execution deadline
//!
//! ## Idempotency
//!
//! When `idempotent: true` is set on a hook entry, the executor tracks
//! hook execution state using message hash + hook ID as the idempotency key.
//! This prevents duplicate execution when messages are replayed or when
//! the node restarts mid-processing.
//!
//! State transitions:
//! - PENDING → PROCESSING (hook starts)
//! - PROCESSING → COMPLETED (hook succeeds)
//! - PROCESSING → FAILED (hook fails/times out, may retry)
//! - FAILED → PROCESSING (retry attempt)

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use reqwest::Client;
use tokio::time::timeout;

use crate::config::HookEntry;
use crate::feed::models::Message;

/// State of a hook execution for idempotency tracking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookState {
    /// Hook execution is in progress.
    Processing {
        started_at: Instant,
        attempt: u32,
    },
    /// Hook completed successfully.
    Completed {
        completed_at: Instant,
    },
    /// Hook failed (may be retried).
    Failed {
        failed_at: Instant,
        attempt: u32,
        error: String,
    },
}

impl HookState {
    /// Check if the hook completed successfully.
    #[cfg(test)]
    pub fn is_completed(&self) -> bool {
        matches!(self, HookState::Completed { .. })
    }

    /// Check if the hook is currently processing.
    #[cfg(test)]
    pub fn is_processing(&self) -> bool {
        matches!(self, HookState::Processing { .. })
    }

    /// Get the attempt count for this state.
    #[cfg(test)]
    pub fn attempt_count(&self) -> u32 {
        match self {
            HookState::Processing { attempt, .. } => *attempt,
            HookState::Failed { attempt, .. } => *attempt,
            HookState::Completed { .. } => 0,
        }
    }
}

/// Tracks hook execution state for idempotency.
/// Uses message hash + hook ID as the key.
#[derive(Debug, Default)]
pub struct IdempotencyTracker {
    /// Maps (message_hash, hook_id) → HookState
    state: DashMap<(String, String), HookState>,
}

impl IdempotencyTracker {
    pub fn new() -> Self {
        Self {
            state: DashMap::new(),
        }
    }

    /// Generate idempotency key from message hash and hook ID.
    fn key(message_hash: &str, hook_id: &str) -> (String, String) {
        (message_hash.to_string(), hook_id.to_string())
    }

    /// Check if hook should be executed. Returns Ok(attempt_number) if should execute,
    /// Err(reason) if should skip.
    pub fn should_execute(
        &self,
        message_hash: &str,
        hook: &HookEntry,
    ) -> Result<u32, String> {
        let key = Self::key(message_hash, &hook.unique_id());

        match self.state.get(&key) {
            Some(entry) => {
                let state = entry.value();
                match state {
                    HookState::Completed { .. } => {
                        Err("already completed".to_string())
                    }
                    HookState::Processing { started_at, attempt } => {
                        // Check for stale processing state (hook might have crashed)
                        let timeout_secs = hook.timeout_secs.unwrap_or(30);
                        let stale_threshold = Duration::from_secs(timeout_secs * 2);
                        if started_at.elapsed() > stale_threshold {
                            // Stale processing state, allow retry
                            if *attempt < hook.max_retries {
                                Ok(attempt + 1)
                            } else {
                                Err(format!("max retries ({}) exceeded", hook.max_retries))
                            }
                        } else {
                            Err("currently processing".to_string())
                        }
                    }
                    HookState::Failed { attempt, .. } => {
                        if *attempt < hook.max_retries {
                            Ok(attempt + 1)
                        } else {
                            Err(format!("max retries ({}) exceeded", hook.max_retries))
                        }
                    }
                }
            }
            None => Ok(0), // First attempt
        }
    }

    /// Mark hook as processing.
    pub fn mark_processing(&self, message_hash: &str, hook_id: &str, attempt: u32) {
        let key = Self::key(message_hash, hook_id);
        self.state.insert(
            key,
            HookState::Processing {
                started_at: Instant::now(),
                attempt,
            },
        );
    }

    /// Mark hook as completed successfully.
    pub fn mark_completed(&self, message_hash: &str, hook_id: &str) {
        let key = Self::key(message_hash, hook_id);
        self.state.insert(
            key,
            HookState::Completed {
                completed_at: Instant::now(),
            },
        );
    }

    /// Mark hook as failed.
    pub fn mark_failed(&self, message_hash: &str, hook_id: &str, attempt: u32, error: &str) {
        let key = Self::key(message_hash, hook_id);
        self.state.insert(
            key,
            HookState::Failed {
                failed_at: Instant::now(),
                attempt,
                error: error.to_string(),
            },
        );
    }

    /// Get current state for a hook.
    pub fn get_state(&self, message_hash: &str, hook_id: &str) -> Option<HookState> {
        let key = Self::key(message_hash, hook_id);
        self.state.get(&key).map(|entry| entry.value().clone())
    }

    /// Get all states (for debugging/metrics).
    pub fn all_states(&self) -> HashMap<(String, String), HookState> {
        self.state.iter().map(|e| (e.key().clone(), e.value().clone())).collect()
    }

    /// Count of tracked entries.
    pub fn len(&self) -> usize {
        self.state.len()
    }

    /// Check if tracker is empty.
    pub fn is_empty(&self) -> bool {
        self.state.is_empty()
    }

    /// Clear old completed entries to prevent unbounded memory growth.
    /// Keeps entries newer than max_age.
    pub fn cleanup(&self, max_age: Duration) {
        let now = Instant::now();
        self.state.retain(|_, state| {
            match state {
                HookState::Completed { completed_at } => {
                    now.duration_since(*completed_at) < max_age
                }
                HookState::Failed { failed_at, .. } => {
                    now.duration_since(*failed_at) < max_age
                }
                HookState::Processing { started_at, .. } => {
                    // Keep processing entries for longer (might still be running)
                    now.duration_since(*started_at) < max_age * 2
                }
            }
        });
    }
}

/// Result of hook execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookResult {
    /// Hook executed successfully.
    Success,
    /// Hook was skipped (already completed or max retries).
    Skipped { reason: String },
    /// Hook failed with error.
    Failed { error: String },
    /// Hook timed out.
    Timeout,
}

impl HookResult {
    pub fn is_success(&self) -> bool {
        matches!(self, HookResult::Success)
    }
}

/// Executes hook scripts and webhooks when messages arrive.
pub struct HookExecutor {
    hooks: Vec<HookEntry>,
    http_client: Client,
    tracker: Arc<IdempotencyTracker>,
}

impl HookExecutor {
    /// Create a hook executor if any hooks are configured.
    /// Returns None if the list is empty or all entries are inactive.
    pub fn new(hooks: Vec<HookEntry>) -> Option<Self> {
        Self::with_tracker(hooks, Arc::new(IdempotencyTracker::new()))
    }

    /// Create a hook executor with a shared idempotency tracker.
    pub fn with_tracker(hooks: Vec<HookEntry>, tracker: Arc<IdempotencyTracker>) -> Option<Self> {
        let active: Vec<HookEntry> = hooks.into_iter().filter(|h| h.is_active()).collect();
        if active.is_empty() {
            return None;
        }
        Some(Self {
            hooks: active,
            http_client: Client::new(),
            tracker,
        })
    }

    /// Get the idempotency tracker (for testing/metrics).
    pub fn tracker(&self) -> &IdempotencyTracker {
        &self.tracker
    }

    /// Execute all configured hooks for a message in parallel.
    pub async fn execute(&self, msg: &Message) {
        let futures: Vec<_> = self.hooks.iter()
            .map(|hook| self.execute_one(hook, msg))
            .collect();
        futures::future::join_all(futures).await;
    }

    /// Execute a single hook entry (subprocess + webhook) with idempotency.
    async fn execute_one(&self, hook: &HookEntry, msg: &Message) {
        let hook_id = hook.unique_id();

        // Check idempotency if enabled
        let attempt = if hook.idempotent {
            match self.tracker.should_execute(&msg.hash, hook) {
                Ok(attempt) => {
                    self.tracker.mark_processing(&msg.hash, &hook_id, attempt);
                    attempt
                }
                Err(reason) => {
                    tracing::debug!(
                        hook_name = ?hook.name,
                        message_hash = %msg.hash,
                        reason = %reason,
                        "skipping hook execution"
                    );
                    return;
                }
            }
        } else {
            0
        };

        // Execute subprocess if configured
        let subprocess_result = if let Some(ref path) = hook.on_message {
            Some(self.execute_subprocess(msg, path.clone(), hook.timeout_secs, &hook.name).await)
        } else {
            None
        };

        // Execute webhook if configured
        let webhook_result = if let Some(ref url) = hook.webhook_url {
            Some(self.execute_webhook(msg, url, hook.timeout_secs, &hook.name).await)
        } else {
            None
        };

        // Update idempotency state based on results
        if hook.idempotent {
            let all_success = subprocess_result.as_ref().is_none_or(|r| r.is_success())
                && webhook_result.as_ref().is_none_or(|r| r.is_success());

            if all_success {
                self.tracker.mark_completed(&msg.hash, &hook_id);
                tracing::debug!(
                    hook_name = ?hook.name,
                    message_hash = %msg.hash,
                    attempt,
                    "hook completed successfully"
                );
            } else {
                let error = match (&subprocess_result, &webhook_result) {
                    (Some(HookResult::Failed { error }), _) => error.clone(),
                    (Some(HookResult::Timeout), _) => "subprocess timeout".to_string(),
                    (_, Some(HookResult::Failed { error })) => error.clone(),
                    (_, Some(HookResult::Timeout)) => "webhook timeout".to_string(),
                    _ => "unknown error".to_string(),
                };
                self.tracker.mark_failed(&msg.hash, &hook_id, attempt, &error);
                tracing::warn!(
                    hook_name = ?hook.name,
                    message_hash = %msg.hash,
                    attempt,
                    max_retries = hook.max_retries,
                    error = %error,
                    "hook failed"
                );
            }
        }
    }

    /// Execute subprocess hook with message JSON on stdin.
    async fn execute_subprocess(
        &self,
        msg: &Message,
        path: std::path::PathBuf,
        timeout_secs: Option<u64>,
        name: &Option<String>,
    ) -> HookResult {
        let json = match serde_json::to_string(msg) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(hook_name = ?name, error = %e, "failed to serialize message for hook");
                return HookResult::Failed {
                    error: format!("serialization failed: {}", e),
                };
            }
        };

        let timeout_secs = timeout_secs.unwrap_or(30);
        let hook_timeout = Duration::from_secs(timeout_secs);
        let hook_path = path.clone();

        // Clone trace context for the blocking task
        let trace_id = msg.trace_id.clone();
        let span_id = msg.span_id.clone();

        let result = timeout(hook_timeout, async {
            tokio::task::spawn_blocking(move || {
                let mut cmd = Command::new(&hook_path);
                cmd.stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());

                // Propagate trace context as environment variables
                if let Some(ref tid) = trace_id {
                    cmd.env("EGREGORE_TRACE_ID", tid);
                }
                if let Some(ref sid) = span_id {
                    cmd.env("EGREGORE_SPAN_ID", sid);
                }

                let mut child = cmd.spawn()?;

                if let Some(ref mut stdin) = child.stdin {
                    stdin.write_all(json.as_bytes())?;
                }
                // Close stdin so the child knows input is complete
                drop(child.stdin.take());

                child.wait()
            })
            .await
        })
        .await;

        match result {
            Ok(Ok(Ok(status))) => {
                if status.success() {
                    tracing::debug!(hook_name = ?name, hook = ?path, "hook completed successfully");
                    HookResult::Success
                } else {
                    let error = format!("exit code: {:?}", status.code());
                    tracing::warn!(hook_name = ?name, hook = ?path, code = ?status.code(), "hook exited with error");
                    HookResult::Failed { error }
                }
            }
            Ok(Ok(Err(e))) => {
                tracing::warn!(hook_name = ?name, hook = ?path, error = %e, "hook execution failed");
                HookResult::Failed {
                    error: e.to_string(),
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(hook_name = ?name, hook = ?path, error = %e, "hook task panicked");
                HookResult::Failed {
                    error: format!("task panicked: {}", e),
                }
            }
            Err(_) => {
                tracing::warn!(hook_name = ?name, hook = ?path, timeout_secs, "hook timed out");
                HookResult::Timeout
            }
        }
    }

    /// Execute webhook by POSTing message JSON to URL.
    async fn execute_webhook(
        &self,
        msg: &Message,
        url: &str,
        timeout_secs: Option<u64>,
        name: &Option<String>,
    ) -> HookResult {
        let timeout_secs = timeout_secs.unwrap_or(30);
        let hook_timeout = Duration::from_secs(timeout_secs);

        let result = timeout(hook_timeout, async {
            let mut request = self.http_client.post(url).json(msg);

            // Propagate trace context as HTTP headers
            if let Some(ref trace_id) = msg.trace_id {
                request = request.header("X-Trace-Id", trace_id);
            }
            if let Some(ref span_id) = msg.span_id {
                request = request.header("X-Span-Id", span_id);
            }

            request.send().await
        })
        .await;

        match result {
            Ok(Ok(response)) => {
                if response.status().is_success() {
                    tracing::debug!(hook_name = ?name, url = %url, status = %response.status(), "webhook completed");
                    HookResult::Success
                } else {
                    let error = format!("HTTP {}", response.status());
                    tracing::warn!(hook_name = ?name, url = %url, status = %response.status(), "webhook returned error");
                    HookResult::Failed { error }
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(hook_name = ?name, url = %url, error = %e, "webhook request failed");
                HookResult::Failed {
                    error: e.to_string(),
                }
            }
            Err(_) => {
                tracing::warn!(hook_name = ?name, url = %url, timeout_secs, "webhook timed out");
                HookResult::Timeout
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn executor_returns_none_without_hooks() {
        assert!(HookExecutor::new(vec![]).is_none());
    }

    #[test]
    fn executor_returns_none_with_inactive_hooks() {
        let hooks = vec![HookEntry::default(), HookEntry::default()];
        assert!(HookExecutor::new(hooks).is_none());
    }

    #[test]
    fn executor_returns_some_with_path() {
        let hooks = vec![HookEntry {
            on_message: Some(PathBuf::from("/bin/true")),
            ..Default::default()
        }];
        assert!(HookExecutor::new(hooks).is_some());
    }

    #[test]
    fn executor_returns_some_with_webhook_url() {
        let hooks = vec![HookEntry {
            webhook_url: Some("https://example.com/webhook".to_string()),
            ..Default::default()
        }];
        assert!(HookExecutor::new(hooks).is_some());
    }

    #[test]
    fn executor_filters_inactive_hooks() {
        let hooks = vec![
            HookEntry::default(), // inactive
            HookEntry {
                on_message: Some(PathBuf::from("/bin/true")),
                ..Default::default()
            },
            HookEntry::default(), // inactive
        ];
        let executor = HookExecutor::new(hooks).unwrap();
        assert_eq!(executor.hooks.len(), 1);
    }

    // Idempotency tracker tests

    #[test]
    fn tracker_allows_first_execution() {
        let tracker = IdempotencyTracker::new();
        let hook = HookEntry {
            name: Some("test".to_string()),
            on_message: Some(PathBuf::from("/bin/true")),
            idempotent: true,
            ..Default::default()
        };
        let result = tracker.should_execute("hash123", &hook);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }

    #[test]
    fn tracker_blocks_completed_execution() {
        let tracker = IdempotencyTracker::new();
        let hook = HookEntry {
            name: Some("test".to_string()),
            on_message: Some(PathBuf::from("/bin/true")),
            idempotent: true,
            ..Default::default()
        };

        tracker.mark_completed("hash123", "test");

        let result = tracker.should_execute("hash123", &hook);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("already completed"));
    }

    #[test]
    fn tracker_blocks_processing_execution() {
        let tracker = IdempotencyTracker::new();
        let hook = HookEntry {
            name: Some("test".to_string()),
            on_message: Some(PathBuf::from("/bin/true")),
            idempotent: true,
            timeout_secs: Some(30),
            ..Default::default()
        };

        tracker.mark_processing("hash123", "test", 0);

        let result = tracker.should_execute("hash123", &hook);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("currently processing"));
    }

    #[test]
    fn tracker_allows_retry_after_failure() {
        let tracker = IdempotencyTracker::new();
        let hook = HookEntry {
            name: Some("test".to_string()),
            on_message: Some(PathBuf::from("/bin/true")),
            idempotent: true,
            max_retries: 3,
            ..Default::default()
        };

        tracker.mark_failed("hash123", "test", 0, "test error");

        let result = tracker.should_execute("hash123", &hook);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 1); // Second attempt
    }

    #[test]
    fn tracker_blocks_after_max_retries() {
        let tracker = IdempotencyTracker::new();
        let hook = HookEntry {
            name: Some("test".to_string()),
            on_message: Some(PathBuf::from("/bin/true")),
            idempotent: true,
            max_retries: 2,
            ..Default::default()
        };

        tracker.mark_failed("hash123", "test", 2, "test error");

        let result = tracker.should_execute("hash123", &hook);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("max retries"));
    }

    #[test]
    fn tracker_cleanup_removes_old_entries() {
        let tracker = IdempotencyTracker::new();

        // Add a completed entry
        tracker.mark_completed("hash123", "test");
        assert_eq!(tracker.len(), 1);

        // Cleanup with zero max_age should remove it
        tracker.cleanup(Duration::from_secs(0));
        assert_eq!(tracker.len(), 0);
    }

    #[test]
    fn tracker_state_transitions() {
        let tracker = IdempotencyTracker::new();

        // Initial state: none
        assert!(tracker.get_state("hash123", "test").is_none());

        // Mark processing
        tracker.mark_processing("hash123", "test", 0);
        let state = tracker.get_state("hash123", "test").unwrap();
        assert!(state.is_processing());
        assert_eq!(state.attempt_count(), 0);

        // Mark completed
        tracker.mark_completed("hash123", "test");
        let state = tracker.get_state("hash123", "test").unwrap();
        assert!(state.is_completed());
    }

    #[test]
    fn hook_unique_id_uses_name() {
        let hook = HookEntry {
            name: Some("my-hook".to_string()),
            on_message: Some(PathBuf::from("/bin/true")),
            ..Default::default()
        };
        assert_eq!(hook.unique_id(), "my-hook");
    }

    #[test]
    fn hook_unique_id_falls_back_to_path() {
        let hook = HookEntry {
            on_message: Some(PathBuf::from("/bin/true")),
            ..Default::default()
        };
        assert!(hook.unique_id().contains("subprocess"));
        assert!(hook.unique_id().contains("/bin/true"));
    }

    #[test]
    fn hook_unique_id_falls_back_to_url() {
        let hook = HookEntry {
            webhook_url: Some("https://example.com".to_string()),
            ..Default::default()
        };
        assert!(hook.unique_id().contains("webhook"));
        assert!(hook.unique_id().contains("example.com"));
    }

    #[test]
    fn hook_result_is_success() {
        assert!(HookResult::Success.is_success());
        assert!(!HookResult::Timeout.is_success());
        assert!(!HookResult::Failed { error: "test".to_string() }.is_success());
        assert!(!HookResult::Skipped { reason: "test".to_string() }.is_success());
    }
}
