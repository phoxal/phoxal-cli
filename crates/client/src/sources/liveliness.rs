use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use phoxal_bus::{Bus, ParticipantLivelinessEvent, ParticipantLivelinessStatus};
use phoxal_cli_core::runtime::RobotKey;
use phoxal_cli_observation::AttachmentEvent;
use tokio::sync::{Notify, RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use crate::attachment::SourceRetry;
use crate::attachment::task_group::TaskGroup;
use crate::sources::TelemetryBackend;
use crate::state::processes::ProcessStore;

pub(crate) struct LivelinessContext {
    pub(crate) processes: Arc<RwLock<ProcessStore>>,
    pub(crate) events: mpsc::Sender<AttachmentEvent>,
    pub(crate) telemetry: TelemetryBackend,
    pub(crate) reopen: mpsc::Sender<()>,
}

pub(crate) fn spawn(
    tasks: &mut TaskGroup,
    epoch: phoxal_cli_observation::AttachmentEpoch,
    robot: RobotKey,
    bus: Bus,
    context: LivelinessContext,
    cancellation: CancellationToken,
) {
    let LivelinessContext {
        processes,
        events,
        telemetry,
        reopen,
    } = context;
    tasks.spawn(async move {
        let pending = Arc::new(Mutex::new(BTreeMap::<String, bool>::new()));
        let wake = Arc::new(Notify::new());
        let source = format!("liveliness:{}/{}", robot.namespace, robot.robot_id);
        let mut retry = SourceRetry::new();
        let _observer = loop {
            telemetry.record_health(&source, phoxal_cli_observation::SourceStatus::Connecting);
            let callback_pending = pending.clone();
            let callback_wake = wake.clone();
            let observed = tokio::select! {
                _ = cancellation.cancelled() => return,
                observed = bus.observe_participant_liveliness(
                    move |event: ParticipantLivelinessEvent| {
                        if event.key.participant() == "phoxal-cli-attachment" {
                            return;
                        }
                        callback_pending
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .insert(
                                event.key.participant().to_string(),
                                event.status == ParticipantLivelinessStatus::Alive,
                            );
                        callback_wake.notify_one();
                    }
                ) => observed,
            };
            match observed {
                Ok(observer) => break observer,
                Err(error) => {
                    tracing::debug!(error = %error, "attachment liveliness source stopped");
                    telemetry.record_health(&source, phoxal_cli_observation::SourceStatus::Failed);
                    if !retry.after_failure(&cancellation, &reopen).await {
                        return;
                    }
                }
            }
        };
        telemetry.record_health(&source, phoxal_cli_observation::SourceStatus::Live);
        loop {
            let notified = wake.notified();
            let updates = {
                let mut pending = pending.lock().unwrap_or_else(PoisonError::into_inner);
                std::mem::take(&mut *pending)
            };
            if !updates.is_empty() {
                telemetry.record_health(&source, phoxal_cli_observation::SourceStatus::Live);
                let mut changed = None;
                {
                    let mut processes = processes.write().await;
                    for (participant, present) in updates {
                        changed = processes
                            .record_presence(&robot, &participant, present)
                            .or(changed);
                    }
                }
                if let Some(table) = changed {
                    let sent = tokio::select! {
                        _ = cancellation.cancelled() => return,
                        sent = events.send(AttachmentEvent::ProcessesChanged {
                            epoch,
                            values: Arc::new(table),
                        }) => sent,
                    };
                    if sent.is_err() {
                        return;
                    }
                }
                continue;
            }
            tokio::select! {
                _ = cancellation.cancelled() => return,
                () = notified => {}
            }
        }
    });
}
