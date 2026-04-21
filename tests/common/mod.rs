//! Shared helpers for transport-invariant integration tests.
//!
//! Rust integration tests under `tests/` don't implicitly share modules; the
//! convention is to put shared code under `tests/common/mod.rs` and declare
//! `mod common;` at the top of each test file that uses it. See plan §11 OQ-7.

#![allow(dead_code)] // some helpers are used only by a subset of test files

pub mod mock_transport;

use egregore::feed::engine::FeedEngine;
use egregore::feed::models::Message;
use egregore::feed::store::FeedStore;
use egregore::identity::Identity;

/// Build a signed chain of `n` messages for a fresh author.
///
/// Spins up a throwaway "remote" engine, calls `publish` n times, returns the
/// resulting signed `Message`s in sequence order alongside the identity that
/// signed them (so tests can filter by author).
///
/// `n` must be >= 1. The content is a minimal `{"type":"test","seq":i}`
/// payload; non-strict schema validation allows this.
pub fn build_chain(n: usize) -> (Identity, Vec<Message>) {
    assert!(n >= 1, "chain length must be at least 1");
    let store = FeedStore::open_memory().expect("in-memory store");
    let engine = FeedEngine::new(store);
    let identity = Identity::generate();

    let mut chain = Vec::with_capacity(n);
    for i in 1..=n {
        let msg = engine
            .publish(
                &identity,
                serde_json::json!({"type": "test", "seq": i}),
                None,
                vec![],
            )
            .expect("publish signed message");
        chain.push(msg);
    }
    (identity, chain)
}

/// Build a signed chain with a specific set of tags attached to each message.
///
/// Used by the filter-honesty test (B.6) — each message in the returned chain
/// carries the same tag set so the subscriber's filter predicate is
/// deterministic.
pub fn build_tagged_chain(n: usize, tags: Vec<String>) -> (Identity, Vec<Message>) {
    assert!(n >= 1, "chain length must be at least 1");
    let store = FeedStore::open_memory().expect("in-memory store");
    let engine = FeedEngine::new(store);
    let identity = Identity::generate();

    let mut chain = Vec::with_capacity(n);
    for i in 1..=n {
        let msg = engine
            .publish(
                &identity,
                serde_json::json!({"type": "test", "seq": i}),
                None,
                tags.clone(),
            )
            .expect("publish signed message");
        chain.push(msg);
    }
    (identity, chain)
}
