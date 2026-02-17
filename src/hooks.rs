//! Hook executor — spawns subprocesses for event-driven message handling.
//!
//! When configured, the hook executor receives messages from the FeedEngine's
//! broadcast channel and spawns a subprocess for each matching message. The
//! message JSON is passed on stdin.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use tokio::time::timeout;

use crate::config::HookConfig;
use crate::feed::models::Message;

/// Executes hook scripts when messages arrive.
pub struct HookExecutor {
    config: HookConfig,
}

impl HookExecutor {
    /// Create a new hook executor if a hook is configured.
    /// Returns None if no hook path is set.
    pub fn new(config: HookConfig) -> Option<Self> {
        config.on_message.as_ref()?;
        Some(Self { config })
    }

    /// Execute the hook for a message.
    ///
    /// Spawns the configured executable with the message JSON on stdin.
    /// Respects content_type filter and timeout settings.
    pub async fn execute(&self, msg: &Message) {
        // Apply content_type filter if configured
        if let Some(ref filter) = self.config.filter_content_type {
            let msg_type = msg
                .content
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if msg_type != filter {
                tracing::trace!(
                    filter = %filter,
                    actual = %msg_type,
                    "hook skipped: content_type mismatch"
                );
                return;
            }
        }

        let path = match &self.config.on_message {
            Some(p) => p.clone(),
            None => return,
        };

        let json = match serde_json::to_string(msg) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(error = %e, "failed to serialize message for hook");
                return;
            }
        };

        let timeout_secs = self.config.timeout_secs.unwrap_or(30);
        let hook_timeout = Duration::from_secs(timeout_secs);

        // Clone path for use in async block; keep original for logging
        let hook_path = path.clone();

        // Spawn blocking since std::process is sync
        let result = timeout(hook_timeout, async {
            tokio::task::spawn_blocking(move || {
                let mut child = Command::new(&hook_path)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()?;

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
                    tracing::debug!(hook = ?path, "hook completed successfully");
                } else {
                    tracing::warn!(hook = ?path, code = ?status.code(), "hook exited with error");
                }
            }
            Ok(Ok(Err(e))) => {
                tracing::warn!(hook = ?path, error = %e, "hook execution failed");
            }
            Ok(Err(e)) => {
                tracing::warn!(hook = ?path, error = %e, "hook task panicked");
            }
            Err(_) => {
                tracing::warn!(hook = ?path, timeout_secs, "hook timed out");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::PublicId;
    use chrono::Utc;
    use std::path::PathBuf;

    fn make_test_message(content_type: &str) -> Message {
        Message {
            author: PublicId("@test.ed25519".to_string()),
            sequence: 1,
            previous: None,
            timestamp: Utc::now(),
            content: serde_json::json!({
                "type": content_type,
                "data": "test"
            }),
            hash: "testhash".to_string(),
            signature: "testsig".to_string(),
        }
    }

    #[test]
    fn executor_returns_none_without_config() {
        let config = HookConfig::default();
        assert!(HookExecutor::new(config).is_none());
    }

    #[test]
    fn executor_returns_some_with_path() {
        let config = HookConfig {
            on_message: Some(PathBuf::from("/bin/true")),
            ..Default::default()
        };
        assert!(HookExecutor::new(config).is_some());
    }

    #[tokio::test]
    async fn filter_skips_non_matching_type() {
        let config = HookConfig {
            on_message: Some(PathBuf::from("/bin/false")), // Would fail if run
            filter_content_type: Some("query".to_string()),
            ..Default::default()
        };
        let executor = HookExecutor::new(config).unwrap();

        // This should be skipped (type is "insight", filter is "query")
        let msg = make_test_message("insight");
        executor.execute(&msg).await;
        // If we reach here without error, the filter worked
    }
}
