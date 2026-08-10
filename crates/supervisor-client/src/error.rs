//! Every way an attachment can fail, as a reason a caller can act on.
//!
//! A mismatch names both sides and what to do about it. An operator reading
//! one of these should never have to compare two version strings by hand.

use phoxal_bus::{BusError, QueryError, SourceLabelError};

/// A failure while establishing or running an attachment.
#[derive(Debug, thiserror::Error)]
pub enum AttachError {
    /// The session reached the endpoint but no router announced itself. The
    /// endpoint is not a Phoxal execution, or the daemon is not up yet.
    #[error(
        "no robot router is reachable at {endpoint}: the endpoint answered but announced no \
         router, so there is no execution to attach to"
    )]
    NoRouter { endpoint: String },

    /// More than one router is directly connected. An endpoint means exactly
    /// one robot-owned router; selecting among several behind a shared fabric
    /// is deliberately out of scope.
    #[error(
        "{count} robot routers are reachable at {endpoint} ({routers}), but an endpoint must \
         name exactly one execution; connect to one robot's own endpoint"
    )]
    MultipleRouters {
        endpoint: String,
        count: usize,
        routers: String,
    },

    #[error(transparent)]
    SourceLabel(#[from] SourceLabelError),

    #[error("the attachment transport did not close cleanly: {0}")]
    Close(String),

    #[error("the supervisor returned an invalid snapshot: {0}")]
    Snapshot(#[from] phoxal_supervisor_api::SnapshotError),

    #[error("the execution failed before readiness: {0}")]
    ReadinessFailed(String),

    #[error("the execution stopped before it became ready")]
    StoppedBeforeReady,

    #[error("the supervisor identity was lost before the execution became ready")]
    DisconnectedBeforeReady,

    #[error("the supervisor snapshot stream ended before readiness")]
    SnapshotStreamClosed,

    /// The transport or the session failed.
    #[error(transparent)]
    Bus(#[from] BusError),

    /// A query failed, timed out, or found no responder.
    #[error(transparent)]
    Query(#[from] QueryError),
}
