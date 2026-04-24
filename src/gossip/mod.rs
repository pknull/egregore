pub mod backoff;
pub mod bloom;
pub mod client;
pub mod connection;
pub mod discovery;
pub mod flow_control;
pub mod health;
pub mod log_dedup;
pub mod mdns;
pub mod peers;
pub mod persistent;
// Phase 2 Wave 4 Step 23: `push` module retired. Its job — broadcasting
// locally-authored messages to connected peers — is now handled by the
// `publish_dispatcher` task (Wave 4 Step 22) which reads DispatchTicket
// values off the engine's bounded mpsc and calls
// `GossipTransport::publish(&msg)` → `registry.broadcast(msg)`. The chain
// is semantically identical to the retired `event_tx → PushManager →
// registry.broadcast` but now routes through the `Transport` trait so
// multi-transport deployments work uniformly.
pub mod registry;
pub mod replication;
pub mod server;
