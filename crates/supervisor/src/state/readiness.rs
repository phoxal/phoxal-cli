//! Exact participant readiness observed by the resident authority.

use crate::SupervisorState;
use anyhow::{Result, anyhow};
use phoxal_bus::{Bus, BusConfig, ParticipantLivelinessEvent, ParticipantLivelinessStatus};
use phoxal_cli_core::identity::{ExecutionId, ProducerId};
use phoxal_cli_core::project::launch_plan::DEFAULT_ROUTER_CONNECT;
use phoxal_cli_core::runtime::{ParticipantInstanceKey, RobotKey};
use phoxal_model::AssetResolver;
use std::time::Duration;
use tokio::task::JoinHandle;

/// The supervisor's own participant id on the robot bus. It observes
/// Liveliness and answers the contracts the supervisor owns; it declares no
/// Liveliness token of its own and is not a graph participant.
const SUPERVISOR_SESSION_ID: &str = "phoxal-cli-liveliness-observer";

#[cfg(test)]
use crate::ProcessState;

/// Open the supervisor's own session on one robot bus.
///
/// It carries everything the supervisor itself does on that bus: observing
/// every planned participant's stable Zenoh Liveliness key, and answering the
/// contracts the supervisor owns (`supervisor/asset/get`). One session per
/// robot target rather than one per concern - each absorbed contract otherwise
/// adds another connection to the same router for no benefit
/// (organization#978).
///
/// Callers register the finite participant set on the board before starting
/// this; Liveliness traffic for any other key is deliberately ignored. History
/// is enabled by the framework wrapper, so participants that completed setup
/// before this session connected are discovered immediately.
pub fn start_supervisor_session(
    namespace: String,
    robot_id: String,
    connect: String,
    execution: ExecutionId,
    board: SupervisorState,
    assets: AssetResolver,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match supervisor_session_loop(
                namespace.clone(),
                robot_id.clone(),
                connect.clone(),
                execution,
                board.clone(),
                assets.clone(),
            )
            .await
            {
                Ok(()) => break,
                Err(error) => {
                    tracing::debug!("supervisor session waiting for router: {error:#}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    })
}

pub(crate) async fn supervisor_session_loop(
    namespace: String,
    robot_id: String,
    connect: String,
    execution: ExecutionId,
    board: SupervisorState,
    assets: AssetResolver,
) -> Result<()> {
    let bus = Bus::open(BusConfig {
        namespace: namespace.clone(),
        robot_id: robot_id.clone(),
        participant: SUPERVISOR_SESSION_ID.to_string(),
        execution,
        producer: ProducerId::mint(),
        connect_endpoints: vec![connect],
    })
    .await
    .map_err(|error| anyhow!("failed to open bus Liveliness observer: {error}"))?;
    let _observer = bus
        .observe_participant_liveliness(move |event| {
            apply_liveliness_event(&board, &namespace, &robot_id, event);
        })
        .await
        .map_err(|error| anyhow!("failed to observe participant Liveliness: {error}"))?;
    // Once declared, the Bus session and Zenoh subscriber own transparent
    // transport reconnection. The outer loop above retries only initial open
    // or declaration failures; there is no application-level heartbeat loop.
    //
    // Serving assets is what parks this task: it returns only when the bus
    // closes, which is also the one condition that used to end the wait here.
    super::assets::serve_assets(&bus, &assets).await
}

fn apply_liveliness_event(
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
            producer: event.key.producer(),
        },
        event.status == ParticipantLivelinessStatus::Alive,
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
        ProducerId::parse(&format!("{:032x}", u128::from(seed)))
            .expect("test producer id must parse")
    }
    use super::*;
    use phoxal_bus::ParticipantLivelinessKey;
    use phoxal_cli_core::runtime::ParticipantKind;

    fn event(participant: &str, status: ParticipantLivelinessStatus) -> ParticipantLivelinessEvent {
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
            event("drive", ParticipantLivelinessStatus::Alive),
        );
        assert_eq!(
            board.supervisor_snapshot().processes[&key].status.actual,
            ProcessState::Ready
        );

        apply_liveliness_event(
            &board,
            "dev",
            "rover",
            event("drive", ParticipantLivelinessStatus::Lost),
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
            ParticipantKind::Tool,
            phoxal_cli_core::runtime::StartupRequirement::Optional,
        );
        apply_liveliness_event(
            &board,
            "dev",
            "rover",
            event(SUPERVISOR_SESSION_ID, ParticipantLivelinessStatus::Alive),
        );
        assert_eq!(
            board.supervisor_snapshot().processes[&key].status.actual,
            ProcessState::Starting
        );
    }
}
