//! Every way an attachment can fail, as a reason a caller can act on.
//!
//! A mismatch names both sides and what to do about it. An operator reading
//! one of these should never have to compare two version strings by hand.

use phoxal_bus::{BusError, QueryError};
use phoxal_runtime_contract::{ApiId, InvalidIdentity, SchemaId};
use phoxal_supervisor_api::{BundlePathRejection, SchemaSurface};

use crate::router::RouterId;

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

    /// More than one router is directly connected. For #978 an endpoint means
    /// exactly one robot-owned router; selecting among several behind a shared
    /// fabric is deliberately out of scope.
    #[error(
        "{count} robot routers are reachable at {endpoint} ({routers}), but an endpoint must \
         name exactly one execution; connect to one robot's own endpoint"
    )]
    MultipleRouters {
        endpoint: String,
        count: usize,
        routers: String,
    },

    /// A router announced an identity that is not a Phoxal execution id.
    #[error("router '{router}' is not a phoxal execution: {source}")]
    RouterIdentity {
        router: RouterId,
        #[source]
        source: InvalidIdentity,
    },

    /// The daemon speaks a different robot API revision.
    #[error(
        "the supervisor at {endpoint} runs robot API {daemon}, but this client speaks {client}; \
         the client and the daemon must be built from the same train - update the phoxal CLI \
         pair, or restart the daemon on the matching build"
    )]
    ApiMismatch {
        endpoint: String,
        client: ApiId,
        daemon: ApiId,
    },

    /// The daemon advertises a different schema for a surface this client
    /// intends to consume.
    #[error(
        "the supervisor at {endpoint} speaks {surface} schema '{daemon}', but this client \
         requires '{client}'; the client and the daemon must be built from the same train - \
         update the phoxal CLI pair, or restart the daemon on the matching build"
    )]
    SchemaMismatch {
        endpoint: String,
        surface: SchemaSurface,
        client: SchemaId,
        daemon: SchemaId,
    },

    /// The connected execution is not the one this attachment was established
    /// against: the daemon restarted, which is a new attachment rather than a
    /// reconnection.
    #[error("the endpoint now runs execution {found}, not {expected}: this is a new execution")]
    ExecutionChanged {
        expected: phoxal_runtime_contract::ExecutionId,
        found: phoxal_runtime_contract::ExecutionId,
    },

    /// A local check refused a bundle path before it reached the wire.
    #[error("'{path}' is not a bundle-relative path: {reason:?}")]
    InvalidBundlePath {
        path: String,
        reason: BundlePathRejection,
    },

    /// The transport or the session failed.
    #[error(transparent)]
    Bus(#[from] BusError),

    /// A query failed, timed out, or found no responder.
    #[error(transparent)]
    Query(#[from] QueryError),

    /// The fabric abstraction could not answer.
    #[error("the transport could not report {operation}: {detail}")]
    Fabric {
        operation: &'static str,
        detail: String,
    },
}

impl AttachError {
    /// Whether this failure means the attachment is gone rather than that one
    /// operation failed. A caller re-runs the whole handshake for these.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::ExecutionChanged { .. } | Self::Bus(BusError::Closed)
        )
    }
}
