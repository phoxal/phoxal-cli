use std::sync::Arc;

use phoxal_cli_core::runtime::{ProcessScope, RobotKey};
use phoxal_cli_observation::{
    AttachmentEpoch, AttachmentEvent, MotionObservation, RuntimeRow, SourceHealth, SourceStatus,
    StoreChanged,
};
use phoxal_cli_protocol::SupervisorSnapshotV0;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::task_group::TaskGroup;
use super::transport_set::GraphTransportSet;
use crate::ports::input::InputCommand;
use crate::reconcile::RetryBackoff;
use crate::sources::{TelemetryBackend, TelemetryMailbox, TelemetryUpdate};
use crate::state::Stores;

pub(crate) struct SourceGroup {
    cancellation: CancellationToken,
    tasks: TaskGroup,
    transports: GraphTransportSet,
    input: Arc<tokio::sync::Mutex<Option<mpsc::Receiver<InputCommand>>>>,
    _reopen_guard: mpsc::Sender<()>,
    reopen_rx: mpsc::Receiver<()>,
}

impl SourceGroup {
    pub fn start(
        epoch: AttachmentEpoch,
        snapshot: &SupervisorSnapshotV0,
        transports: GraphTransportSet,
        stores: Stores,
        events: mpsc::Sender<AttachmentEvent>,
        input_rx: mpsc::Receiver<InputCommand>,
        freshness: super::freshness::Scheduler,
    ) -> Self {
        let cancellation = CancellationToken::new();
        let mut tasks = TaskGroup::new();
        let (reopen_tx, reopen_rx) = mpsc::channel(1);
        let input = Arc::new(tokio::sync::Mutex::new(Some(input_rx)));
        let telemetry_mailbox = Arc::new(TelemetryMailbox::default());
        let telemetry = TelemetryBackend::with_updates(telemetry_mailbox.clone());
        let mut initial_health = SourceHealth::default();
        for (robot, _) in transports.iter() {
            for source in ["liveliness", "logs", "runtimes"] {
                initial_health.sources.insert(
                    format!("{source}:{}/{}", robot.namespace, robot.robot_id),
                    SourceStatus::Connecting,
                );
            }
        }
        initial_health
            .sources
            .insert("input".to_string(), SourceStatus::Connecting);
        initial_health
            .sources
            .insert("motion".to_string(), SourceStatus::Connecting);
        let health = Arc::new(tokio::sync::RwLock::new(initial_health.clone()));
        let _ = events.try_send(AttachmentEvent::SourceHealthChanged {
            epoch,
            values: Arc::new(initial_health),
        });
        let update_stores = stores.clone();
        let update_events = events.clone();
        let update_health = health.clone();
        let update_cancel = cancellation.clone();
        let update_freshness = freshness.clone();
        tasks.spawn(async move {
            loop {
                tokio::select! {
                    _ = update_cancel.cancelled() => return,
                    batch = telemetry_mailbox.recv() => {
                        if batch.dropped != update_health.read().await.ingress_dropped {
                            let changed = {
                                let mut health = update_health.write().await;
                                health.ingress_dropped = batch.dropped;
                                health.clone()
                            };
                            let _ = update_events
                                .send(AttachmentEvent::SourceHealthChanged {
                                    epoch,
                                    values: Arc::new(changed),
                                })
                                .await;
                        }
                        for update in batch.updates {
                            apply_telemetry_update(
                                epoch,
                                &update_stores,
                                &update_events,
                                &update_health,
                                &update_freshness,
                                update,
                            ).await;
                        }
                    }
                }
            }
        });
        let mut first_bus = None;
        for (robot, bus) in transports.iter() {
            first_bus.get_or_insert_with(|| bus.clone());
            crate::sources::liveliness::spawn(
                &mut tasks,
                epoch,
                robot.clone(),
                bus.clone(),
                crate::sources::liveliness::LivelinessContext {
                    processes: stores.processes.clone(),
                    events: events.clone(),
                    telemetry: telemetry.clone(),
                    reopen: reopen_tx.clone(),
                },
                cancellation.clone(),
            );
            let log_bus = bus.clone();
            let log_scope = phoxal_cli_observation::LogScope {
                namespace: robot.namespace.clone(),
                robot_id: robot.robot_id.clone(),
            };
            let log_store = stores.logs.clone();
            let log_events = events.clone();
            let log_telemetry = telemetry.clone();
            let log_health_key = format!("logs:{}/{}", log_scope.namespace, log_scope.robot_id);
            let log_cancel = cancellation.clone();
            let log_reopen = reopen_tx.clone();
            tasks.spawn(async move {
                let mut retry = SourceRetry::new();
                loop {
                    log_telemetry.record_health(&log_health_key, SourceStatus::Connecting);
                    tokio::select! {
                        _ = log_cancel.cancelled() => break,
                        result = crate::sources::logs::run(
                            log_bus.clone(),
                            log_scope.clone(),
                            epoch,
                            log_store.clone(),
                            log_events.clone(),
                            log_telemetry.clone(),
                        ) => {
                            let error = result.expect_err("log source is intentionally endless");
                            tracing::debug!(error = %error, "attachment log source stopped");
                            log_telemetry.record_health(&log_health_key, SourceStatus::Failed);
                        }
                    }
                    if !retry.after_failure(&log_cancel, &log_reopen).await {
                        break;
                    }
                }
            });
            let scope = phoxal_cli_observation::RobotScope {
                namespace: robot.namespace.clone(),
                robot_id: robot.robot_id.clone(),
            };
            spawn_runtimes(
                &mut tasks,
                bus.clone(),
                scope,
                participants_for(snapshot, robot),
                telemetry.clone(),
                cancellation.clone(),
                reopen_tx.clone(),
            );
        }
        if let Some(bus) = first_bus {
            spawn_motion(
                &mut tasks,
                bus.clone(),
                telemetry.clone(),
                cancellation.clone(),
                reopen_tx.clone(),
            );
            let input_cancel = cancellation.clone();
            let input_telemetry = telemetry.clone();
            let input_reopen = reopen_tx.clone();
            let input = input.clone();
            let manual_input = snapshot.manual_input.clone();
            tasks.spawn(async move {
                    let mut input = input.lock().await;
                    let input_rx = input.as_mut().expect("source group owns its input port");
                    let mut retry = SourceRetry::new();
                    loop {
                        input_telemetry.record_health("input", SourceStatus::Connecting);
                        tokio::select! {
                            _ = input_cancel.cancelled() => break,
                            result = crate::sources::input::run(
                                bus.clone(),
                                manual_input.clone(),
                                telemetry.clone(),
                                input_rx,
                            ) => {
                                match result {
                                    Ok(()) => break,
                                    Err(error) => {
                                        tracing::debug!(error = %error, "attachment input source stopped");
                                        input_telemetry.record_health("input", SourceStatus::Failed);
                                    }
                                }
                            }
                        }
                        if !retry.after_failure(&input_cancel, &input_reopen).await {
                            break;
                        }
                    }
                });
        }
        Self {
            cancellation,
            tasks,
            transports,
            input,
            _reopen_guard: reopen_tx,
            reopen_rx,
        }
    }

    pub async fn reopen_requested(&mut self) {
        self.reopen_rx
            .recv()
            .await
            .expect("source group retains the reopen sender");
    }

    pub async fn shutdown(self) -> mpsc::Receiver<InputCommand> {
        self.cancellation.cancel();
        self.tasks.join().await;
        self.transports.close().await;
        self.input
            .lock()
            .await
            .take()
            .expect("source group owns its input receiver until shutdown")
    }
}

fn participants_for(snapshot: &SupervisorSnapshotV0, robot: &RobotKey) -> Vec<String> {
    snapshot
        .processes
        .keys()
        .filter_map(|key| match &key.scope {
            ProcessScope::Robot(candidate) if candidate == robot => Some(key.id.clone()),
            _ => None,
        })
        .collect()
}

fn spawn_runtimes(
    tasks: &mut TaskGroup,
    bus: phoxal_bus::Bus,
    scope: phoxal_cli_observation::RobotScope,
    participants: Vec<String>,
    telemetry: TelemetryBackend,
    cancellation: CancellationToken,
    reopen: mpsc::Sender<()>,
) {
    let source = format!("runtimes:{}/{}", scope.namespace, scope.robot_id);
    let failed = telemetry.clone();
    tasks.spawn(async move {
        let mut retry = SourceRetry::new();
        loop {
            failed.record_health(&source, SourceStatus::Connecting);
            tokio::select! {
                _ = cancellation.cancelled() => break,
                result = crate::sources::runtimes::run(
                    bus.clone(),
                    scope.clone(),
                    participants.clone(),
                    telemetry.clone(),
                ) => {
                    let error = result.expect_err("runtime source is intentionally endless");
                    tracing::debug!(error = %error, "attachment runtime source stopped");
                    failed.record_health(&source, SourceStatus::Failed);
                }
            }
            if !retry.after_failure(&cancellation, &reopen).await {
                break;
            }
        }
    });
}

fn spawn_motion(
    tasks: &mut TaskGroup,
    bus: phoxal_bus::Bus,
    telemetry: TelemetryBackend,
    cancellation: CancellationToken,
    reopen: mpsc::Sender<()>,
) {
    let failed = telemetry.clone();
    tasks.spawn(async move {
        let mut retry = SourceRetry::new();
        loop {
            failed.record_health("motion", SourceStatus::Connecting);
            tokio::select! {
                _ = cancellation.cancelled() => break,
                result = crate::sources::motion::run(bus.clone(), telemetry.clone()) => {
                    match result {
                        Ok(()) => break,
                        Err(error) => {
                            tracing::debug!(error = %error, "attachment motion source stopped");
                            failed.record_health("motion", SourceStatus::Failed);
                        }
                    }
                }
            }
            if !retry.after_failure(&cancellation, &reopen).await {
                break;
            }
        }
    });
}

const SOURCE_FAILURES_BEFORE_REOPEN: u8 = 6;

pub(crate) struct SourceRetry {
    backoff: RetryBackoff,
    failures: u8,
}

impl SourceRetry {
    pub(crate) fn new() -> Self {
        Self::with_backoff(
            std::time::Duration::from_millis(100),
            std::time::Duration::from_secs(2),
        )
    }

    fn with_backoff(initial: std::time::Duration, maximum: std::time::Duration) -> Self {
        Self {
            backoff: RetryBackoff::new(initial, maximum),
            failures: 0,
        }
    }

    pub(crate) async fn after_failure(
        &mut self,
        cancellation: &CancellationToken,
        reopen: &mpsc::Sender<()>,
    ) -> bool {
        self.failures = self.failures.saturating_add(1);
        let retry = tokio::select! {
            _ = cancellation.cancelled() => return false,
            _ = tokio::time::sleep(self.backoff.next_delay()) => {
                self.failures < SOURCE_FAILURES_BEFORE_REOPEN
            }
        };
        if !retry {
            let _ = reopen.try_send(());
        }
        retry
    }
}

async fn apply_telemetry_update(
    epoch: AttachmentEpoch,
    stores: &Stores,
    events: &mpsc::Sender<AttachmentEvent>,
    health: &Arc<tokio::sync::RwLock<SourceHealth>>,
    freshness: &super::freshness::Scheduler,
    update: TelemetryUpdate,
) {
    let (source, status) = match &update {
        TelemetryUpdate::Runtimes(scope, _, _) | TelemetryUpdate::Runtime(scope, _) => (
            format!("runtimes:{}/{}", scope.namespace, scope.robot_id),
            SourceStatus::Live,
        ),
        TelemetryUpdate::Joypads(_) => ("input".to_string(), SourceStatus::Live),
        TelemetryUpdate::Motion(_) => ("motion".to_string(), SourceStatus::Live),
        TelemetryUpdate::Health(source, status) => (source.clone(), *status),
    };
    if status == SourceStatus::Live {
        freshness.refresh(
            epoch,
            source.clone(),
            crate::attachment::DEFAULT_FRESHNESS_TTL,
        );
    }
    let changed_health = {
        let mut health = health.write().await;
        let changed = health.sources.insert(source, status) != Some(status);
        changed.then(|| health.clone())
    };
    if let Some(health) = changed_health {
        let _ = events
            .send(AttachmentEvent::SourceHealthChanged {
                epoch,
                values: Arc::new(health),
            })
            .await;
    }
    match update {
        TelemetryUpdate::Runtimes(scope, samples, status) => {
            let rows = samples.into_iter().map(|sample| RuntimeRow {
                scope: scope.clone(),
                sample,
                capacity_evictions: status.capacity_evictions,
            });
            let revision = stores.runtimes.write().await.install(epoch, &scope, rows);
            if let Some(revision) = revision {
                let _ = events
                    .send(AttachmentEvent::RuntimesChanged(StoreChanged {
                        epoch,
                        revision,
                    }))
                    .await;
            }
        }
        TelemetryUpdate::Runtime(scope, sample) => {
            let revision = stores.runtimes.write().await.record(
                epoch,
                RuntimeRow {
                    scope,
                    sample,
                    capacity_evictions: 0,
                },
            );
            if let Some(revision) = revision {
                let _ = events
                    .send(AttachmentEvent::RuntimesChanged(StoreChanged {
                        epoch,
                        revision,
                    }))
                    .await;
            }
        }
        TelemetryUpdate::Joypads(joypads) => {
            let motion = stores.motion.read().await.current();
            let observation = stores.input.write().await.record_joypads(joypads, motion);
            let _ = events
                .send(AttachmentEvent::InputChanged {
                    epoch,
                    values: Arc::new(observation),
                })
                .await;
        }
        TelemetryUpdate::Motion(motion) => {
            let motion = stores.motion.write().await.record(MotionObservation {
                linear_x_mps: motion.final_target.linear_x_mps,
                angular_z_radps: motion.final_target.angular_z_radps,
            });
            let observation = stores.input.read().await.observe(motion);
            let _ = events
                .send(AttachmentEvent::InputChanged {
                    epoch,
                    values: Arc::new(observation),
                })
                .await;
        }
        TelemetryUpdate::Health(_, _) => {}
    }
}

#[cfg(test)]
mod tests {

    use std::time::Duration;

    use super::SourceRetry;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn sustained_source_failures_emit_one_reopen_after_local_retries() {
        let cancellation = CancellationToken::new();
        let (reopen_tx, mut reopen_rx) = mpsc::channel(1);
        let mut retry =
            SourceRetry::with_backoff(Duration::from_millis(1), Duration::from_millis(1));

        for _ in 0..5 {
            assert!(retry.after_failure(&cancellation, &reopen_tx).await);
            assert!(reopen_rx.try_recv().is_err());
        }
        assert!(!retry.after_failure(&cancellation, &reopen_tx).await);
        assert_eq!(reopen_rx.recv().await, Some(()));
        assert!(reopen_rx.try_recv().is_err());
    }
}
