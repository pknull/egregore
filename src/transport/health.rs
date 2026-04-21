//! `TransportHealth` — see RFC 0001 §5.2.
//!
//! Coarse liveness signal surfaced by each `Transport` implementation and
//! aggregated across children by composite transports. The `/v1/status`
//! endpoint surfaces this type in Phase 2; Phase 1 ships the type and the
//! aggregation helper so composite transports in Phase 2 do not re-touch
//! this file.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Coarse liveness signal for a transport.
///
/// Emitted by each `Transport::health()` impl. Composite transports aggregate
/// children via `TransportHealth::aggregate` per RFC 0001 §5.2.
///
/// Serde attributes preserve byte-for-byte output compatibility with the
/// single-transport case: empty `children`, `None` options, etc. are omitted.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransportHealth {
    /// True when the transport has at least one live path to peers/brokers.
    pub connected: bool,

    /// Adapter name: `"gossip"`, `"nats"`, `"composite"`, `"mock"`.
    pub backend: &'static str,

    /// Wall-clock of the most recent successful `publish` (local observation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_successful_publish: Option<DateTime<Utc>>,

    /// Wall-clock of the most recent inbound contact from any peer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_peer_contact: Option<DateTime<Utc>>,

    /// Messages accepted locally but not yet confirmed replicated to at least
    /// one peer. Summed across children in composite aggregation.
    pub unreplicated_count: u64,

    /// In-flight `publish` calls not yet resolved. Summed across children.
    pub inflight_publishes: usize,

    /// Most recent adapter-level error surfaced for operator visibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,

    /// Per-child health for composite transports. Omitted from serialized
    /// output when empty (the single-transport case) so `/v1/status` retains
    /// byte-for-byte compatibility with the pre-abstraction shape.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<TransportHealth>,
}

impl TransportHealth {
    /// Compose a top-level health from children per RFC 0001 §5.2.
    ///
    /// Aggregation rules:
    /// - `connected`: logical AND across children — any disconnected child
    ///   marks the composite disconnected.
    /// - `unreplicated_count`: saturating sum (never panics on `u64::MAX`).
    /// - `inflight_publishes`: saturating sum.
    /// - `last_successful_publish`, `last_peer_contact`: `max` (most recent).
    /// - `last_error`: most recent non-`None` across children. Tie-breaking:
    ///   when two children report errors at the same instant (or both lack a
    ///   timestamp for the error), the first child's error wins — this is
    ///   deterministic but arbitrary; see plan §11 OQ-3.
    /// - `children`: moved into the result for operator inspection.
    ///
    /// # Panics
    ///
    /// Debug builds `debug_assert!` that `children.len() >= 2`. `aggregate`
    /// is only valid for composite transports with at least two children;
    /// single-transport health is returned by each adapter directly. In
    /// release builds the empty/single case produces a degenerate result.
    pub fn aggregate(backend: &'static str, children: Vec<TransportHealth>) -> Self {
        debug_assert!(
            children.len() >= 2,
            "aggregate called on fewer than 2 children"
        );

        let connected = children.iter().all(|c| c.connected);

        let unreplicated_count = children
            .iter()
            .fold(0u64, |acc, c| acc.saturating_add(c.unreplicated_count));

        let inflight_publishes = children
            .iter()
            .fold(0usize, |acc, c| acc.saturating_add(c.inflight_publishes));

        let last_successful_publish = children
            .iter()
            .filter_map(|c| c.last_successful_publish)
            .max();

        let last_peer_contact = children.iter().filter_map(|c| c.last_peer_contact).max();

        // `last_error`: pick the error from the child with the latest
        // `last_peer_contact` (as a proxy for recency), breaking ties by first
        // occurrence. If no child exposes a timestamp alongside its error, we
        // fall back to the first child whose `last_error` is set.
        let last_error = children
            .iter()
            .filter(|c| c.last_error.is_some())
            .max_by_key(|c| c.last_peer_contact)
            .and_then(|c| c.last_error.clone());

        Self {
            connected,
            backend,
            last_successful_publish,
            last_peer_contact,
            unreplicated_count,
            inflight_publishes,
            last_error,
            children,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn base(connected: bool) -> TransportHealth {
        TransportHealth {
            connected,
            backend: "mock",
            last_successful_publish: None,
            last_peer_contact: None,
            unreplicated_count: 0,
            inflight_publishes: 0,
            last_error: None,
            children: vec![],
        }
    }

    #[test]
    fn aggregate_all_connected() {
        let children = vec![base(true), base(true), base(true)];
        let agg = TransportHealth::aggregate("composite", children);
        assert!(
            agg.connected,
            "all-connected children must aggregate to connected"
        );
        assert_eq!(agg.backend, "composite");
        assert_eq!(agg.children.len(), 3);
    }

    #[test]
    fn aggregate_any_disconnected() {
        let children = vec![base(true), base(false), base(true)];
        let agg = TransportHealth::aggregate("composite", children);
        assert!(
            !agg.connected,
            "any disconnected child forces composite disconnected"
        );
    }

    #[test]
    fn aggregate_timestamps_take_max() {
        let t1 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap();
        let t3 = Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap();

        let mut c1 = base(true);
        c1.last_peer_contact = Some(t1);
        c1.last_successful_publish = Some(t2);

        let mut c2 = base(true);
        c2.last_peer_contact = Some(t3);
        c2.last_successful_publish = Some(t1);

        let mut c3 = base(true);
        c3.last_peer_contact = Some(t2);
        c3.last_successful_publish = Some(t3);

        let agg = TransportHealth::aggregate("composite", vec![c1, c2, c3]);
        assert_eq!(agg.last_peer_contact, Some(t3), "max peer contact");
        assert_eq!(agg.last_successful_publish, Some(t3), "max publish time");
    }

    #[test]
    fn aggregate_unreplicated_sum() {
        let mut c1 = base(true);
        c1.unreplicated_count = 10;
        c1.inflight_publishes = 2;

        let mut c2 = base(true);
        c2.unreplicated_count = u64::MAX - 5; // near-overflow
        c2.inflight_publishes = 3;

        let mut c3 = base(true);
        c3.unreplicated_count = 100; // pushes over u64::MAX without saturation
        c3.inflight_publishes = 5;

        let agg = TransportHealth::aggregate("composite", vec![c1, c2, c3]);
        assert_eq!(
            agg.unreplicated_count,
            u64::MAX,
            "saturating_add must prevent wrap-around on u64::MAX-adjacent inputs"
        );
        assert_eq!(agg.inflight_publishes, 10, "inflight sums normally");
    }

    #[test]
    fn aggregate_last_error_surfaces() {
        let t1 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap();

        let mut c1 = base(true);
        c1.last_error = Some("older: connection reset".to_string());
        c1.last_peer_contact = Some(t1);

        let mut c2 = base(true); // no error
        c2.last_peer_contact = Some(t2);

        let mut c3 = base(false);
        c3.last_error = Some("newer: broker unreachable".to_string());
        c3.last_peer_contact = Some(t2);

        let agg = TransportHealth::aggregate("composite", vec![c1, c2, c3]);
        // Tie-breaking rule (documented on `aggregate`): child with latest
        // `last_peer_contact` wins; ties fall to first occurrence. c2 and c3
        // share t2; c2 has no error so c3's error is the only candidate at t2.
        assert_eq!(
            agg.last_error.as_deref(),
            Some("newer: broker unreachable"),
            "latest error (by last_peer_contact) surfaces"
        );
    }

    #[test]
    fn serde_empty_children_omitted() {
        // Load-bearing per plan §2.4 A-nit-2: empty children MUST NOT appear
        // in serialized output. This preserves /v1/status byte-compat for the
        // single-transport case.
        let h = base(true);
        let json = serde_json::to_string(&h).expect("serialize");
        assert!(
            !json.contains("children"),
            "empty children must be omitted; got: {json}"
        );
        assert!(
            !json.contains("last_successful_publish"),
            "None options must be omitted; got: {json}"
        );
        assert!(
            !json.contains("last_peer_contact"),
            "None options must be omitted; got: {json}"
        );
        assert!(
            !json.contains("last_error"),
            "None options must be omitted; got: {json}"
        );
    }

    #[test]
    fn serde_nonempty_children_included() {
        let children = vec![base(true), base(false)];
        let agg = TransportHealth::aggregate("composite", children);
        let json = serde_json::to_string(&agg).expect("serialize");
        assert!(json.contains("\"children\""));
    }
}
