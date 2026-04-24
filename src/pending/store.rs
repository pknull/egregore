//! Re-exports for pending-forwarding row types.
//!
//! The SQLite CRUD lives in `feed/store/pending.rs` (so it sits next to the
//! other `FeedStore` CRUD and shares the schema migration entry point). This
//! module re-exports the row + result types so consumers in `pending::` do
//! not have to reach into the feed module's internals.
