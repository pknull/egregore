//! Pending-forwarding state + retry scheduler — RFC 0001 §10 Phase 2.
//!
//! Tracks messages accepted locally but not yet acknowledged by a given
//! transport backend (initially `"bus"`; extension point for future
//! backends). On transport recovery, the retry scheduler opportunistically
//! re-drains the pending set.
//!
//! Submodules:
//! - `store` — durable row helpers (delegates to `feed/store/pending.rs`
//!   CRUD; re-exports result types)
//! - `scheduler` — retry loop (`run_retry_scheduler`)

pub mod scheduler;
pub mod store;
