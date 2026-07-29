use std::sync::Arc;

use phoxal_cli_core::session::{ProcessScope, RobotKey};
use phoxal_cli_observation::{
    AttachmentEpoch, AttachmentEvent, BusRow, MotionObservation, RuntimeRow, SourceHealth,
    SourceStatus, StoreChanged,
};
use phoxal_cli_protocol::SupervisorSnapshotV0;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::task_group::TaskGroup;
use super::transport_set::GraphTransportSet;
use crate::ports::input::InputCommand;
use crate::sources::{TelemetryBackend, TelemetryUpdate};
use crate::state::Stores;

pub(crate) struct SourceGroup {
    cancellation: CancellationToken,
    tasks: TaskGroup,
    transports: GraphTransportSet,
    input_return: tokio::sync::oneshot::Receiver<mpsc::Receiver<InputCommand>>,
}

impl SourceGroup {
    pub fn start(
        epoch: AttachmentEpoch,
        snapshot: &SupervisorSnapshotV0,
        transports: GraphTransportSet,
        stores: Stores,
        events: mpsc::Sender<AttachmentEvent>,
        input_rx: mpsc::Receiver<InputCommand>,
        freshness: super::freshness::DeadlineSender,
    ) -> Self {
        let cancellation = CancellationToken::new();
        let mut tasks = TaskGroup::new();
        let (telemetry_tx, mut telemetry_rx) = mpsc::unbounded_channel();
        let telemetry = TelemetryBackend::with_updates(telemetry_tx.clone());
        let mut initial_health = SourceHealth::default();
        for (robot, _) in transports.iter() {
            for source in ["liveliness", "logs", "device", "bus", "runtimes"] {
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
        initial_health
            .sources
            .insert("clock".to_string(), SourceStatus::Connecting);
        let health = Arc::new(tokio::sync::RwLock::new(initial_health.clone()));
        let _ = events.try_send(AttachmentEvent::SourceHealthChanged(Arc::new(
            initial_health,
        )));
        let update_stores = stores.clone();
        let update_events = events.clone();
        let update_health = health.clone();
        let update_cancel = cancellation.clone();
        let update_freshness = freshness.clone();
        tasks.spawn(async move {
            loop {
                tokio::select! {
                    _ = update_cancel.cancelled() => return,
                    update = telemetry_rx.recv() => {
                        let Some(update) = update else { return; };
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
        });
        let mut first_bus = None;
        for (robot, bus) in transports.iter() {
            first_bus.get_or_insert_with(|| bus.clone());
            crate::sources::liveliness::spawn(
                &mut tasks,
                robot.clone(),
                bus.clone(),
                stores.processes.clone(),
                events.clone(),
                telemetry.clone(),
                cancellation.clone(),
            );
            let log_bus = bus.clone();
            let log_scope = phoxal_cli_core::session::LogScope {
                namespace: robot.namespace.clone(),
                robot_id: robot.robot_id.clone(),
            };
            let log_store = stores.logs.clone();
            let log_events = events.clone();
            let log_telemetry = telemetry.clone();
            let log_health_key = format!("logs:{}/{}", log_scope.namespace, log_scope.robot_id);
            let log_cancel = cancellation.clone();
            tasks.spawn(async move {
                tokio::select! {
                    _ = log_cancel.cancelled() => {}
                    result = crate::sources::logs::run(
                        log_bus,
                        log_scope,
                        epoch,
                        log_store,
                        log_events,
                        log_telemetry.clone(),
                    ) => {
                        let error = result.expect_err("log source is intentionally endless");
                        tracing::debug!(error = %error, "attachment log source stopped");
                        log_telemetry.record_health(log_health_key, SourceStatus::Failed);
                    }
                }
            });
            let scope = phoxal_cli_core::session::RobotScope {
                namespace: robot.namespace.clone(),
                robot_id: robot.robot_id.clone(),
            };
            spawn_device(
                &mut tasks,
                bus.clone(),
                scope.clone(),
                telemetry.clone(),
                cancellation.clone(),
            );
            spawn_bus(
                &mut tasks,
                bus.clone(),
                scope.clone(),
                telemetry.clone(),
                cancellation.clone(),
            );
            spawn_runtimes(
                &mut tasks,
                bus.clone(),
                scope,
                participants_for(snapshot, robot),
                telemetry.clone(),
                cancellation.clone(),
            );
        }
        let (return_tx, input_return) = tokio::sync::oneshot::channel();
        match first_bus {
            Some(bus) => {
                let clock_bus = bus.clone();
                let clock_updates = telemetry_tx;
                let clock_telemetry = telemetry.clone();
                let clock_cancel = cancellation.clone();
                tasks.spawn(async move {
                    tokio::select! {
                        _ = clock_cancel.cancelled() => {}
                        result = crate::sources::clock::run(clock_bus, clock_updates) => {
                            if let Err(error) = result {
                                tracing::debug!(error = %error, "attachment clock source stopped");
                            }
                            clock_telemetry.record_health("clock", SourceStatus::Failed);
                        }
                    }
                });
                spawn_motion(
                    &mut tasks,
                    bus.clone(),
                    telemetry.clone(),
                    cancellation.clone(),
                );
                let input_cancel = cancellation.clone();
                let input_telemetry = telemetry.clone();
                tasks.spawn(async move {
                    let mut input_rx = input_rx;
                    tokio::select! {
                        _ = input_cancel.cancelled() => {}
                        result = crate::sources::input::run(bus, telemetry, &mut input_rx) => {
                            if let Err(error) = result {
                                tracing::debug!(error = %error, "attachment input source stopped");
                            }
                            input_telemetry.record_health("input", SourceStatus::Failed);
                        }
                    }
                    let _ = return_tx.send(input_rx);
                });
            }
            None => {
                let input_cancel = cancellation.clone();
                tasks.spawn(async move {
                    input_cancel.cancelled().await;
                    let _ = return_tx.send(input_rx);
                });
            }
        }
        Self {
            cancellation,
            tasks,
            transports,
            input_return,
        }
    }

    pub async fn shutdown(self) -> mpsc::Receiver<InputCommand> {
        self.cancellation.cancel();
        self.tasks.join().await;
        self.transports.close().await;
        self.input_return
            .await
            .unwrap_or_else(|_| mpsc::channel(1).1)
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

fn spawn_device(
    tasks: &mut TaskGroup,
    bus: phoxal::raw::Bus,
    scope: phoxal_cli_core::session::RobotScope,
    telemetry: TelemetryBackend,
    cancellation: CancellationToken,
) {
    let source = format!("device:{}/{}", scope.namespace, scope.robot_id);
    let failed = telemetry.clone();
    tasks.spawn(async move {
        tokio::select! {
            _ = cancellation.cancelled() => {}
            result = crate::sources::device::run(bus, scope, telemetry) => {
                let error = result.expect_err("device source is intentionally endless");
                tracing::debug!(error = %error, "attachment device source stopped");
                failed.record_health(source, SourceStatus::Failed);
            }
        }
    });
}

fn spawn_bus(
    tasks: &mut TaskGroup,
    bus: phoxal::raw::Bus,
    scope: phoxal_cli_core::session::RobotScope,
    telemetry: TelemetryBackend,
    cancellation: CancellationToken,
) {
    let source = format!("bus:{}/{}", scope.namespace, scope.robot_id);
    let failed = telemetry.clone();
    tasks.spawn(async move {
        tokio::select! {
            _ = cancellation.cancelled() => {}
            result = crate::sources::bus::run(bus, scope, telemetry) => {
                let error = result.expect_err("bus source is intentionally endless");
                tracing::debug!(error = %error, "attachment bus source stopped");
                failed.record_health(source, SourceStatus::Failed);
            }
        }
    });
}

fn spawn_runtimes(
    tasks: &mut TaskGroup,
    bus: phoxal::raw::Bus,
    scope: phoxal_cli_core::session::RobotScope,
    participants: Vec<String>,
    telemetry: TelemetryBackend,
    cancellation: CancellationToken,
) {
    let source = format!("runtimes:{}/{}", scope.namespace, scope.robot_id);
    let failed = telemetry.clone();
    tasks.spawn(async move {
        tokio::select! {
            _ = cancellation.cancelled() => {}
            result = crate::sources::runtimes::run(bus, scope, participants, telemetry) => {
                let error = result.expect_err("runtime source is intentionally endless");
                tracing::debug!(error = %error, "attachment runtime source stopped");
                failed.record_health(source, SourceStatus::Failed);
            }
        }
    });
}

fn spawn_motion(
    tasks: &mut TaskGroup,
    bus: phoxal::raw::Bus,
    telemetry: TelemetryBackend,
    cancellation: CancellationToken,
) {
    let failed = telemetry.clone();
    tasks.spawn(async move {
        tokio::select! {
            _ = cancellation.cancelled() => {}
            result = crate::sources::motion::run(bus, telemetry) => {
                if let Err(error) = result {
                    tracing::debug!(error = %error, "attachment motion source stopped");
                }
                failed.record_health("motion", SourceStatus::Failed);
            }
        }
    });
}

async fn apply_telemetry_update(
    epoch: AttachmentEpoch,
    stores: &Stores,
    events: &mpsc::Sender<AttachmentEvent>,
    health: &Arc<tokio::sync::RwLock<SourceHealth>>,
    freshness: &super::freshness::DeadlineSender,
    update: TelemetryUpdate,
) {
    let (source, status) = match &update {
        TelemetryUpdate::Clock(_) => ("clock".to_string(), SourceStatus::Live),
        TelemetryUpdate::Device(scope, _) => (
            format!("device:{}/{}", scope.namespace, scope.robot_id),
            SourceStatus::Live,
        ),
        TelemetryUpdate::Router(scope, _) | TelemetryUpdate::Routers(scope, _) => (
            format!("bus:{}/{}", scope.namespace, scope.robot_id),
            SourceStatus::Live,
        ),
        TelemetryUpdate::Runtimes(scope, _, _) | TelemetryUpdate::Runtime(scope, _) => (
            format!("runtimes:{}/{}", scope.namespace, scope.robot_id),
            SourceStatus::Live,
        ),
        TelemetryUpdate::Joypads(_) => ("input".to_string(), SourceStatus::Live),
        TelemetryUpdate::Motion(_) => ("motion".to_string(), SourceStatus::Live),
        TelemetryUpdate::Health(source, status) => (source.clone(), *status),
    };
    if status == SourceStatus::Live {
        super::freshness::refresh(
            freshness,
            source.clone(),
            phoxal_cli_core::session::DEFAULT_FRESHNESS_TTL,
        );
    }
    let changed_health = {
        let mut health = health.write().await;
        let changed = health.sources.insert(source, status) != Some(status);
        changed.then(|| health.clone())
    };
    if let Some(health) = changed_health {
        let _ = events
            .send(AttachmentEvent::SourceHealthChanged(Arc::new(health)))
            .await;
    }
    match update {
        TelemetryUpdate::Clock(sample) => {
            let observation = stores.device.write().await.record_clock(sample);
            let _ = events
                .send(AttachmentEvent::DeviceChanged(Arc::new(observation)))
                .await;
        }
        TelemetryUpdate::Device(scope, sample) => {
            let observation = stores
                .device
                .write()
                .await
                .record(RobotKey::new(scope.namespace, scope.robot_id), sample);
            let _ = events
                .send(AttachmentEvent::DeviceChanged(Arc::new(observation)))
                .await;
        }
        TelemetryUpdate::Router(scope, sample) => {
            let rows = bus_rows(&scope, &sample);
            let revision = stores.bus.write().await.record(epoch, rows);
            if let Some(revision) = revision {
                let _ = events
                    .send(AttachmentEvent::BusChanged(StoreChanged {
                        epoch,
                        revision,
                    }))
                    .await;
            }
        }
        TelemetryUpdate::Routers(scope, samples) => {
            let rows = samples
                .iter()
                .flat_map(|sample| bus_rows(&scope, sample))
                .collect::<Vec<_>>();
            let revision = stores.bus.write().await.install(epoch, &scope, rows);
            if let Some(revision) = revision {
                let _ = events
                    .send(AttachmentEvent::BusChanged(StoreChanged {
                        epoch,
                        revision,
                    }))
                    .await;
            }
        }
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
                .send(AttachmentEvent::InputChanged(Arc::new(observation)))
                .await;
        }
        TelemetryUpdate::Motion(motion) => {
            let motion = stores.motion.write().await.record(MotionObservation {
                linear_x_mps: motion.final_target.linear_x_mps,
                angular_z_radps: motion.final_target.angular_z_radps,
            });
            let observation = stores.input.read().await.observe(motion);
            let _ = events
                .send(AttachmentEvent::InputChanged(Arc::new(observation)))
                .await;
        }
        TelemetryUpdate::Health(_, _) => {}
    }
}

fn bus_rows(
    scope: &phoxal_cli_core::session::RobotScope,
    sample: &phoxal_cli_core::session::Timestamped<phoxal_cli_core::session::RouterMetricsSample>,
) -> Vec<BusRow> {
    let make_row = |topic: Option<&phoxal_cli_core::session::TopicMetric>| BusRow {
        scope: scope.clone(),
        observed_at: sample.received_at,
        topic: topic.map_or_else(String::new, |topic| topic.topic.clone()),
        participant: topic.map_or_else(String::new, |topic| topic.from_participant.clone()),
        rate_hz: topic.map_or(0.0, |topic| topic.ingress_rate_hz),
        count: topic.map_or(0, |topic| topic.count),
        aggregate_overflow: topic.is_some_and(|topic| topic.aggregate_overflow),
        topics_truncated: sample.value.topics_truncated,
        throughput_msg_s: sample.value.throughput_msg_s,
        window_ns: sample.value.window_ns,
    };
    if sample.value.topics.is_empty() {
        vec![make_row(None)]
    } else {
        sample
            .value
            .topics
            .iter()
            .map(|topic| make_row(Some(topic)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use phoxal_cli_core::session::{RobotScope, RouterMetricsSample, Timestamped};

    use super::bus_rows;

    #[test]
    fn a_topicless_retained_window_still_preserves_throughput_history() {
        let scope = RobotScope {
            namespace: "lab".to_string(),
            robot_id: "rover".to_string(),
        };
        let sample = Timestamped::new(
            RouterMetricsSample {
                topics: Arc::new(Vec::new()),
                topics_truncated: 0,
                throughput_msg_s: 12.5,
                window_ns: 1_000_000_000,
            },
            std::time::Instant::now(),
        );
        let rows = bus_rows(&scope, &sample);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].topic.is_empty());
        assert_eq!(rows[0].throughput_msg_s, 12.5);
    }
}
