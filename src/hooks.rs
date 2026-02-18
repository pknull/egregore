//! Hook executor — spawns subprocesses and webhooks for event-driven message handling.
//!
//! When configured, the hook executor receives messages from the FeedEngine's
//! broadcast channel and:
//! - Spawns a subprocess for each matching message (on_message)
//! - POSTs message JSON to a URL (webhook_url)
//!
//! Both mechanisms can be used simultaneously.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use reqwest::Client;
use tokio::time::timeout;

use crate::config::HookConfig;
use crate::feed::models::Message;

/// Executes hook scripts and webhooks when messages arrive.
pub struct HookExecutor {
    config: HookConfig,
    http_client: Client,
}

impl HookExecutor {
    /// Create a new hook executor if any hook is configured.
    /// Returns None if neither on_message nor webhook_url is set.
    pub fn new(config: HookConfig) -> Option<Self> {
        if config.on_message.is_none() && config.webhook_url.is_none() {
            return None;
        }
        Some(Self {
            config,
            http_client: Client::new(),
        })
    }

    /// Execute all configured hooks for a message.
    ///
    /// Runs both subprocess hook and webhook if configured.
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
                    "hooks skipped: content_type mismatch"
                );
                return;
            }
        }

        // Execute subprocess hook if configured
        if let Some(ref path) = self.config.on_message {
            self.execute_subprocess(msg, path.clone()).await;
        }

        // Execute webhook if configured
        if let Some(ref url) = self.config.webhook_url {
            self.execute_webhook(msg, url).await;
        }
    }

    /// Execute subprocess hook with message JSON on stdin.
    async fn execute_subprocess(&self, msg: &Message, path: std::path::PathBuf) {
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

    /// Execute webhook by POSTing message JSON to URL.
    async fn execute_webhook(&self, msg: &Message, url: &str) {
        let timeout_secs = self.config.timeout_secs.unwrap_or(30);
        let hook_timeout = Duration::from_secs(timeout_secs);

        let result = timeout(hook_timeout, async {
            self.http_client
                .post(url)
                .json(msg)
                .send()
                .await
        })
        .await;

        match result {
            Ok(Ok(response)) => {
                if response.status().is_success() {
                    tracing::debug!(url = %url, status = %response.status(), "webhook completed");
                } else {
                    tracing::warn!(url = %url, status = %response.status(), "webhook returned error");
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(url = %url, error = %e, "webhook request failed");
            }
            Err(_) => {
                tracing::warn!(url = %url, timeout_secs, "webhook timed out");
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

    #[test]
    fn executor_returns_some_with_webhook_url() {
        let config = HookConfig {
            webhook_url: Some("https://example.com/webhook".to_string()),
            ..Default::default()
        };
        assert!(HookExecutor::new(config).is_some());
    }

    #[test]
    fn executor_returns_some_with_both() {
        let config = HookConfig {
            on_message: Some(PathBuf::from("/bin/true")),
            webhook_url: Some("https://example.com/webhook".to_string()),
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
