//! `SubscriptionHandle` — opaque. Drop cancels. See plan §2.3.
//!
//! Public type is intentionally opaque; adapters construct their own variant
//! via a `pub(crate)` inner enum so the public API does not leak adapter
//! details.

// TODO(Step 2): add enum-backed inner per plan §2.3 (SubscriptionInner with
// per-adapter cancel primitives; Drop impl that fires the cancel).
pub struct SubscriptionHandle;
