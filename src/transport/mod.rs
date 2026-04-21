//! Transport abstraction — narrow interface between `FeedEngine` and the wire.
//!
//! See RFC 0001 §5.1 for the trait rationale and §6 for the seven invariants
//! that every implementation MUST uphold (per-author FIFO, no silent drop,
//! at-least-once delivery, envelope preservation, gap detectability,
//! filter honesty, shutdown drains).
//!
//! Phase 1 scope: trait + types + `GossipTransport` adapter over the existing
//! `gossip/` stack. This module is scaffolding in Step 1 — functional contents
//! land in Steps 2+ of `Work/implementation/2026-04-21--phase-1-plan.md`.

pub mod filter;
pub mod gossip;
pub mod health;
pub mod subscription;
pub mod trait_def;

pub use self::filter::TopicFilter;
pub use self::health::TransportHealth;
pub use self::subscription::SubscriptionHandle;
pub use self::trait_def::Transport;
