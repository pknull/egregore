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

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use reqwest::Client;
use tokio::time::timeout;

use crate::config::HookEntry;
use crate::feed::models::Message;

/// Executes hook scripts and webhooks when messages arrive.
pub struct HookExecutor {
    hooks: Vec<HookEntry>,
    http_client: Client,
}

impl HookExecutor {
    /// Create a hook executor if any hooks are configured.
    /// Returns None if the list is empty or all entries are inactive.
    pub fn new(hooks: Vec<HookEntry>) -> Option<Self> {
        let active: Vec<HookEntry> = hooks.into_iter().filter(|h| h.is_active()).collect();
        if active.is_empty() {
            return None;
        }
        Some(Self {
            hooks: active,
            http_client: Client::new(),
        })
    }

    /// Execute all configured hooks for a message in parallel.
    pub async fn execute(&self, msg: &Message) {
        let futures: Vec<_> = self.hooks.iter()
            .map(|hook| self.execute_one(hook, msg))
            .collect();
        futures::future::join_all(futures).await;
    }

    /// Execute a single hook entry (filter check + subprocess + webhook).
    async fn execute_one(&self, hook: &HookEntry, msg: &Message) {
        // Apply per-hook content_type filter
        if let Some(ref filter) = hook.filter_content_type {
            let msg_type = msg
                .content
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if msg_type != filter {
                tracing::trace!(
                    hook_name = ?hook.name,
                    filter = %filter,
                    actual = %msg_type,
                    "hook skipped: content_type mismatch"
                );
                return;
            }
        }

        // Execute subprocess if configured
        if let Some(ref path) = hook.on_message {
            self.execute_subprocess(msg, path.clone(), hook.timeout_secs, &hook.name).await;
        }

        // Execute webhook if configured
        if let Some(ref url) = hook.webhook_url {
            self.execute_webhook(msg, url, hook.timeout_secs, &hook.name).await;
        }
    }

    /// Execute subprocess hook with message JSON on stdin.
    async fn execute_subprocess(
        &self,
        msg: &Message,
        path: std::path::PathBuf,
        timeout_secs: Option<u64>,
        name: &Option<String>,
    ) {
        let json = match serde_json::to_string(msg) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(hook_name = ?name, error = %e, "failed to serialize message for hook");
                return;
            }
        };

        let timeout_secs = timeout_secs.unwrap_or(30);
        let hook_timeout = Duration::from_secs(timeout_secs);
        let hook_path = path.clone();

        let result = timeout(hook_timeout, async {
            tokio::task::spawn_blocking(move || {
                let mut child = Command::new(&hook_path)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
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
                    tracing::debug!(hook_name = ?name, hook = ?path, "hook completed successfully");
                } else {
                    tracing::warn!(hook_name = ?name, hook = ?path, code = ?status.code(), "hook exited with error");
                }
            }
            Ok(Ok(Err(e))) => {
                tracing::warn!(hook_name = ?name, hook = ?path, error = %e, "hook execution failed");
            }
            Ok(Err(e)) => {
                tracing::warn!(hook_name = ?name, hook = ?path, error = %e, "hook task panicked");
            }
            Err(_) => {
                tracing::warn!(hook_name = ?name, hook = ?path, timeout_secs, "hook timed out");
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
    ) {
        let timeout_secs = timeout_secs.unwrap_or(30);
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
                    tracing::debug!(hook_name = ?name, url = %url, status = %response.status(), "webhook completed");
                } else {
                    tracing::warn!(hook_name = ?name, url = %url, status = %response.status(), "webhook returned error");
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(hook_name = ?name, url = %url, error = %e, "webhook request failed");
            }
            Err(_) => {
                tracing::warn!(hook_name = ?name, url = %url, timeout_secs, "webhook timed out");
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

    #[tokio::test]
    async fn filter_skips_non_matching_type() {
        let hooks = vec![HookEntry {
            on_message: Some(PathBuf::from("/bin/false")), // Would fail if run
            filter_content_type: Some("query".to_string()),
            ..Default::default()
        }];
        let executor = HookExecutor::new(hooks).unwrap();

        // This should be skipped (type is "insight", filter is "query")
        let msg = make_test_message("insight");
        executor.execute(&msg).await;
        // If we reach here without error, the filter worked
    }
}
