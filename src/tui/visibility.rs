//! One visibility policy shared by Overview, Runtimes, Logs, and Bus.

use crate::participant_kind::ParticipantKind;
use crate::stores::runtime_store::{RuntimeOwnership, RuntimeStore};
use crate::supervisor::{BoardSnapshot, ParticipantStatus};

#[must_use]
pub fn is_visible_runtime(status: &ParticipantStatus, runtime: &RuntimeStore) -> bool {
    if !matches!(
        status.kind,
        ParticipantKind::Service | ParticipantKind::Driver
    ) {
        return false;
    }
    runtime.metadata(&status.id).map_or_else(
        || !is_session_internal_runtime_id(&status.id),
        |metadata| metadata.ownership != RuntimeOwnership::SimulationManaged,
    )
}

fn is_session_internal_runtime_id(id: &str) -> bool {
    id == "supervisor"
        || id == "webots"
        || id.starts_with("tool-")
        || id.starts_with("simulator-")
        || id.starts_with("webots-")
}

#[must_use]
pub fn is_internal_id(id: &str, board: &BoardSnapshot, runtime: &RuntimeStore) -> bool {
    if let Some(status) = board.participants.get(id) {
        return !is_visible_runtime(status, runtime);
    }
    if id == "phoxal-cli"
        || id.starts_with("tool-")
        || id.starts_with("webots")
        || id.contains("supervisor")
        || id.contains("controller")
        || id.contains("simulator")
    {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::participant_kind::ParticipantKind;
    use crate::supervisor::{ParticipantState, ParticipantStatus};

    #[test]
    fn run_and_webots_share_robot_runtime_visibility() {
        let mut runtime = RuntimeStore::new();
        let service =
            ParticipantStatus::new("motion", ParticipantKind::Service, ParticipantState::Ready);
        let driver =
            ParticipantStatus::new("wheels", ParticipantKind::Driver, ParticipantState::Ready);
        let tool = ParticipantStatus::new(
            "tool-router",
            ParticipantKind::Tool,
            ParticipantState::Ready,
        );
        let simulator = ParticipantStatus::new(
            "webots-controller",
            ParticipantKind::Simulator,
            ParticipantState::Ready,
        );
        assert!(is_visible_runtime(&service, &runtime));
        assert!(is_visible_runtime(&driver, &runtime));
        assert!(!is_visible_runtime(&tool, &runtime));
        assert!(!is_visible_runtime(&simulator, &runtime));
        runtime.set_test_ownership("wheels", RuntimeOwnership::SimulationManaged);
        assert!(
            !is_visible_runtime(&driver, &runtime),
            "a Webots-substituted physical driver is not an executing runtime"
        );
    }

    #[test]
    fn a_robot_service_named_controller_is_not_hidden_by_name() {
        let runtime = RuntimeStore::new();
        let status = ParticipantStatus::new(
            "flight-controller",
            ParticipantKind::Service,
            ParticipantState::Ready,
        );
        let board = BoardSnapshot {
            participants: [(status.id.clone(), status)].into(),
        };
        assert!(!is_internal_id("flight-controller", &board, &runtime));
    }

    #[test]
    fn synthetic_session_rows_are_hidden_without_runtime_metadata() {
        let runtime = RuntimeStore::new();
        for id in ["supervisor", "webots"] {
            let status =
                ParticipantStatus::new(id, ParticipantKind::Service, ParticipantState::Ready);
            assert!(
                !is_visible_runtime(&status, &runtime),
                "{id} is session infrastructure, not a robot runtime"
            );
        }
    }
}
