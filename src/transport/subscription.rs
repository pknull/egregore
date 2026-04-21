//! `SubscriptionHandle` — opaque. Drop cancels. See plan §2.3.
//!
//! Public type is intentionally opaque; adapters construct their own variant
//! via a `pub(crate)` inner enum so the public API does not leak adapter
//! details. Dropping the handle fires the adapter-specific cancel primitive
//! and the associated `BoxStream` ends on its next poll (or soon thereafter
//! at the adapter's discretion, within the bounds of Invariant 7).

use tokio::sync::oneshot;

/// Opaque subscription handle. Dropping the handle cancels the subscription.
///
/// The inner shape is `pub(crate)` so adapters in this crate construct their
/// own variant; consumers outside the crate cannot introspect or match.
pub struct SubscriptionHandle {
    pub(crate) inner: SubscriptionInner,
}

/// Per-adapter cancel primitives.
///
/// Variant choice is intentionally a `oneshot::Sender<()>` — smaller and
/// cheaper than `AbortHandle`, and more uniform across adapters that may
/// have their own loop structures. Senders are dropped (not sent on) in
/// the `Drop` impl; the receiver's `.await` returns `Err(RecvError)` which
/// adapters treat as a cancel signal.
///
/// Phase 2 adds `Nats(oneshot::Sender<()>)` and
/// `Composite(Vec<SubscriptionHandle>)` variants.
pub(crate) enum SubscriptionInner {
    /// Generic oneshot-based cancel channel, used by both the `GossipTransport`
    /// adapter and test-only adapters (`MockTransport`). A single variant
    /// suffices because all current adapters share the same cancel shape;
    /// Phase 2 adds additional variants as new wire backends land.
    OneshotCancel(oneshot::Sender<()>),
}

impl SubscriptionHandle {
    /// Construct a handle whose cancel primitive is a `tokio::sync::oneshot`
    /// sender. When the handle drops, the sender drops, and the adapter's
    /// receiver observes `Err(RecvError)` — which the adapter interprets as
    /// "stop the subscription."
    ///
    /// This is the single construction path for Phase-1 adapters (both
    /// `GossipTransport` and the integration-test `MockTransport`). The
    /// constructor is `pub` rather than `pub(crate)` because integration
    /// tests live in a separate crate (`egregore/tests/`) and need to build
    /// their own adapter impls; the opaque return type keeps the cancel
    /// primitive unobservable to external callers.
    pub fn from_cancel(cancel: oneshot::Sender<()>) -> Self {
        Self {
            inner: SubscriptionInner::OneshotCancel(cancel),
        }
    }
}

impl Drop for SubscriptionHandle {
    fn drop(&mut self) {
        // Firing the cancel is as simple as letting the sender drop.
        // `oneshot::Receiver::recv().await` on the adapter side resolves to
        // `Err(RecvError)` which the adapter interprets as "stop".
        //
        // This branch exists so future variants (Nats, Composite) have a
        // dedicated hook; today it is effectively a no-op because drop of
        // the struct already drops its fields.
        match &mut self.inner {
            SubscriptionInner::OneshotCancel(_cancel) => {
                // dropped implicitly; nothing to do. Keeping the branch
                // explicit documents the contract for future variants.
            }
        }
    }
}
