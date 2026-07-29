//! Exact participant readiness observed by the resident authority.

use crate::SupervisorState;
use anyhow::{Result, anyhow};
use phoxal::raw::{Bus, BusConfig, ParticipantLivelinessEvent, ParticipantLivelinessStatus};
use phoxal_cli_core::identity::{ExecutionId, ProducerId};
use phoxal_cli_core::project::launch_plan::DEFAULT_ROUTER_CONNECT;
use phoxal_cli_core::session::{ParticipantInstanceKey, RobotKey};
use std::time::Duration;
use tokio::task::JoinHandle;

const LIVELINESS_OBSERVER_ID: &str = "phoxal-cli-liveliness-observer";

#[cfg(test)]
use crate::ProcessState;

/// Observe every planned participant's stable Zenoh Liveliness key on one
/// robot bus. Callers register the finite participant set on the board before
/// starting this observer; traffic for any other key is deliberately ignored.
/// History is enabled by the framework wrapper, so participants that completed
/// setup before this observer connected are discovered immediately.
pub fn start_liveliness_observer(
    namespace: String,
    robot_id: String,
    connect: String,
    execution: ExecutionId,
    board: SupervisorState,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match liveliness_observer_loop(
                namespace.clone(),
                robot_id.clone(),
                connect.clone(),
                execution,
                board.clone(),
            )
            .await
            {
                Ok(()) => break,
                Err(error) => {
                    tracing::debug!("liveliness observer waiting for router: {error:#}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    })
}

pub(crate) async fn liveliness_observer_loop(
    namespace: String,
    robot_id: String,
    connect: String,
    execution: ExecutionId,
    board: SupervisorState,
) -> Result<()> {
    let bus = Bus::open(BusConfig {
        namespace: namespace.clone(),
        robot_id: robot_id.clone(),
        participant: LIVELINESS_OBSERVER_ID.to_string(),
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
    std::future::pending::<()>().await;
    Ok(())
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
    if id == LIVELINESS_OBSERVER_ID {
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
    use phoxal::raw::ParticipantLivelinessKey;
    use phoxal_cli_core::session::ParticipantKind;

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
        let key = phoxal_cli_core::session::ProcessKey::robot(
            phoxal_cli_core::session::RobotKey::new("dev", "rover"),
            "drive",
        );
        board.register_planned(
            &key,
            ParticipantKind::Service,
            phoxal_cli_core::session::StartupRequirement::Required,
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
        let key = phoxal_cli_core::session::ProcessKey::robot(
            phoxal_cli_core::session::RobotKey::new("dev", "rover"),
            LIVELINESS_OBSERVER_ID,
        );
        board.register_planned(
            &key,
            ParticipantKind::Tool,
            phoxal_cli_core::session::StartupRequirement::Optional,
        );
        apply_liveliness_event(
            &board,
            "dev",
            "rover",
            event(LIVELINESS_OBSERVER_ID, ParticipantLivelinessStatus::Alive),
        );
        assert_eq!(
            board.supervisor_snapshot().processes[&key].status.actual,
            ProcessState::Starting
        );
    }
}
