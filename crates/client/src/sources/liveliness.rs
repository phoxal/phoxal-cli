use std::sync::Arc;

use phoxal::raw::{Bus, ParticipantLivelinessEvent, ParticipantLivelinessStatus};
use phoxal_cli_core::runtime::RobotKey;
use phoxal_cli_observation::AttachmentEvent;
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use crate::attachment::task_group::TaskGroup;
use crate::sources::TelemetryBackend;
use crate::state::processes::ProcessStore;

pub(crate) fn spawn(
    tasks: &mut TaskGroup,
    robot: RobotKey,
    bus: Bus,
    processes: Arc<RwLock<ProcessStore>>,
    events: mpsc::Sender<AttachmentEvent>,
    telemetry: TelemetryBackend,
    cancellation: CancellationToken,
) {
    tasks.spawn(async move {
        let (liveliness_tx, mut liveliness_rx) = mpsc::unbounded_channel();
        let observer = bus
            .observe_participant_liveliness(move |event: ParticipantLivelinessEvent| {
                if event.key.participant() == "phoxal-cli-attachment" {
                    return;
                }
                let _ = liveliness_tx.send((
                    event.key.participant().to_string(),
                    event.status == ParticipantLivelinessStatus::Alive,
                ));
            })
            .await;
        let Ok(_observer) = observer else {
            telemetry.record_health(
                format!("liveliness:{}/{}", robot.namespace, robot.robot_id),
                phoxal_cli_observation::SourceStatus::Failed,
            );
            return;
        };
        telemetry.record_health(
            format!("liveliness:{}/{}", robot.namespace, robot.robot_id),
            phoxal_cli_observation::SourceStatus::Live,
        );
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => return,
                update = liveliness_rx.recv() => {
                    let Some((participant, present)) = update else {
                        return;
                    };
                    telemetry.record_health(
                        format!("liveliness:{}/{}", robot.namespace, robot.robot_id),
                        phoxal_cli_observation::SourceStatus::Live,
                    );
                    let table = processes.write().await.record_presence(
                        &robot,
                        &participant,
                        present,
                    );
                    if let Some(table) = table {
                        let _ = events
                            .send(AttachmentEvent::ProcessesChanged(Arc::new(table)))
                            .await;
                    }
                }
            }
        }
    });
}
