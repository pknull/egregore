//! `CompositeTransport` — multi-transport bridge adapter. RFC 0002 §8.
//!
//! Phase 2 Wave 1 lays scaffolding only; the trait impl and ingress/egress
//! tasks land in Wave 3 per the amended step ordering. See:
//!
//! - `Work/implementation/2026-04-23--phase-2-plan.md` (base plan §6)
//! - `Work/implementation/2026-04-23--phase-2-plan-amendments.md` (§C.5 N>2 ack barrier)
//! - `docs/rfcs/0002-transport-bridge-mode.md` §8 (flow control)

pub mod direction;
pub mod egress;
pub mod health;
pub mod ingress;
pub mod transport;
