//! `TopicFilter` — see RFC 0001 §5.3.
//!
//! Subscription filter carrying optional author and tag predicates.
//! Transports honor Invariant 6 (filter honesty): delivered set is a superset
//! of the match set; subset is a bug.

// TODO(Step 2): populate per plan §2.3 (authors: Option<Vec<PublicId>>,
// tags: Option<Vec<String>>, derive Clone + Debug + Default).
pub struct TopicFilter;
