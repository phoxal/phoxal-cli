//! Exact participant readiness observed by the resident authority.
//!
//! The bus session that feeds this lives in [`crate::serve`]; this module owns
//! only what a Liveliness event means for the board.

use crate::SupervisorState;
use phoxal_bus::{LivelinessStatus, ParticipantLivelinessEvent};
use phoxal_cli_core::project::launch_plan::DEFAULT_ROUTER_CONNECT;
use phoxal_cli_core::runtime::{ParticipantInstanceKey, RobotKey};

/// The supervisor's own participant id on the robot bus. It observes
/// Liveliness and answers the contracts the supervisor owns; it declares no
/// Liveliness token of its own and is not a graph participant.
pub(crate) const SUPERVISOR_SESSION_ID: &str = "phoxal-cli-liveliness-observer";

#[cfg(test)]
use crate::ProcessState;
#[cfg(test)]
use phoxal_cli_core::identity::ProducerId;

pub(crate) fn apply_liveliness_event(
    board: &SupervisorState,
    namespace: &str,
    robot_id: &str,
    event: ParticipantLivelinessEvent,
) {
    let id = event.key.participant();
    // Participant ids are the launch plan's validated, robot-scoped flat
    // namespace. The framework documents that a session observing a key it also holds
    // can receive an uncompensated self-Lost after duplicate-key
    // reconciliation. This observer does not normally declare a token, but
    // filtering its own id keeps that invariant explicit.
    if id == SUPERVISOR_SESSION_ID {
        return;
    }
    board.record_instance_presence(
        ParticipantInstanceKey {
            robot: RobotKey::new(namespace, robot_id),
            participant: id.to_string(),
        },
        event.key.producer(),
        event.status == LivelinessStatus::Alive,
    );
}

#[must_use]
pub fn default_connect_endpoint() -> String {
    DEFAULT_ROUTER_CONNECT.to_string()
}

#[cfg(test)]
mod tests {

    /// A deterministic producer identity for tests, so a case can name the
    /// exact restart it means.
    fn producer(seed: u8) -> ProducerId {
        // A producer id is the publisher's session ZID: 1 to 32 lowercase hex
        // characters with no leading zero, so a seed renders bare.
        ProducerId::parse(&format!("{seed:x}")).expect("test producer id must parse")
    }
    use super::*;
    use phoxal_bus::ParticipantLivelinessKey;
    use phoxal_cli_core::runtime::ParticipantKind;

    fn event(participant: &str, status: LivelinessStatus) -> ParticipantLivelinessEvent {
        ParticipantLivelinessEvent {
            key: ParticipantLivelinessKey::new("dev/robots/rover/xdead", participant, producer(7))
                .expect("valid participant key"),
            status,
        }
    }

    #[test]
    fn observer_events_drive_presence_without_becoming_restart_authority() {
        let board = SupervisorState::new();
        let key = phoxal_cli_core::runtime::ProcessKey::robot(
            phoxal_cli_core::runtime::RobotKey::new("dev", "rover"),
            "drive",
        );
        board.register_planned(
            &key,
            ParticipantKind::Service,
            phoxal_cli_core::runtime::StartupRequirement::Required,
        );
        board.set_producer(&key, producer(7));

        apply_liveliness_event(
            &board,
            "dev",
            "rover",
            event("drive", LivelinessStatus::Alive),
        );
        assert_eq!(
            board.supervisor_snapshot().processes[&key].status.actual,
            ProcessState::Ready
        );

        apply_liveliness_event(
            &board,
            "dev",
            "rover",
            event("drive", LivelinessStatus::Lost),
        );
        assert_eq!(
            board.supervisor_snapshot().processes[&key].status.actual,
            ProcessState::Ready,
            "Lost is observational and must not mutate process lifecycle"
        );
    }

    #[test]
    fn observer_filters_its_own_participant_id() {
        // Synthetic guard coverage: the observer currently holds no
        // Liveliness token, but its reserved id must never become a board row.
        let board = SupervisorState::new();
        let key = phoxal_cli_core::runtime::ProcessKey::robot(
            phoxal_cli_core::runtime::RobotKey::new("dev", "rover"),
            SUPERVISOR_SESSION_ID,
        );
        board.register_planned(
            &key,
            ParticipantKind::Host,
            phoxal_cli_core::runtime::StartupRequirement::Optional,
        );
        apply_liveliness_event(
            &board,
            "dev",
            "rover",
            event(SUPERVISOR_SESSION_ID, LivelinessStatus::Alive),
        );
        assert_eq!(
            board.supervisor_snapshot().processes[&key].status.actual,
            ProcessState::Starting
        );
    }
}
