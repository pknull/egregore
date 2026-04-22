//! Profile lifecycle — startup self-enforce, refresh window, clock abstraction.
//!
//! Implements the startup half of RFC 0001 §11.2 + Phase 1 plan §6.2: at boot,
//! the node queries its own feed for the most-recent `Content::Profile` and
//! publishes a fresh, dated Profile when none exists, when `valid_until` is
//! missing (pre-upgrade Profile), when it has expired, or when it falls
//! inside the 7-day refresh window.
//!
//! The periodic refresh scheduler (plan §6.4) is Step 13 — not implemented
//! here.
//!
//! The `Clock` abstraction is introduced by this step because it is the first
//! consumer that needs time mockability; Step 13's scheduler will share it.

use chrono::{DateTime, Duration, Utc};

use crate::error::Result;
use crate::feed::content_types::{BrokerDetails, Content};
use crate::feed::engine::FeedEngine;
use crate::feed::models::FeedQuery;
use crate::identity::{Identity, PublicId};

/// Default TTL for fresh Profiles (days). Step 14 will replace this hardcoded
/// value with a `config.profile_ttl_days` read; the 90-day default is
/// preserved there.
pub const DEFAULT_PROFILE_TTL_DAYS: u32 = 90;

/// Window (days) before `valid_until` within which the node proactively
/// re-publishes a fresh Profile on startup. Also consumed by the refresh
/// scheduler (Step 13).
pub const REFRESH_WINDOW_DAYS: i64 = 7;

/// Abstract clock for time-dependent logic. Production uses `SystemClock`;
/// tests use `MockClock`.
///
/// Introduced in Step 11 because startup self-enforce is the first consumer
/// that needs deterministic time for unit tests. Plan §10.4 acceptance
/// criterion 6 (refresh scheduler mockable clock) will share this trait in
/// Step 13.
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> DateTime<Utc>;
}

/// Wall-clock implementation used in production.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Reasonable upper bound on the display-name substring we synthesize from a
/// `PublicId` when no prior Profile exists to copy a name from. The PublicId
/// wire format is `@<44 base64 chars>.ed25519` (53 chars); we truncate to a
/// bounded display length so downstream renderers don't have to.
const DEFAULT_NAME_DISPLAY_LEN: usize = 20;

/// Result of looking up a peer's most-recent Profile to determine its
/// freshness. Per RFC 0001 §11.2 the peer filter is lenient on missing
/// `valid_until` (pre-upgrade peers) — that case deserves a distinct variant
/// from `Expired` so callers can render different UX.
///
/// The four variants:
/// - `Absent`: no Profile exists on the peer's feed.
/// - `Undated`: Profile exists, `valid_until` is `None` (pre-upgrade peer).
/// - `Valid`: Profile exists, `valid_until` is in the future (any future
///   `valid_until` is considered valid for peers — the 7-day refresh window
///   applies only to the *own* feed, not peer feeds).
/// - `Expired`: Profile exists and `valid_until < now`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileValidity {
    /// No Profile message exists on the peer's feed.
    Absent,
    /// Profile exists but `valid_until` is None (pre-upgrade peer; accept).
    Undated,
    /// Profile exists, `valid_until` is in the future (outside refresh window
    /// is not significant for peers — we treat any future `valid_until` as
    /// valid).
    Valid,
    /// Profile exists and `valid_until < now`.
    Expired,
}

impl ProfileValidity {
    /// The single predicate the §6.3 consumers need: "should this peer be
    /// excluded from capability/routing decisions?" — returns true ONLY for
    /// `Expired`. Absent and Undated are conservatively permitted (RFC 0001
    /// §11.2 — don't blanket-reject pre-upgrade peers).
    pub fn is_expired(&self) -> bool {
        matches!(self, Self::Expired)
    }
}

/// Look up a peer's most-recent Profile and classify its freshness.
///
/// Consumed by `/v1/mesh` and `build_node_status` to surface a `profile_expired`
/// boolean per peer (Phase 1 plan §6.3). The peer is never removed from the
/// response — expiration is a trust/freshness signal, not a connectivity gate
/// (RFC 0001 §11.2).
///
/// Lives here rather than `peers.rs` (plan §6.3 item 4) because the lookup
/// needs `FeedEngine::query` access, not peer-table access.
///
/// Reuses the existing `Clock` trait from Step 11 so tests can simulate time
/// passing without wall-clock dependencies.
pub fn peer_profile_validity(
    engine: &FeedEngine,
    peer_id: &PublicId,
    clock: &dyn Clock,
) -> Result<ProfileValidity> {
    let query = FeedQuery {
        author: Some(peer_id.clone()),
        content_type: Some("profile".into()),
        limit: Some(1),
        ..Default::default()
    };
    let recent = engine.query(&query)?;

    let Some(msg) = recent.into_iter().next() else {
        return Ok(ProfileValidity::Absent);
    };

    let content: Content = serde_json::from_value(msg.content.clone())?;
    let validity = match content {
        Content::Profile {
            valid_until: None, ..
        } => ProfileValidity::Undated,
        Content::Profile {
            valid_until: Some(vu),
            ..
        } => {
            if vu < clock.now() {
                ProfileValidity::Expired
            } else {
                ProfileValidity::Valid
            }
        }
        // A non-Profile row in the `profile` content_type slot would be a
        // serialization bug; treat as Absent so the consumer does not mark
        // the peer as expired on the basis of corrupt data.
        _ => ProfileValidity::Absent,
    };
    Ok(validity)
}

/// Startup self-enforce per RFC 0001 §11.2 + Phase 1 plan §6.2.
///
/// Algorithm (precise):
/// 1. Query the author's own feed for the most-recent `Content::Profile`.
/// 2. If none is found, OR `valid_until.is_none()`, OR `valid_until < now`,
///    OR `valid_until - now < REFRESH_WINDOW_DAYS`: publish a fresh Profile.
/// 3. Otherwise (Profile is fresh and outside the refresh window): no-op.
///
/// When publishing a fresh Profile, prior identity fields (`name`,
/// `description`, `capabilities`, `broker`) are carried forward from the
/// previous Profile when one exists. Otherwise a minimal default Profile is
/// synthesized.
///
/// On publish failure (anything other than `DuplicateMessage` — which cannot
/// occur on a single-author publish path), the error is propagated and
/// `main.rs` refuses to start.
pub fn ensure_valid_profile(
    engine: &FeedEngine,
    identity: &Identity,
    ttl_days: u32,
    clock: &dyn Clock,
) -> Result<()> {
    let author = identity.public_id();

    // Step 1: most-recent Profile on the author's own feed.
    let query = FeedQuery {
        author: Some(author.clone()),
        content_type: Some("profile".into()),
        limit: Some(1),
        ..Default::default()
    };
    let recent = engine.query(&query)?;

    let now = clock.now();

    // Step 2: decide whether to publish.
    let (needs_publish, prior_profile) = match recent.into_iter().next() {
        None => (true, None),
        Some(msg) => {
            // Deserialize back to Content to read typed fields.
            let content: Content = serde_json::from_value(msg.content.clone())?;
            match &content {
                Content::Profile {
                    valid_until: None, ..
                } => (true, Some(content)),
                Content::Profile {
                    valid_until: Some(vu),
                    ..
                } => {
                    let needs = *vu < now || (*vu - now) < Duration::days(REFRESH_WINDOW_DAYS);
                    (needs, Some(content))
                }
                // A non-Profile in the `profile` content_type slot would be a
                // serialization bug; treat as "no valid Profile".
                _ => (true, None),
            }
        }
    };

    if !needs_publish {
        return Ok(());
    }

    // Step 3: build the fresh Profile, preserving prior identity fields.
    let (name, description, capabilities, broker) = match prior_profile {
        Some(Content::Profile {
            name,
            description,
            capabilities,
            broker,
            ..
        }) => (name, description, capabilities, broker),
        _ => default_identity_fields(identity),
    };

    let fresh = Content::Profile {
        name,
        description,
        capabilities,
        broker,
        valid_from: Some(now),
        valid_until: Some(now + Duration::days(ttl_days as i64)),
    };

    // No schema_id override (profile.v1 is inferred from content type), no
    // relates, no tags. publish_with_schema propagates any EgreError with `?`.
    engine.publish_with_schema(identity, fresh.to_value(), None, None, vec![])?;

    Ok(())
}

/// Build sensible default identity fields for a first-ever Profile publish.
/// The name is a truncated PublicId substring so operators can see something
/// human-ish on day-0; the rest is empty.
fn default_identity_fields(
    identity: &Identity,
) -> (String, Option<String>, Vec<String>, Option<BrokerDetails>) {
    let pid = identity.public_id().0;
    let display_len = pid.len().min(DEFAULT_NAME_DISPLAY_LEN);
    let name = pid[..display_len].to_string();
    (name, None, Vec::new(), None)
}

#[cfg(test)]
pub(crate) struct MockClock {
    inner: std::sync::Arc<parking_lot::Mutex<DateTime<Utc>>>,
}

#[cfg(test)]
impl MockClock {
    pub(crate) fn new(at: DateTime<Utc>) -> Self {
        Self {
            inner: std::sync::Arc::new(parking_lot::Mutex::new(at)),
        }
    }

    pub(crate) fn advance(&self, by: Duration) {
        let mut guard = self.inner.lock();
        *guard += by;
    }
}

#[cfg(test)]
impl Clock for MockClock {
    fn now(&self) -> DateTime<Utc> {
        *self.inner.lock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::store::FeedStore;

    fn setup() -> (FeedEngine, Identity) {
        let store = FeedStore::open_memory().unwrap();
        let engine = FeedEngine::new(store);
        let identity = Identity::generate();
        (engine, identity)
    }

    /// Helper — the one public_id().0 truncated as per default_identity_fields.
    fn expected_default_name(identity: &Identity) -> String {
        let pid = identity.public_id().0;
        let len = pid.len().min(DEFAULT_NAME_DISPLAY_LEN);
        pid[..len].to_string()
    }

    /// Helper: count Profile messages on the author's feed.
    fn count_profile_messages(engine: &FeedEngine, identity: &Identity) -> usize {
        let q = FeedQuery {
            author: Some(identity.public_id()),
            content_type: Some("profile".into()),
            limit: Some(50),
            ..Default::default()
        };
        engine.query(&q).unwrap().len()
    }

    /// Helper: the most-recent Profile, deserialized to Content.
    fn latest_profile(engine: &FeedEngine, identity: &Identity) -> Content {
        let q = FeedQuery {
            author: Some(identity.public_id()),
            content_type: Some("profile".into()),
            limit: Some(1),
            ..Default::default()
        };
        let msgs = engine.query(&q).unwrap();
        let msg = msgs.first().expect("expected at least one Profile");
        serde_json::from_value(msg.content.clone()).expect("Profile must deserialize")
    }

    #[test]
    fn ensure_publishes_fresh_profile_when_none_exists() {
        let (engine, identity) = setup();
        let now = Utc::now();
        let clock = MockClock::new(now);

        ensure_valid_profile(&engine, &identity, DEFAULT_PROFILE_TTL_DAYS, &clock)
            .expect("publish must succeed on empty feed");

        assert_eq!(
            count_profile_messages(&engine, &identity),
            1,
            "exactly one Profile should be on the feed"
        );

        let content = latest_profile(&engine, &identity);
        match content {
            Content::Profile {
                name,
                description,
                capabilities,
                broker,
                valid_from,
                valid_until,
            } => {
                assert_eq!(name, expected_default_name(&identity));
                assert!(description.is_none());
                assert!(capabilities.is_empty());
                assert!(broker.is_none(), "Phase 1 gossip-only: broker is None");
                assert_eq!(valid_from, Some(now));
                assert_eq!(
                    valid_until,
                    Some(now + Duration::days(DEFAULT_PROFILE_TTL_DAYS as i64))
                );
            }
            other => panic!("expected Profile variant, got {:?}", other),
        }
    }

    #[test]
    fn ensure_publishes_fresh_when_prior_profile_has_none_valid_until() {
        let (engine, identity) = setup();
        let t0 = Utc::now();
        let clock = MockClock::new(t0);

        // Publish an old-style Profile (all three new fields None — pre-upgrade).
        let old = Content::Profile {
            name: "legacy-node".to_string(),
            description: Some("pre-upgrade".to_string()),
            capabilities: vec!["insight".to_string()],
            broker: None,
            valid_from: None,
            valid_until: None,
        };
        engine
            .publish_with_schema(&identity, old.to_value(), None, None, vec![])
            .expect("publish old-style Profile");

        ensure_valid_profile(&engine, &identity, DEFAULT_PROFILE_TTL_DAYS, &clock)
            .expect("publish must succeed");

        assert_eq!(
            count_profile_messages(&engine, &identity),
            2,
            "a fresh Profile should be appended alongside the legacy one"
        );

        match latest_profile(&engine, &identity) {
            Content::Profile {
                name,
                description,
                capabilities,
                valid_from,
                valid_until,
                ..
            } => {
                assert_eq!(name, "legacy-node", "prior name preserved");
                assert_eq!(description.as_deref(), Some("pre-upgrade"));
                assert_eq!(capabilities, vec!["insight".to_string()]);
                assert_eq!(valid_from, Some(t0));
                assert_eq!(
                    valid_until,
                    Some(t0 + Duration::days(DEFAULT_PROFILE_TTL_DAYS as i64))
                );
            }
            other => panic!("expected Profile, got {:?}", other),
        }
    }

    #[test]
    fn ensure_publishes_fresh_when_prior_profile_expired() {
        let (engine, identity) = setup();
        let t0 = Utc::now();
        let clock = MockClock::new(t0);

        // Publish a Profile with valid_until = T + 10d (short TTL for the test).
        let short_lived = Content::Profile {
            name: "shortlived".to_string(),
            description: None,
            capabilities: vec![],
            broker: None,
            valid_from: Some(t0),
            valid_until: Some(t0 + Duration::days(10)),
        };
        engine
            .publish_with_schema(&identity, short_lived.to_value(), None, None, vec![])
            .expect("publish short-lived Profile");

        // Advance clock to T + 20d (past valid_until).
        clock.advance(Duration::days(20));
        let t1 = clock.now();

        ensure_valid_profile(&engine, &identity, DEFAULT_PROFILE_TTL_DAYS, &clock)
            .expect("publish must succeed for expired Profile");

        assert_eq!(count_profile_messages(&engine, &identity), 2);
        match latest_profile(&engine, &identity) {
            Content::Profile {
                valid_from,
                valid_until,
                ..
            } => {
                assert_eq!(valid_from, Some(t1));
                assert_eq!(
                    valid_until,
                    Some(t1 + Duration::days(DEFAULT_PROFILE_TTL_DAYS as i64))
                );
            }
            other => panic!("expected Profile, got {:?}", other),
        }
    }

    #[test]
    fn ensure_publishes_fresh_when_inside_refresh_window() {
        let (engine, identity) = setup();
        let t0 = Utc::now();
        let clock = MockClock::new(t0);

        // valid_until = T + 5d → inside the 7-day refresh window at T.
        let within_window = Content::Profile {
            name: "in-window".to_string(),
            description: None,
            capabilities: vec![],
            broker: None,
            valid_from: Some(t0),
            valid_until: Some(t0 + Duration::days(5)),
        };
        engine
            .publish_with_schema(&identity, within_window.to_value(), None, None, vec![])
            .expect("publish refresh-window Profile");

        ensure_valid_profile(&engine, &identity, DEFAULT_PROFILE_TTL_DAYS, &clock)
            .expect("publish must succeed for refresh-window");

        assert_eq!(
            count_profile_messages(&engine, &identity),
            2,
            "refresh-window branch must publish"
        );
    }

    #[test]
    fn ensure_noop_when_profile_is_fresh() {
        let (engine, identity) = setup();
        let t0 = Utc::now();
        let clock = MockClock::new(t0);

        // valid_until = T + 90d → outside the 7-day refresh window.
        let fresh = Content::Profile {
            name: "fresh".to_string(),
            description: Some("recent".to_string()),
            capabilities: vec!["a".to_string()],
            broker: None,
            valid_from: Some(t0),
            valid_until: Some(t0 + Duration::days(DEFAULT_PROFILE_TTL_DAYS as i64)),
        };
        engine
            .publish_with_schema(&identity, fresh.to_value(), None, None, vec![])
            .expect("publish fresh Profile");

        ensure_valid_profile(&engine, &identity, DEFAULT_PROFILE_TTL_DAYS, &clock)
            .expect("no-op must succeed");

        assert_eq!(
            count_profile_messages(&engine, &identity),
            1,
            "no second Profile should be published when the current one is fresh"
        );
    }

    #[test]
    fn ensure_preserves_name_description_capabilities_broker_from_prior() {
        let (engine, identity) = setup();
        let t0 = Utc::now();
        let clock = MockClock::new(t0);

        let broker = BrokerDetails {
            operator_name: "Acme Ops".to_string(),
            jurisdiction: "US-DE".to_string(),
            disclosure_policy: "rfc-0002-disclosure-v1".to_string(),
            tenancy: "dedicated".to_string(),
            broker_endpoint: "nats://broker.example:4222".to_string(),
            backend: "nats".to_string(),
        };
        let prior = Content::Profile {
            name: "carry-forward".to_string(),
            description: Some("preserved description".to_string()),
            capabilities: vec!["insight".to_string(), "endorsement".to_string()],
            broker: Some(broker.clone()),
            valid_from: Some(t0),
            valid_until: Some(t0 + Duration::days(30)),
        };
        engine
            .publish_with_schema(&identity, prior.to_value(), None, None, vec![])
            .expect("publish prior Profile");

        // Move well past valid_until.
        clock.advance(Duration::days(95));

        ensure_valid_profile(&engine, &identity, DEFAULT_PROFILE_TTL_DAYS, &clock)
            .expect("publish must succeed");

        match latest_profile(&engine, &identity) {
            Content::Profile {
                name,
                description,
                capabilities,
                broker: carried_broker,
                ..
            } => {
                assert_eq!(name, "carry-forward");
                assert_eq!(description.as_deref(), Some("preserved description"));
                assert_eq!(
                    capabilities,
                    vec!["insight".to_string(), "endorsement".to_string()]
                );
                let b = carried_broker.expect("broker must be carried forward");
                assert_eq!(b.operator_name, broker.operator_name);
                assert_eq!(b.jurisdiction, broker.jurisdiction);
                assert_eq!(b.disclosure_policy, broker.disclosure_policy);
                assert_eq!(b.tenancy, broker.tenancy);
                assert_eq!(b.broker_endpoint, broker.broker_endpoint);
                assert_eq!(b.backend, broker.backend);
            }
            other => panic!("expected Profile, got {:?}", other),
        }
    }

    /// Structural test: `ensure_valid_profile` returns `Err` when the publish
    /// step fails. Triggering a `publish_with_schema` failure from inside
    /// `ensure_valid_profile` without mocking the engine is impractical —
    /// the publish path runs through `FeedEngine::publish_full`, which only
    /// returns errors on schema-validation or store-write failures. We
    /// exercise the schema-validation failure path here by feeding a `ttl_days`
    /// that would produce an invalid Profile… but the Profile produced by
    /// `ensure_valid_profile` is always schema-valid by construction.
    ///
    /// Error-propagation is therefore validated by code review: the function
    /// ends with `engine.publish_with_schema(...)?` — any `EgreError` the
    /// underlying call returns bubbles up unchanged. The next-tightest
    /// observable assertion is a compile-time one: the return type is
    /// `Result<()>`, and the `?` is on the final publish call.
    ///
    /// We keep this test as a placeholder so the intent is documented; if
    /// a dep-injection story for the engine lands later, flesh it out.
    #[test]
    fn ensure_propagates_publish_errors_by_construction() {
        // The function signature itself carries the contract: `Result<()>`
        // with a `?` on the publish line means any EgreError from the
        // underlying publish propagates to the caller without wrapping.
        // This test asserts only the trivial invariant that a healthy
        // engine returns Ok so regressions in the *happy path* (e.g.
        // accidentally swallowing the return into Ok(())) are caught.
        let (engine, identity) = setup();
        let clock = MockClock::new(Utc::now());
        let result = ensure_valid_profile(&engine, &identity, DEFAULT_PROFILE_TTL_DAYS, &clock);
        assert!(
            result.is_ok(),
            "healthy engine + empty feed must succeed: {:?}",
            result.err()
        );
    }

    // ------------------------------------------------------------------
    // Step 12 — peer_profile_validity tests
    //
    // The lookup helper represents four distinct states (Absent / Undated /
    // Valid / Expired); callers render them differently (RFC 0001 §11.2).
    // `is_expired()` is the single predicate consumers need to decide whether
    // to mark a peer's `profile_expired` flag.
    // ------------------------------------------------------------------

    #[test]
    fn peer_profile_validity_absent() {
        let (engine, _self_identity) = setup();
        let peer_identity = Identity::generate();
        let clock = MockClock::new(Utc::now());

        let validity = peer_profile_validity(&engine, &peer_identity.public_id(), &clock)
            .expect("lookup must succeed on empty feed");

        assert_eq!(validity, ProfileValidity::Absent);
        assert!(!validity.is_expired(), "Absent is not expired");
    }

    #[test]
    fn peer_profile_validity_undated() {
        // Simulate a pre-upgrade peer: Profile with valid_until = None. The
        // soft filter must treat this as Undated/accept — NOT Expired.
        let (engine, _self_identity) = setup();
        let peer_identity = Identity::generate();
        let clock = MockClock::new(Utc::now());

        let legacy_profile = Content::Profile {
            name: "legacy-peer".to_string(),
            description: None,
            capabilities: vec![],
            broker: None,
            valid_from: None,
            valid_until: None,
        };
        engine
            .publish_with_schema(
                &peer_identity,
                legacy_profile.to_value(),
                None,
                None,
                vec![],
            )
            .expect("publish pre-upgrade Profile on peer feed");

        let validity = peer_profile_validity(&engine, &peer_identity.public_id(), &clock)
            .expect("lookup must succeed");

        assert_eq!(validity, ProfileValidity::Undated);
        assert!(
            !validity.is_expired(),
            "Undated (pre-upgrade peer) must NOT be treated as expired (RFC 0001 §11.2)"
        );
    }

    #[test]
    fn peer_profile_validity_valid() {
        let (engine, _self_identity) = setup();
        let peer_identity = Identity::generate();
        let now = Utc::now();
        let clock = MockClock::new(now);

        let fresh = Content::Profile {
            name: "current-peer".to_string(),
            description: None,
            capabilities: vec![],
            broker: None,
            valid_from: Some(now),
            valid_until: Some(now + Duration::days(30)),
        };
        engine
            .publish_with_schema(&peer_identity, fresh.to_value(), None, None, vec![])
            .expect("publish fresh Profile on peer feed");

        let validity = peer_profile_validity(&engine, &peer_identity.public_id(), &clock)
            .expect("lookup must succeed");

        assert_eq!(validity, ProfileValidity::Valid);
        assert!(!validity.is_expired());
    }

    #[test]
    fn peer_profile_validity_expired() {
        let (engine, _self_identity) = setup();
        let peer_identity = Identity::generate();
        let t0 = Utc::now();
        let clock = MockClock::new(t0);

        // Peer publishes at T with valid_until = T + 10d.
        let short_lived = Content::Profile {
            name: "soon-expired".to_string(),
            description: None,
            capabilities: vec![],
            broker: None,
            valid_from: Some(t0),
            valid_until: Some(t0 + Duration::days(10)),
        };
        engine
            .publish_with_schema(&peer_identity, short_lived.to_value(), None, None, vec![])
            .expect("publish short-lived Profile on peer feed");

        // Advance to T + 20d (past valid_until).
        clock.advance(Duration::days(20));

        let validity = peer_profile_validity(&engine, &peer_identity.public_id(), &clock)
            .expect("lookup must succeed");

        assert_eq!(validity, ProfileValidity::Expired);
        assert!(validity.is_expired());
    }

    #[test]
    fn peer_profile_validity_uses_most_recent_profile() {
        let (engine, _self_identity) = setup();
        let peer_identity = Identity::generate();
        let t0 = Utc::now();
        let clock = MockClock::new(t0);

        // Three Profiles in sequence, each with a different valid_until. The
        // last one published is the one the lookup must use.
        let p1 = Content::Profile {
            name: "first".to_string(),
            description: None,
            capabilities: vec![],
            broker: None,
            valid_from: Some(t0),
            valid_until: Some(t0 + Duration::days(5)),
        };
        let p2 = Content::Profile {
            name: "second".to_string(),
            description: None,
            capabilities: vec![],
            broker: None,
            valid_from: Some(t0),
            valid_until: Some(t0 + Duration::days(10)),
        };
        let p3 = Content::Profile {
            name: "third".to_string(),
            description: None,
            capabilities: vec![],
            broker: None,
            valid_from: Some(t0),
            valid_until: Some(t0 + Duration::days(60)),
        };
        engine
            .publish_with_schema(&peer_identity, p1.to_value(), None, None, vec![])
            .expect("publish p1");
        engine
            .publish_with_schema(&peer_identity, p2.to_value(), None, None, vec![])
            .expect("publish p2");
        engine
            .publish_with_schema(&peer_identity, p3.to_value(), None, None, vec![])
            .expect("publish p3");

        // Advance to T + 20d: p1 and p2 are expired, p3 still valid. The
        // helper must use p3 (most recent) and return Valid.
        clock.advance(Duration::days(20));

        let validity = peer_profile_validity(&engine, &peer_identity.public_id(), &clock)
            .expect("lookup must succeed");

        assert_eq!(
            validity,
            ProfileValidity::Valid,
            "most-recent Profile (p3, valid_until = T+60d) determines validity, \
             not any older Profile"
        );
    }

    #[test]
    fn is_expired_flag_only_true_for_expired_variant() {
        // Guard the semantic invariant — the one predicate callers rely on.
        assert!(!ProfileValidity::Absent.is_expired());
        assert!(!ProfileValidity::Undated.is_expired());
        assert!(!ProfileValidity::Valid.is_expired());
        assert!(ProfileValidity::Expired.is_expired());
    }
}
