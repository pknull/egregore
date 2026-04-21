//! Transport abstraction — narrow interface between `FeedEngine` and the wire.
//!
//! See RFC 0001 §5.1 for the trait rationale and §6 for the seven invariants
//! that every implementation MUST uphold (per-author FIFO, no silent drop,
//! at-least-once delivery, envelope preservation, gap detectability,
//! filter honesty, shutdown drains).
//!
//! Phase 1 scope: trait + types + `GossipTransport` adapter over the existing
//! `gossip/` stack. Step 2 populates the trait, `TopicFilter`,
//! `SubscriptionHandle`, and `TransportHealth` (with its aggregation helper).
//! `GossipTransport` itself lands in Step 3 — see
//! `Work/implementation/2026-04-21--phase-1-plan.md`.

pub mod filter;
pub mod gossip;
pub mod health;
pub mod subscription;
pub mod trait_def;

pub use self::filter::TopicFilter;
pub use self::health::TransportHealth;
pub use self::subscription::SubscriptionHandle;
pub use self::trait_def::Transport;

/// Emit the WARN-level startup log line required by RFC 0002 §4 Principle 7
/// when the engine is configured with two or more transports.
///
/// Factored out so the conditional + log call can be unit-tested via
/// tracing-subscriber capture; `main.rs` invokes this once during boot,
/// after the gossip transport is attached and before the node accepts
/// inbound traffic.
///
/// The exact log string is normative (RFC 0002 §4 Principle 7 / §10.4 RL10);
/// changes require an RFC amendment.
pub fn announce_if_multi_transport(engine: &crate::feed::engine::FeedEngine) {
    if engine.transport_count() >= 2 {
        tracing::warn!(
            "Multi-transport (bridge) mode active — forwarding all signed messages"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use tracing_subscriber::fmt::MakeWriter;

    use super::{announce_if_multi_transport, Transport, TransportHealth};
    use crate::error::Result;
    use crate::feed::engine::FeedEngine;
    use crate::feed::models::Message;
    use crate::feed::store::FeedStore;
    use crate::identity::PublicId;
    use crate::transport::filter::TopicFilter;
    use crate::transport::subscription::SubscriptionHandle;

    // The exact RFC 0002 §4 Principle 7 / §10.4 RL10 log message. Duplicated
    // verbatim here (rather than shared via a `const`) so the test exercises
    // the rendered output character-for-character and catches drift in the
    // shim's literal string.
    const RFC_WARN_LINE: &str =
        "Multi-transport (bridge) mode active — forwarding all signed messages";

    /// Shared `Vec<u8>` writer for capturing `tracing` output in tests.
    /// Implements `io::Write` (so `tracing-subscriber` can write into it) and
    /// `MakeWriter` (so it can serve as the subscriber's writer factory).
    #[derive(Clone, Default)]
    struct BufferWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for BufferWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for BufferWriter {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Inline test-only mock mirroring the pattern in `engine.rs` tests.
    /// Only `health()` has a real impl; the other trait methods panic because
    /// they are not exercised by the shim tests.
    struct InlineMock {
        health: TransportHealth,
    }

    impl InlineMock {
        fn new(backend: &'static str) -> Arc<Self> {
            Arc::new(Self {
                health: TransportHealth {
                    connected: true,
                    backend,
                    last_successful_publish: None,
                    last_peer_contact: None,
                    unreplicated_count: 0,
                    inflight_publishes: 0,
                    last_error: None,
                    children: vec![],
                },
            })
        }
    }

    #[async_trait]
    impl Transport for InlineMock {
        async fn publish(&self, _msg: &Message) -> Result<()> {
            unimplemented!("InlineMock::publish — not exercised by shim tests")
        }
        async fn subscribe(
            &self,
            _filter: TopicFilter,
        ) -> Result<(SubscriptionHandle, BoxStream<'static, Message>)> {
            unimplemented!("InlineMock::subscribe — not exercised by shim tests")
        }
        async fn request_from(
            &self,
            _author: PublicId,
            _after_seq: u64,
        ) -> Result<BoxStream<'static, Message>> {
            unimplemented!("InlineMock::request_from — not exercised by shim tests")
        }
        async fn start(&self) -> Result<()> {
            Ok(())
        }
        async fn shutdown(&self, _deadline: Duration) -> Result<()> {
            Ok(())
        }
        fn health(&self) -> TransportHealth {
            self.health.clone()
        }
    }

    /// Run `f` with a `tracing` subscriber whose writer buffers output,
    /// then return the captured UTF-8 string.
    fn capture_tracing<F: FnOnce()>(f: F) -> String {
        let buffer = BufferWriter::default();
        let captured = buffer.0.clone();

        let subscriber = tracing_subscriber::fmt()
            .with_writer(buffer)
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .finish();

        tracing::subscriber::with_default(subscriber, f);

        let bytes = captured.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn silent_for_single_transport() {
        let store = FeedStore::open_memory().unwrap();
        let engine = FeedEngine::new(store);
        engine.attach_transport(InlineMock::new("mock-a"));
        assert_eq!(engine.transport_count(), 1);

        let output = capture_tracing(|| announce_if_multi_transport(&engine));
        assert!(
            output.is_empty(),
            "single-transport node must NOT emit the bridge WARN line; got: {output:?}"
        );
    }

    #[test]
    fn announces_for_multi_transport() {
        let store = FeedStore::open_memory().unwrap();
        let engine = FeedEngine::new(store);
        engine.attach_transport(InlineMock::new("mock-a"));
        engine.attach_transport(InlineMock::new("mock-b"));
        assert_eq!(engine.transport_count(), 2);

        let output = capture_tracing(|| announce_if_multi_transport(&engine));

        assert!(
            output.contains(RFC_WARN_LINE),
            "multi-transport node must emit the RFC 0002 §4 Principle 7 WARN line \
             verbatim (em-dash, not hyphen); got: {output:?}"
        );
        assert!(
            output.contains("WARN"),
            "log must be emitted at WARN level; got: {output:?}"
        );
    }
}
