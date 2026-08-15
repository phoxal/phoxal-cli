//! Structured connection and operation failures.

use phoxal_bus::{BusError, BusFault, QueryError, SourceLabelError};
use phoxal_protocol::supervisor::execution::{SnapshotError, SupervisorFailure};
use phoxal_runtime_contract::identity::ExecutionId;
use phoxal_runtime_contract::version::FrameworkVersion;

/// Which peer is on the newer incompatible framework line.
///
/// This is a fact established from the two exact versions, not advice about
/// how an application should resolve it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompatibilityRefusal {
    /// The remote supervisor is newer than this client.
    RemoteNewer,
    /// This client is newer than the remote supervisor.
    ClientNewer,
}

impl std::fmt::Display for CompatibilityRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RemoteNewer => formatter.write_str("remote framework line is newer"),
            Self::ClientNewer => formatter.write_str("client framework line is newer"),
        }
    }
}

/// A failure while establishing one connection.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// The configured endpoint announced no execution.
    #[error("no Phoxal execution is reachable at {endpoint}")]
    NoExecution { endpoint: String },

    /// The configured endpoint announced more than one execution and therefore
    /// did not identify one connection target.
    #[error(
        "{count} Phoxal executions are reachable at {endpoint}, which must identify exactly one: {executions:?}"
    )]
    MultipleExecutions {
        endpoint: String,
        count: usize,
        executions: Vec<ExecutionId>,
    },

    /// The diagnostic label could not be represented by the framework bus.
    #[error(transparent)]
    SourceLabel(#[from] SourceLabelError),

    /// The peers were built from incompatible framework lines.
    #[error("remote framework {remote} is incompatible with client framework {client}: {refusal}")]
    IncompatibleFramework {
        remote: FrameworkVersion,
        client: FrameworkVersion,
        refusal: CompatibilityRefusal,
    },

    /// The frozen bootstrap returned a document this client could not decode.
    #[error("the frozen supervisor bootstrap reply could not be decoded: {detail}")]
    UnreadableBootstrap { detail: String },

    /// The supervisor identity was already absent when setup completed.
    #[error("the supervisor identity was lost while the connection was being established")]
    SupervisorUnavailable,

    /// The initial supervisor snapshot was internally inconsistent.
    #[error("the supervisor returned an invalid initial snapshot: {0}")]
    Snapshot(#[from] SnapshotError),

    /// The underlying transport failed while the connection was opening.
    #[error(transparent)]
    Bus(#[from] BusError),

    /// A bootstrap or initial-state query failed.
    #[error(transparent)]
    Query(#[from] QueryError),
}

impl ConnectError {
    /// Whether the peer answered the frozen bootstrap but could not be admitted
    /// as a compatible framework peer.
    #[must_use]
    pub const fn is_compatibility_refusal(&self) -> bool {
        matches!(
            self,
            Self::IncompatibleFramework { .. } | Self::UnreadableBootstrap { .. }
        )
    }
}

/// The terminal fact that ended an established connection.
///
/// The first observed reason is latched for the connection's lifetime. A
/// later close request cannot overwrite an earlier supervisor, snapshot, or
/// transport failure.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DisconnectReason {
    /// The unique connection owner explicitly closed or was dropped.
    ConnectionClosed,
    /// The supervisor's execution-scoped identity token was lost.
    SupervisorIdentityLost,
    /// The authoritative supervisor snapshot stream failed.
    SnapshotStreamFailed { detail: String },
    /// An owner-owned transport worker failed.
    TransportFault { fault: BusFault },
    /// The private lifecycle channel ended without publishing a cause.
    LifecycleEnded,
}

impl std::fmt::Display for DisconnectReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConnectionClosed => formatter.write_str("the connection owner closed"),
            Self::SupervisorIdentityLost => {
                formatter.write_str("the supervisor identity token was lost")
            }
            Self::SnapshotStreamFailed { detail } => {
                write!(formatter, "the supervisor snapshot stream failed: {detail}")
            }
            Self::TransportFault { fault } => write!(formatter, "transport fault: {fault}"),
            Self::LifecycleEnded => {
                formatter.write_str("the connection lifecycle ended without a terminal cause")
            }
        }
    }
}

/// A failure while using an established client.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The established connection reached a terminal state.
    #[error("the connection ended: {reason}")]
    Disconnected { reason: DisconnectReason },

    /// The execution entered a failed lifecycle before becoming ready.
    #[error("the execution failed before readiness: {failure:?}")]
    ReadinessFailed { failure: Option<SupervisorFailure> },

    /// The execution stopped before becoming ready.
    #[error("the execution stopped before it became ready")]
    StoppedBeforeReady,

    /// The underlying typed transport operation failed.
    #[error(transparent)]
    Bus(#[from] BusError),

    /// A typed query failed.
    #[error(transparent)]
    Query(#[from] QueryError),
}

/// A failure while deterministically closing the unique connection owner.
#[derive(Debug, thiserror::Error)]
pub enum CloseError {
    /// The bus completed close with retained transport or worker evidence.
    #[error("the connection transport did not close cleanly: {detail}")]
    Transport { detail: String },

    /// The private lifecycle task failed before returning close evidence.
    #[error("the connection lifecycle task failed: {detail}")]
    Lifecycle { detail: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refusal(remote: FrameworkVersion, client: FrameworkVersion) -> ConnectError {
        crate::connection::ensure_compatible_framework(remote, client)
            .expect_err("different lines are incompatible")
    }

    #[test]
    fn compatibility_refusal_preserves_versions_and_which_peer_is_newer() {
        let older = FrameworkVersion::new(0, 60, 4);
        let newer = FrameworkVersion::new(0, 61, 2);

        assert!(matches!(
            refusal(newer, older),
            ConnectError::IncompatibleFramework {
                remote,
                client,
                refusal: CompatibilityRefusal::RemoteNewer,
            } if remote == newer && client == older
        ));
        assert!(matches!(
            refusal(older, newer),
            ConnectError::IncompatibleFramework {
                remote,
                client,
                refusal: CompatibilityRefusal::ClientNewer,
            } if remote == older && client == newer
        ));
    }

    #[test]
    fn compatibility_errors_are_neutral_structured_facts() {
        let error = refusal(
            FrameworkVersion::new(0, 61, 0),
            FrameworkVersion::new(0, 60, 0),
        );
        let rendered = error.to_string();
        assert!(rendered.contains("0.61.0"), "{rendered}");
        assert!(rendered.contains("0.60.0"), "{rendered}");
        assert!(
            rendered.contains("remote framework line is newer"),
            "{rendered}"
        );
    }
}
