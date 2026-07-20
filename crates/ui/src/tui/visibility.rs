//! One terminal visibility policy shared by Overview, Runtimes, Logs, and Bus.

use phoxal_cli_core::session::ParticipantKind;
use phoxal_cli_core::session::stores::runtime::{RuntimeOwnership, RuntimeStore};
use phoxal_cli_core::session::{BoardSnapshot, ParticipantStatus};

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
    is_known_internal_id(id)
}

#[must_use]
pub fn is_internal_id(id: &str, board: &BoardSnapshot, runtime: &RuntimeStore) -> bool {
    if let Some(status) = board.participants.get(id) {
        return !is_visible_runtime(status, runtime);
    }
    is_known_internal_id(id)
}

fn is_known_internal_id(id: &str) -> bool {
    id.eq_ignore_ascii_case("phoxal-cli")
        || starts_with_ignore_ascii_case(id, "phoxal-cli/")
        || starts_with_ignore_ascii_case(id, "tool-")
        || id.eq_ignore_ascii_case("supervisor")
        || id.eq_ignore_ascii_case("webots")
        || starts_with_ignore_ascii_case(id, "webots-")
        || starts_with_ignore_ascii_case(id, "simulator-")
}

fn starts_with_ignore_ascii_case(value: &str, prefix: &str) -> bool {
    value
        .as_bytes()
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_cli_core::session::ParticipantKind;
    use phoxal_cli_core::session::{ParticipantState, ParticipantStatus};

    #[test]
    fn run_and_webots_share_robot_runtime_visibility() {
        let mut runtime = RuntimeStore::new();
        let service =
            ParticipantStatus::new("motion", ParticipantKind::Service, ParticipantState::Ready);
        let driver =
            ParticipantStatus::new("wheels", ParticipantKind::Driver, ParticipantState::Ready);
        let tool = ParticipantStatus::new(
            "infrastructure-router",
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
        assert!(
            !is_internal_id("arm-controller", &BoardSnapshot::default(), &runtime),
            "unknown robot-style names stay visible until metadata arrives"
        );
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

    #[test]
    fn cli_diagnostic_sources_are_internal_case_insensitively() {
        let board = BoardSnapshot::default();
        let runtime = RuntimeStore::new();
        for id in ["phoxal-cli", "phoxal-cli/Cli", "phoxal-cli/Supervisor"] {
            assert!(is_internal_id(id, &board, &runtime), "{id}");
        }

        let cli = ParticipantStatus::new(
            "phoxal-cli",
            ParticipantKind::Service,
            ParticipantState::Ready,
        );
        let board = BoardSnapshot {
            participants: [(cli.id.clone(), cli)].into(),
        };
        assert!(is_internal_id("phoxal-cli", &board, &runtime));
    }
}
