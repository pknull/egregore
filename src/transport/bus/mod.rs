//! NATS JetStream transport adapter — RFC 0001 §9–§10.2, Phase 2.
//!
//! This module implements `BusTransport`, a `Transport`-trait adapter over
//! NATS JetStream. Scope is Phase 2 Wave 1 (foundation) → Wave 4 (dispatch
//! integration). See:
//!
//! - `Work/implementation/2026-04-23--phase-2-plan.md` (base plan)
//! - `Work/implementation/2026-04-23--phase-2-plan-amendments.md` (binding deltas)
//! - `docs/rfcs/0001-transport-abstraction.md` (trait spec)
//!
//! Submodules:
//! - `config` — `BusConfig`, `BrokerConfigInput`, `Config::validate` extension
//! - `subjects` — NATS subject mapping helpers (author → subject, consumer name)
//! - `consumer` — JetStream stream + consumer bootstrap
//! - `transport` — `BusTransport` struct + `Transport` trait impl
//! - `ingest` — durable-local-ingest precondition (RFC 0002 §8.2)

pub mod config;
pub mod consumer;
pub mod ingest;
pub mod subjects;
pub mod transport;

pub use self::config::{BrokerConfigInput, BusConfig};
pub use self::transport::BusTransport;
