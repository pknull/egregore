//! `TransportHealth` — see RFC 0001 §5.2.
//!
//! Coarse liveness signal surfaced by each `Transport` implementation and
//! aggregated across children by composite transports.

// TODO(Step 2): populate fields per plan §3.1 (connected, backend,
// last_successful_publish, last_peer_contact, unreplicated_count,
// inflight_publishes, last_error, children) and add
// `TransportHealth::aggregate` per plan §3.2.
pub struct TransportHealth;
