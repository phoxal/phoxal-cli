//! The one transport capability this client needs and `phoxal-bus` does not
//! yet expose.
//!
//! # NEEDED-FROM-FRAMEWORK
//!
//! Direct attachment is defined by the router a session is *actually* connected
//! to, and disconnection is defined by the supervisor's liveliness token. Both
//! are Zenoh facts that `phoxal-bus` already holds privately but does not
//! publish:
//!
//! - the zid-to-[`ExecutionId`](phoxal_runtime_contract::ExecutionId)
//!   conversion (`identity::producer_from_zid`'s counterpart) is `pub(crate)`;
//! - the shared transport policy (`apply_phoxal_transport_policy`: 3s lease,
//!   4 keepalives, **multicast scouting off**) is `pub(crate)`, and its own
//!   docs say a peer disagreeing with the router about lease or keepalive
//!   produces exactly the link churn that is hardest to diagnose;
//! - generic liveliness is participant-scoped
//!   (`liveliness/participants/{id}/{producer}`), so there is no way to observe
//!   one exact key.
//!
//! Reproducing any of that outside the framework would fork the transport
//! policy into a second source, so this crate does not. Instead it names the
//! gap as a two-method trait. The proposed home is `phoxal-bus`:
//!
//! ```ignore
//! impl Bus {
//!     /// Probe an endpoint for the executions whose routers it is directly
//!     /// connected to, using the shared transport policy. The session is
//!     /// opened and closed by this call: an execution-scoped `Bus` cannot be
//!     /// opened before its execution is known.
//!     pub async fn probe_executions(endpoint: &str) -> Result<Vec<ExecutionId>>;
//!
//!     /// Observe one exact liveliness key below this session's root.
//!     pub async fn observe_liveliness(
//!         &self,
//!         relative_key: &str,
//!         on_lost: impl Fn() + Send + Sync + 'static,
//!     ) -> Result<LivelinessObserver>;
//! }
//! ```
//!
//! With those two, this module collapses to their call sites and the trait
//! disappears.

use std::any::Any;

use async_trait::async_trait;
use phoxal_bus::Bus;
use tokio::sync::oneshot;

use crate::error::AttachError;
use crate::router::RouterId;

/// A live observation of the supervisor's identity token.
///
/// Holding it keeps the observation declared; dropping it stops watching.
pub struct IdentityWatch {
    lost: oneshot::Receiver<()>,
    /// Whatever the implementation must keep alive for the observation to stay
    /// declared. Opaque so this crate never names a transport type.
    _declaration: Box<dyn Any + Send + Sync>,
}

impl IdentityWatch {
    /// Build a watch from a loss signal and the declaration that produces it.
    #[must_use]
    pub fn new(lost: oneshot::Receiver<()>, declaration: Box<dyn Any + Send + Sync>) -> Self {
        Self {
            lost,
            _declaration: declaration,
        }
    }

    /// Resolve when the supervisor's token is lost.
    ///
    /// Cancellation-safe: this is a `oneshot` receiver, so a `select!` arm that
    /// loses the race has consumed nothing. A dropped sender - the observation
    /// itself going away - counts as loss, because a watch that can no longer
    /// report is not evidence of presence.
    pub async fn lost(&mut self) {
        let _ = (&mut self.lost).await;
    }
}

/// The transport facts direct attachment is defined by.
///
/// One trait, two methods, both destined for `phoxal-bus` - see this module's
/// docs. A caller passes an implementation to
/// [`Attachment::open`](crate::Attachment::open); tests pass a fake.
#[async_trait]
pub trait SupervisorFabric: Send + Sync + 'static {
    /// The routers a session at `endpoint` is *directly* connected to.
    ///
    /// Multicast scouting must be disabled: for #978 an endpoint means one
    /// robot-owned router, and a scouted third party is not one.
    async fn directly_connected_routers(
        &self,
        endpoint: &str,
    ) -> Result<Vec<RouterId>, AttachError>;

    /// Observe the supervisor's liveliness token at `key`, an execution-rooted
    /// absolute key produced by
    /// [`identity_key`](phoxal_supervisor_api::identity_key).
    ///
    /// The observation must include already-live tokens: a client that attaches
    /// after the daemon declared its token must still see it, and must be told
    /// immediately if it is already gone.
    async fn watch_identity(&self, bus: &Bus, key: &str) -> Result<IdentityWatch, AttachError>;
}
