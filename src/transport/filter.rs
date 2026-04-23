//! `TopicFilter` — see RFC 0001 §5.3.
//!
//! Subscription filter carrying optional author and tag predicates.
//! Transports honor Invariant 6 (filter honesty): the delivered set is a
//! superset of the match set; a subset is a bug. Filter evaluation is
//! best-effort at the wire layer — canonical filtering still happens at the
//! engine / consumer boundary where Message contents are fully decoded.

use crate::identity::PublicId;

/// Filter applied to a live `Transport::subscribe` stream.
///
/// Both fields are `Option<Vec<_>>` rather than plain `Vec<_>` so that `None`
/// is unambiguously "no predicate on this dimension" rather than "match
/// nothing". An empty `Some(vec![])` is reserved as "match nothing on this
/// dimension" for future use; Phase 1 implementations MAY treat it as a
/// no-match but MUST NOT panic.
#[derive(Clone, Debug, Default)]
pub struct TopicFilter {
    /// Deliver only messages authored by one of these public IDs.
    pub authors: Option<Vec<PublicId>>,

    /// Deliver only messages whose `tags` intersect this set.
    pub tags: Option<Vec<String>>,
}
