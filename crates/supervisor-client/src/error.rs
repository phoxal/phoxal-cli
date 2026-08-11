//! Every way an attachment can fail, as a reason a caller can act on.
//!
//! A mismatch names both sides and what to do about it. An operator reading
//! one of these should never have to compare two version strings by hand.

use phoxal_bus::{BusError, QueryError, SourceLabelError};
use phoxal_runtime_contract::version::FrameworkVersion;

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
    Snapshot(#[from] phoxal_api::supervisor::snapshot::SnapshotError),

    /// The robot and this client were built on different framework
    /// compatibility lines. The whole remediation is the message: the operator
    /// is told which side to move and the exact command that moves it.
    #[error("{}", incompatible_framework(*robot, *client))]
    IncompatibleFramework {
        robot: FrameworkVersion,
        client: FrameworkVersion,
    },

    /// The frozen bootstrap answered with a document this client cannot read.
    /// Failing to decode the one permanently stable reply is itself the
    /// compatibility answer, so it is reported as one rather than as a
    /// transport fault; `detail` carries the schema tag the robot sent.
    #[error(
        "this robot answered the attachment bootstrap with a document this phoxal client cannot \
         read, so the two speak different framework contracts: {detail}"
    )]
    UnreadableConnectReply { detail: String },

    #[error("the execution failed before readiness: {0}")]
    ReadinessFailed(String),

    #[error("the execution stopped before it became ready")]
    StoppedBeforeReady,

    #[error("the supervisor identity was lost before the execution became ready")]
    DisconnectedBeforeReady,

    #[error("the supervisor snapshot stream ended before readiness")]
    SnapshotStreamClosed,

    /// A command needs the snapshot revision it fences on and no snapshot has
    /// been observed yet. The client refuses to guess a revision rather than
    /// send one the supervisor would have to reject.
    #[error(
        "this attachment has observed no supervisor snapshot yet, so there is no revision to \
         fence the command on"
    )]
    NoSnapshotRevision,

    /// The transport or the session failed.
    #[error(transparent)]
    Bus(#[from] BusError),

    /// A query failed, timed out, or found no responder.
    #[error(transparent)]
    Query(#[from] QueryError),
}

impl AttachError {
    /// Whether this failure means the two binaries speak different framework
    /// contracts. Such a failure is terminal: retrying it, or reading it as
    /// "nothing is running here", would hide the one fact the operator needs.
    #[must_use]
    pub const fn is_framework_mismatch(&self) -> bool {
        matches!(
            self,
            Self::IncompatibleFramework { .. } | Self::UnreadableConnectReply { .. }
        )
    }
}

/// The operator-facing account of one framework mismatch.
///
/// It renders only for a genuine line disagreement, because that is the only
/// thing the gate refuses; two trains on one line never reach it. The versions
/// named are still the exact ones the frozen bootstrap reported, since that is
/// the provenance an operator acts on.
///
/// Which side is asked to move follows from which side is behind: a client
/// older than the robot upgrades itself, and a client newer than the robot
/// names the line the robot would have to be rebuilt onto.
fn incompatible_framework(robot: FrameworkVersion, client: FrameworkVersion) -> String {
    let preamble = format!(
        "Cannot attach to this robot.\n\nRobot framework: {robot}\nThis client speaks framework: \
         {client}\n\nThese framework versions are not compatible.\n"
    );
    if ordering(robot) > ordering(client) {
        format!("{preamble}Upgrade this phoxal client:\n    phoxal self upgrade")
    } else {
        format!(
            "{preamble}Rebuild or redeploy the robot with a compatible framework,\nor use a \
             client that speaks framework {}.",
            robot.compatibility_line()
        )
    }
}

/// A version's ordering key. `FrameworkVersion` decides compatibility by its
/// line and deliberately carries no ordering; a diagnostic still needs to know
/// which side is behind.
const fn ordering(version: FrameworkVersion) -> (u16, u16, u16) {
    (version.major(), version.minor(), version.patch())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(robot: FrameworkVersion, client: FrameworkVersion) -> String {
        AttachError::IncompatibleFramework { robot, client }.to_string()
    }

    /// A client a line behind the robot is told to upgrade itself, with the
    /// command that does it.
    #[test]
    fn a_newer_robot_asks_the_operator_to_upgrade_the_client() {
        assert_eq!(
            rendered(
                FrameworkVersion::new(0, 57, 0),
                FrameworkVersion::new(0, 56, 2)
            ),
            "Cannot attach to this robot.\n\
             \n\
             Robot framework: 0.57.0\n\
             This client speaks framework: 0.56.2\n\
             \n\
             These framework versions are not compatible.\n\
             Upgrade this phoxal client:\n    \
             phoxal self upgrade"
        );
    }

    /// A client a line ahead of the robot cannot fix itself, so the message
    /// names the robot's own line instead.
    #[test]
    fn an_older_robot_asks_the_operator_to_move_the_robot() {
        assert_eq!(
            rendered(
                FrameworkVersion::new(0, 56, 2),
                FrameworkVersion::new(0, 57, 0)
            ),
            "Cannot attach to this robot.\n\
             \n\
             Robot framework: 0.56.2\n\
             This client speaks framework: 0.57.0\n\
             \n\
             These framework versions are not compatible.\n\
             Rebuild or redeploy the robot with a compatible framework,\n\
             or use a client that speaks framework 0.56.x."
        );
    }

    /// Two patches of one line are still two trains, but they interoperate, so
    /// neither text is ever rendered for them: the gate produces no error at
    /// all, in either direction.
    #[test]
    fn a_difference_within_one_line_is_not_a_mismatch() {
        let older = FrameworkVersion::new(0, 56, 1);
        let newer = FrameworkVersion::new(0, 56, 2);
        assert_ne!(older, newer);
        assert_eq!(older.compatibility_line(), newer.compatibility_line());
        assert!(older.is_compatible_with(newer));
        assert!(newer.is_compatible_with(older));
    }

    /// The line a remediation names is the framework's one spelling of it, not
    /// a second one invented here.
    #[test]
    fn the_remediation_names_the_robot_line_the_way_the_framework_spells_it() {
        let robot = FrameworkVersion::new(0, 56, 2);
        let rendered = rendered(robot, FrameworkVersion::new(0, 57, 0));
        assert!(
            rendered.contains(&format!("framework {}.", robot.compatibility_line())),
            "{rendered}"
        );
    }

    /// A released train names its major, so the remediation stays the release
    /// an operator can actually ask for.
    #[test]
    fn a_released_train_names_its_major_line() {
        let rendered = rendered(
            FrameworkVersion::new(1, 2, 3),
            FrameworkVersion::new(2, 0, 0),
        );
        assert!(rendered.contains("framework 1.x."), "{rendered}");
    }

    #[test]
    fn every_mismatch_shape_is_recognized_as_one() {
        assert!(
            AttachError::IncompatibleFramework {
                robot: FrameworkVersion::new(0, 56, 2),
                client: FrameworkVersion::CURRENT,
            }
            .is_framework_mismatch()
        );
        assert!(
            AttachError::UnreadableConnectReply {
                detail: "phoxal/supervisor-connect/v1".to_string(),
            }
            .is_framework_mismatch()
        );
        assert!(!AttachError::StoppedBeforeReady.is_framework_mismatch());
    }
}
