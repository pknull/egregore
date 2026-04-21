//! `GossipTransport` — wraps `egregore/src/gossip/` stack behind the
//! `Transport` trait. See plan §4 Step 3.
//!
//! Delegates to the existing `ConnectionRegistry`, `PushManager`, sync loop,
//! and server without relocating any code from `gossip/`.

// TODO(Step 3): populate GossipTransport struct + impl per plan §4 Step 3-4
// (fields: Arc<ConnectionRegistry>, network_key, Identity, Arc<FeedEngine>,
// shutdown signal, server join handle; methods: new/start/shutdown/health,
// then publish/subscribe/request_from in Step 4+).
