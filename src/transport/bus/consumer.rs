//! JetStream stream + consumer bootstrap — Phase 2 Wave 1 Step 4.
//!
//! Provides two idempotent helpers:
//!
//! - `bootstrap_stream(ctx, config)` — create-or-reuse a single
//!   `egregore-feed` stream covering all `egregore.feed.>` subjects.
//! - `bootstrap_consumer(stream, config, name)` — create-or-reuse a
//!   durable pull consumer for this bridge node.
//!
//! Both paths use async-nats's `get_or_create_*` primitives so re-running
//! against an existing stream/consumer with the SAME config is a no-op.
//! The returned live configurations are then validated for the ordering
//! requirements that NATS permits but egregore forbids. Divergence is never
//! reconciled automatically; operators reset the stream/consumer deliberately
//! when semantics need to change (see `docs/deployment/disaster-recovery.md`,
//! Wave 4 Step 28).
//!
//! Stream parameters (amendment §C.7 binding):
//! - subjects: `["egregore.feed.>"]`
//! - retention: `Limits` (NATS retains until limits hit; egregore's
//!   own retention runner handles per-author cleanup in SQLite)
//! - storage: `File` (durable across restarts)
//! - max_message_size: **131_072 bytes** (128 KB) — amendment §C.7
//!   leaves headroom for the signed envelope's optional fields (tags,
//!   relates, trace/span ids, timestamps) above the 64 KB content cap.
//! - max_age: `0` (unlimited; rely on SQLite retention, not NATS's).
//! - discard: `Old` (older messages discarded when limits are reached).
//!
//! Consumer parameters:
//! - durable name: caller-supplied (derived from identity via
//!   `subjects::derive_consumer_name`).
//! - filter_subject: `egregore.feed.>` (all authors; composite transport
//!   applies per-subscriber filters on top).
//! - ack_policy: `Explicit` (bridge acks after all destinations resolve).
//! - ack_wait: `BusConfig::effective_ack_wait()` — amendment §C.11.
//! - max_ack_pending: `BusConfig::max_ack_pending` — validates to a
//!   bridge-capacity-sized window.
//!
//! Live broker behavior is covered by the ignored testcontainers smoke suite;
//! the startup-refusal predicates below have pure unit tests.

use async_nats::jetstream::consumer::{
    pull as pull_consumer, AckPolicy, Config as ConsumerConfig, Consumer, DeliverPolicy,
};
use async_nats::jetstream::context::ConsumerInfoErrorKind;
use async_nats::jetstream::stream::{
    Config as StreamConfig, DiscardPolicy, RetentionPolicy, StorageType, Stream,
};
use async_nats::jetstream::Context;

use crate::error::{EgreError, Result};

use super::config::BusConfig;
use super::subjects::WILDCARD_FILTER;

/// Convenience alias for a pull-mode consumer carrying our config type.
pub type PullConsumer = Consumer<pull_consumer::Config>;

/// NATS `max_msg_size` binding — amendment §C.7. Content cap is 64 KB but
/// the signed `Message` envelope adds up to ~4 KB of optional fields; 128
/// KB leaves headroom without requiring operators to reconfigure when new
/// fields land. Documented in `docs/deployment/bus.md` (Wave 4 Step 28).
pub const STREAM_MAX_MSG_SIZE: i32 = 131_072;

/// Refuse live stream configurations that can distribute one author's
/// messages across competing consumers or omit part of the feed namespace.
fn validate_stream_config(config: &StreamConfig) -> Result<()> {
    if config.retention == RetentionPolicy::WorkQueue {
        return Err(EgreError::Config {
            reason: "RFC 0001 §9.2: nats stream retention MUST NOT be WorkQueue (per-author ordering cannot be guaranteed)".to_string(),
        });
    }
    if !config
        .subjects
        .iter()
        .any(|subject| subject_covers_feed_prefix(subject))
    {
        return Err(EgreError::Config {
            reason: format!(
                "RFC 0001 §9.2: nats stream subjects must cover {WILDCARD_FILTER}; got {:?}",
                config.subjects
            ),
        });
    }
    Ok(())
}

/// Refuse queue-group consumers: a deliver group may split one author's
/// ordered feed across bridge instances.
fn validate_consumer_config(config: &ConsumerConfig) -> Result<()> {
    if config.deliver_group.is_some() {
        return Err(EgreError::Config {
            reason: "RFC 0001 §9.2: nats consumer deliver_group MUST be unset (per-author ordering cannot be guaranteed)".to_string(),
        });
    }
    Ok(())
}

/// Return true when a stream subject includes every subject matched by
/// `egregore.feed.>`. Broader terminal-wildcard subjects such as
/// `egregore.>` and `>` are valid coverage; narrower subjects are not.
fn subject_covers_feed_prefix(subject: &str) -> bool {
    const REQUIRED_PREFIX: [&str; 2] = ["egregore", "feed"];

    let tokens: Vec<&str> = subject.split('.').collect();
    let Some(wildcard_index) = tokens.iter().position(|token| *token == ">") else {
        return false;
    };

    wildcard_index <= REQUIRED_PREFIX.len()
        && wildcard_index + 1 == tokens.len()
        && tokens[..wildcard_index]
            .iter()
            .zip(REQUIRED_PREFIX)
            .all(|(actual, required)| *actual == required || *actual == "*")
}

/// Idempotently create or reuse the `egregore-feed` stream.
///
/// Returns the live `Stream` handle. On mismatched config (e.g., operator
/// changed the retention policy out-of-band), the NATS server returns an
/// error which we surface as `EgreError::Peer`.
pub async fn bootstrap_stream(ctx: &Context, config: &BusConfig) -> Result<Stream> {
    let stream_cfg = StreamConfig {
        name: config.stream_name.clone(),
        subjects: vec![WILDCARD_FILTER.to_string()],
        retention: RetentionPolicy::Limits,
        storage: StorageType::File,
        max_message_size: STREAM_MAX_MSG_SIZE,
        // 0 == unlimited. SQLite retention runner handles per-author
        // cleanup; NATS-side age is left to operator discretion via
        // stream-info edits (not the bootstrap path).
        max_age: std::time::Duration::ZERO,
        discard: DiscardPolicy::Old,
        ..Default::default()
    };

    let stream = ctx
        .get_or_create_stream(stream_cfg)
        .await
        .map_err(|e| EgreError::Peer {
            reason: format!("nats bootstrap_stream failed: {e}"),
        })?;
    validate_stream_config(&stream.cached_info().config)?;
    Ok(stream)
}

/// Idempotently create or reuse a durable pull consumer on the given
/// stream.
///
/// `consumer_name` is passed as a parameter (rather than derived inside
/// this function) so callers can either use `config.consumer_name` when
/// the operator has pinned one, or fall back to
/// `subjects::derive_consumer_name(identity)` when it is unset. Keeps
/// identity coupling at the caller level — the bootstrap helpers
/// themselves do not need an `Identity`.
pub async fn bootstrap_consumer(
    stream: &Stream,
    config: &BusConfig,
    consumer_name: &str,
) -> Result<PullConsumer> {
    let ack_wait = config.effective_ack_wait();

    // async-nats uses i64 for max_ack_pending; our config enforces the
    // u32 range [1, 65_535] so this cast is lossless.
    let max_ack_pending = i64::from(config.max_ack_pending);

    let consumer_cfg = pull_consumer::Config {
        durable_name: Some(consumer_name.to_string()),
        name: Some(consumer_name.to_string()),
        filter_subject: WILDCARD_FILTER.to_string(),
        ack_policy: AckPolicy::Explicit,
        ack_wait,
        max_ack_pending,
        deliver_policy: DeliverPolicy::All,
        ..Default::default()
    };

    // Inspect generic metadata before asking async-nats for a typed pull
    // consumer. A queue-group consumer is push-based, so typed conversion
    // would reject it before `validate_consumer_config` could produce the
    // RFC-specific startup refusal required by RFC 0001 §9.2.
    match stream.consumer_info(consumer_name).await {
        Ok(info) => validate_consumer_config(&info.config)?,
        Err(error) if error.kind() == ConsumerInfoErrorKind::NotFound => {}
        Err(error) => {
            return Err(EgreError::Peer {
                reason: format!("RFC 0001 §9.2: nats consumer ordering preflight failed: {error}"),
            });
        }
    }

    let consumer = stream
        .get_or_create_consumer(consumer_name, consumer_cfg)
        .await
        .map_err(|e| EgreError::Peer {
            reason: format!("RFC 0001 §9.2: nats bootstrap_consumer failed: {e}"),
        })?;
    validate_consumer_config(&consumer.cached_info().config)?;
    Ok(consumer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_nats::jetstream::consumer::Config as ConsumerConfig;

    #[test]
    fn stream_validation_rejects_work_queue_retention() {
        let config = StreamConfig {
            subjects: vec![WILDCARD_FILTER.to_string()],
            retention: RetentionPolicy::WorkQueue,
            ..Default::default()
        };

        let error = validate_stream_config(&config).unwrap_err();
        assert!(error.to_string().contains("RFC 0001 §9.2"));
    }

    #[test]
    fn stream_validation_rejects_missing_feed_wildcard_coverage() {
        let config = StreamConfig {
            subjects: vec!["some.other.subject.>".to_string()],
            retention: RetentionPolicy::Limits,
            ..Default::default()
        };

        let error = validate_stream_config(&config).unwrap_err();
        assert!(error.to_string().contains("RFC 0001 §9.2"));
    }

    #[test]
    fn stream_validation_accepts_limits_with_feed_wildcard() {
        let config = StreamConfig {
            subjects: vec![WILDCARD_FILTER.to_string()],
            retention: RetentionPolicy::Limits,
            ..Default::default()
        };

        assert!(validate_stream_config(&config).is_ok());
    }

    #[test]
    fn stream_validation_accepts_broader_terminal_wildcard() {
        let config = StreamConfig {
            subjects: vec!["egregore.>".to_string()],
            retention: RetentionPolicy::Limits,
            ..Default::default()
        };

        assert!(validate_stream_config(&config).is_ok());
    }

    #[test]
    fn stream_validation_rejects_narrower_subject_pattern() {
        let config = StreamConfig {
            subjects: vec!["egregore.feed.*".to_string()],
            retention: RetentionPolicy::Limits,
            ..Default::default()
        };

        let error = validate_stream_config(&config).unwrap_err();
        assert!(error.to_string().contains("RFC 0001 §9.2"));
    }

    #[test]
    fn consumer_validation_rejects_deliver_group() {
        let config = ConsumerConfig {
            deliver_group: Some("competing-workers".to_string()),
            filter_subject: WILDCARD_FILTER.to_string(),
            ..Default::default()
        };

        let error = validate_consumer_config(&config).unwrap_err();
        assert!(error.to_string().contains("RFC 0001 §9.2"));
    }

    #[test]
    fn consumer_validation_accepts_ungrouped_feed_wildcard() {
        let config = ConsumerConfig {
            deliver_group: None,
            filter_subject: WILDCARD_FILTER.to_string(),
            ..Default::default()
        };

        assert!(validate_consumer_config(&config).is_ok());
    }
}
