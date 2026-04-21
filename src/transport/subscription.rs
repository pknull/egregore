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
// TODO(Step 3): `GossipTransport::subscribe` is the first constructor of
// `SubscriptionInner::Gossip`; the allow-dead-code annotations below are
// removed once Step 3 wires the first caller.
#[allow(dead_code)]
pub(crate) enum SubscriptionInner {
    /// Gossip adapter cancel channel. `GossipTransport` owns the receiver
    /// side and shuts down its per-subscription task when the sender is
    /// dropped.
    Gossip(oneshot::Sender<()>),
}

impl SubscriptionHandle {
    /// Construct a Gossip-variant handle. Called by `GossipTransport` in
    /// Step 3+; kept `pub(crate)` so adapters inside the crate can build it.
    // TODO(Step 3): remove `#[allow(dead_code)]` once `GossipTransport`
    // calls this from its `subscribe` implementation.
    #[allow(dead_code)]
    pub(crate) fn gossip(cancel: oneshot::Sender<()>) -> Self {
        Self {
            inner: SubscriptionInner::Gossip(cancel),
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
            SubscriptionInner::Gossip(_cancel) => {
                // dropped implicitly; nothing to do. Keeping the branch
                // explicit documents the contract for future variants.
            }
        }
    }
}
