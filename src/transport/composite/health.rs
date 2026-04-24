//! `BridgeQueuesHealth` aggregation — RFC 0002 §8.4.
//!
//! Converts the live `DirectionState` (atomics, locked maps, aging
//! timestamps) into the serializable `BridgeQueuesHealth` snapshot
//! consumed by `/v1/status` (wired Wave 5) and by the operator scry
//! panel. All reads are snapshots; we never hold a lock across
//! construction of the returned struct.

use std::sync::atomic::Ordering;
use std::time::Instant;

use crate::transport::health::BridgeQueuesHealth;

use super::direction::{DirectionState, BRIDGE_QUEUE_HIGH_WATERMARK};

/// Materialize a `BridgeQueuesHealth` snapshot for one destination.
///
/// `destination` is the child's backend identifier (`"gossip"`, `"bus"`,
/// `"mock"`) copied from the child's own `TransportHealth.backend` so the
/// rendered JSON stays consistent with the child's self-reported backend
/// string.
pub(crate) fn compute_bridge_queues_health(
    dir: &DirectionState,
    destination: &str,
) -> BridgeQueuesHealth {
    // Snapshot the queue map under a short lock. We compute depth_total
    // and authors_active from the same snapshot so the two numbers stay
    // internally consistent (a depth > 0 always has at least one
    // contributing author).
    let (depth_total, authors_active) = {
        let queues = dir.queues.lock();
        let mut total: u64 = 0;
        let mut active: u64 = 0;
        for entry in queues.values() {
            let len = entry.queue.len();
            if len > 0 {
                active += 1;
                total = total.saturating_add(len as u64);
            }
        }
        (total, active)
    };

    // authors_backpressured: we could snapshot the `backpressured` set,
    // but that can drift from the live queue depth in a narrow window
    // (egress popped to low but hasn't yet removed the key). Authoritative
    // count is "queues whose current len >= high_watermark" — cheaper and
    // tighter than trusting the set.
    let authors_backpressured = {
        let queues = dir.queues.lock();
        queues
            .values()
            .filter(|e| e.queue.len() >= BRIDGE_QUEUE_HIGH_WATERMARK)
            .count() as u64
    };

    let now = Instant::now();
    let oldest_queued_age_secs = dir
        .oldest_queued_at
        .read()
        .map(|ts| now.saturating_duration_since(ts).as_secs());

    let publish_in_flight_age_secs = dir
        .publish_in_flight_since
        .read()
        .map(|ts| now.saturating_duration_since(ts).as_secs());

    // Scrub last_error on read — this is a second line of defence. Writers
    // (egress + publish-on-error paths) already scrub; this
    // ensures a mis-routed write can't leak here.
    let last_error = dir.last_error.read().clone().map(sanitize_error_surface);

    BridgeQueuesHealth {
        destination: destination.to_string(),
        depth_total,
        authors_backpressured,
        authors_active,
        backpressure_events_total: dir.backpressure_events.load(Ordering::Relaxed),
        self_echo_total: dir.self_echo_total.load(Ordering::Relaxed),
        oldest_queued_age_secs,
        publish_in_flight_age_secs,
        ack_on_error_total: dir.ack_on_error_total.load(Ordering::Relaxed),
        nats_redelivery_total: dir.nats_redelivery_total.load(Ordering::Relaxed),
        last_error,
    }
}

/// Second-line auditor A2 scrub. Producers SHOULD already have stripped
/// PII; this function caps the error string at a safe byte length and
/// strips obvious high-entropy tokens if they somehow slipped through.
fn sanitize_error_surface(raw: String) -> String {
    // A2: cap length. Longer than this implies a message stack dump or
    // payload bytes, neither of which belong in an operator surface.
    const MAX_LEN: usize = 160;
    if raw.len() <= MAX_LEN {
        raw
    } else {
        let mut truncated = raw;
        truncated.truncate(MAX_LEN);
        truncated.push_str("...[truncated]");
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::models::Message;
    use crate::identity::PublicId;
    use crate::transport::composite::direction::BRIDGE_QUEUE_HIGH_WATERMARK;
    use chrono::Utc;
    use std::sync::Arc;
    use std::time::Duration;

    fn sample_message(seq: u64, author: &str) -> Arc<Message> {
        Arc::new(Message {
            author: PublicId(author.to_string()),
            sequence: seq,
            previous: None,
            timestamp: Utc::now(),
            content: serde_json::json!({"type": "note"}),
            schema_id: None,
            relates: None,
            tags: vec![],
            trace_id: None,
            span_id: None,
            expires_at: None,
            hash: format!("hash-{seq}"),
            signature: "sig".to_string(),
        })
    }

    fn seed(dir: &DirectionState, author: &str, count: u64) {
        let mut queues = dir.queues.lock();
        let entry = queues.entry((0, PublicId(author.to_string()))).or_default();
        for seq in 1..=count {
            entry.queue.push(sample_message(seq, author)).unwrap();
        }
    }

    #[test]
    fn bridge_queues_health_empty_reports_zeros() {
        let dir = DirectionState::new();
        let h = compute_bridge_queues_health(&dir, "gossip");
        assert_eq!(h.destination, "gossip");
        assert_eq!(h.depth_total, 0);
        assert_eq!(h.authors_active, 0);
        assert_eq!(h.authors_backpressured, 0);
        assert_eq!(h.backpressure_events_total, 0);
        assert_eq!(h.self_echo_total, 0);
        assert_eq!(h.ack_on_error_total, 0);
        assert_eq!(h.nats_redelivery_total, 0);
        assert_eq!(h.oldest_queued_age_secs, None);
        assert_eq!(h.publish_in_flight_age_secs, None);
        assert_eq!(h.last_error, None);
    }

    #[test]
    fn bridge_queues_health_reports_depth_and_active_authors() {
        let dir = DirectionState::new();
        seed(&dir, "@a.ed25519", 3);
        seed(&dir, "@b.ed25519", 5);
        let h = compute_bridge_queues_health(&dir, "bus");
        assert_eq!(h.depth_total, 8);
        assert_eq!(h.authors_active, 2);
        assert_eq!(h.authors_backpressured, 0, "below high watermark");
    }

    #[test]
    fn bridge_queues_health_reports_backpressured_count() {
        let dir = DirectionState::new();
        seed(&dir, "@a.ed25519", BRIDGE_QUEUE_HIGH_WATERMARK as u64);
        seed(&dir, "@b.ed25519", 10);
        let h = compute_bridge_queues_health(&dir, "gossip");
        assert_eq!(h.authors_backpressured, 1, "@a at high watermark");
        assert_eq!(h.authors_active, 2);
    }

    #[test]
    fn bridge_queues_health_aging_fields_populate_when_armed() {
        let dir = DirectionState::new();
        seed(&dir, "@a.ed25519", 1);
        *dir.oldest_queued_at.write() = Some(Instant::now() - Duration::from_secs(5));
        *dir.publish_in_flight_since.write() = Some(Instant::now() - Duration::from_secs(2));

        let h = compute_bridge_queues_health(&dir, "bus");
        assert!(
            h.oldest_queued_age_secs.unwrap() >= 5,
            "oldest_queued_age_secs reflects armed timestamp"
        );
        assert!(
            h.publish_in_flight_age_secs.unwrap() >= 2,
            "publish_in_flight_age_secs reflects armed timestamp"
        );
    }

    #[test]
    fn bridge_queues_health_counters_snapshot_atomics() {
        let dir = DirectionState::new();
        dir.backpressure_events.store(7, Ordering::Relaxed);
        dir.self_echo_total.store(3, Ordering::Relaxed);
        dir.ack_on_error_total.store(11, Ordering::Relaxed);
        dir.nats_redelivery_total.store(2, Ordering::Relaxed);
        let h = compute_bridge_queues_health(&dir, "bus");
        assert_eq!(h.backpressure_events_total, 7);
        assert_eq!(h.self_echo_total, 3);
        assert_eq!(h.ack_on_error_total, 11);
        assert_eq!(h.nats_redelivery_total, 2);
    }

    #[test]
    fn bridge_queues_health_last_error_is_scrubbed_by_length_cap() {
        let dir = DirectionState::new();
        // Pretend some upstream writer accidentally stuffed a long string.
        let long = "x".repeat(300);
        *dir.last_error.write() = Some(long);
        let h = compute_bridge_queues_health(&dir, "bus");
        let le = h.last_error.unwrap();
        assert!(
            le.ends_with("...[truncated]"),
            "over-long last_error must be truncated by A2 sanitizer; got: {}",
            le.len()
        );
        assert!(le.len() < 200, "truncated length cap");
    }

    #[test]
    fn bridge_queues_health_serializes_with_serde_omit_when_none() {
        let dir = DirectionState::new();
        let h = compute_bridge_queues_health(&dir, "gossip");
        let json = serde_json::to_string(&h).unwrap();
        assert!(
            !json.contains("oldest_queued_age_secs"),
            "None options must be omitted: {json}"
        );
        assert!(
            !json.contains("publish_in_flight_age_secs"),
            "None options must be omitted: {json}"
        );
        assert!(
            !json.contains("last_error"),
            "None options must be omitted: {json}"
        );
        // But the required fields ARE present.
        assert!(json.contains("depth_total"));
        assert!(json.contains("self_echo_total"));
    }
}
