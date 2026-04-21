//! Transport trait — see RFC 0001 §5.1 and §6.
//!
//! Narrow abstraction between `FeedEngine` and the wire. Any implementation
//! MUST uphold all seven invariants in RFC 0001 §6.

// TODO(Step 2): populate trait signatures per plan §2.2 (publish, subscribe,
// request_from, start, shutdown, health) and add `#[async_trait]`.
pub trait Transport {}
