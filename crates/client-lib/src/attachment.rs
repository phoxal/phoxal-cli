use phoxal_api::supervisor;
use phoxal_api::supervisor::command::{Command, CommandOutcome};
use phoxal_api::supervisor::connect::{ConnectReply, ConnectRequest, PRESENCE_KEY};
use phoxal_api::supervisor::info::ManualDrive;
use phoxal_api::supervisor::snapshot::{Snapshot, SnapshotDocument};
use phoxal_bus::{
    BusConfig, BusHandle, BusOwner, DEFAULT_QUERY_TIMEOUT, KeyLivelinessObserver, LivelinessStatus,
    Querier, QueryError, SourceLabel, StreamReceiver,
};
use phoxal_runtime_contract::clock::Clock;
use phoxal_runtime_contract::identity::{ExecutionId, ParticipantId, ProducerId, RobotId};
use phoxal_runtime_contract::version::FrameworkVersion;
use std::sync::Arc;
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::AttachError;
use crate::router::exactly_one_execution;

/// Inputs for one direct attachment.
#[derive(Clone, Debug)]
pub struct AttachmentConfig {
    pub endpoint: String,
    pub label: String,
}

impl AttachmentConfig {
    #[must_use]
    pub fn new(endpoint: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            label: label.into(),
        }
    }
}

/// Immutable facts established by the attachment handshake.
#[derive(Clone, Debug)]
pub struct Connected {
    pub execution: ExecutionId,
    pub robot: RobotId,
    pub clock: Clock,
    pub manual_drive: Option<ManualDrive>,
}

/// A cloneable set of operations on one uniquely-owned attachment.
#[derive(Clone)]
pub struct AttachmentPort {
    connected: Arc<Connected>,
    bus: BusHandle,
    snapshots: watch::Receiver<Option<Snapshot>>,
    disconnected: watch::Receiver<bool>,
    command: Querier<supervisor::command::Request, supervisor::command::Reply>,
    logs: Querier<supervisor::logs::SnapshotRequest, supervisor::logs::Snapshot>,
    telemetry: Querier<supervisor::telemetry::SnapshotRequest, supervisor::telemetry::Snapshot>,
}

impl std::fmt::Debug for AttachmentPort {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AttachmentPort")
            .field("execution", &self.connected.execution)
            .finish_non_exhaustive()
    }
}

impl AttachmentPort {
    #[must_use]
    pub fn connected(&self) -> &Connected {
        &self.connected
    }

    #[must_use]
    pub fn execution(&self) -> ExecutionId {
        self.connected.execution
    }

    #[must_use]
    pub fn bus(&self) -> &BusHandle {
        &self.bus
    }

    #[must_use]
    pub fn snapshots(&self) -> watch::Receiver<Option<Snapshot>> {
        self.snapshots.clone()
    }

    #[must_use]
    pub fn snapshot(&self) -> Option<Snapshot> {
        self.snapshots.borrow().clone()
    }

    pub async fn disconnected(&self) {
        let mut receiver = self.disconnected.clone();
        while !*receiver.borrow_and_update() {
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }

    /// Wait until the execution can accept operator work. Timeout policy stays
    /// with the caller; lifecycle classification and disconnect handling are
    /// identical for every attachment consumer.
    pub async fn wait_ready(&self) -> Result<Snapshot, AttachError> {
        let mut snapshots = self.snapshots();
        loop {
            if let Some(snapshot) = snapshots.borrow_and_update().clone() {
                match classify_readiness(&snapshot)? {
                    Readiness::Ready => return Ok(snapshot),
                    Readiness::Pending => {}
                }
            }
            tokio::select! {
                () = self.disconnected() => return Err(AttachError::DisconnectedBeforeReady),
                changed = snapshots.changed() => {
                    changed.map_err(|_| AttachError::SnapshotStreamClosed)?;
                }
            }
        }
    }

    pub async fn command(&self, command: Command) -> Result<CommandOutcome, AttachError> {
        Ok(self
            .command
            .query(supervisor::command::Request::V0 { command })
            .await
            .map(|reply| match reply {
                supervisor::command::Reply::V0 { outcome } => outcome,
            })?)
    }

    pub async fn restart(
        &self,
        participant: ParticipantId,
        expected_producer: ProducerId,
    ) -> Result<CommandOutcome, AttachError> {
        self.command(Command::Restart {
            participant,
            expected_producer: Some(expected_producer),
        })
        .await
    }

    /// End the execution, fenced on the newest revision this attachment has
    /// installed. The fence is read here rather than passed in: this port owns
    /// the snapshot watch, so it is the one place that knows the revision the
    /// caller is actually acting on.
    pub async fn stop(&self) -> Result<CommandOutcome, AttachError> {
        self.command(Command::Stop {
            expected_revision: self.revision()?,
        })
        .await
    }

    /// The revision of the newest snapshot this attachment has installed.
    fn revision(&self) -> Result<u64, AttachError> {
        self.snapshots
            .borrow()
            .as_ref()
            .map(|snapshot| snapshot.revision)
            .ok_or(AttachError::NoSnapshotRevision)
    }

    pub async fn logs(
        &self,
        participant_id: Option<String>,
        limit: u32,
        before_sequence: Option<u64>,
    ) -> Result<supervisor::logs::Snapshot, AttachError> {
        Ok(self
            .logs
            .query(supervisor::logs::SnapshotRequest {
                participant_id,
                limit,
                before_sequence,
            })
            .await?)
    }

    pub async fn follow_logs(
        &self,
    ) -> Result<StreamReceiver<supervisor::endpoint::logs::FollowEndpoint>, AttachError> {
        Ok(StreamReceiver::new(&self.bus, &supervisor::topic::client().logs().follow()).await?)
    }

    pub async fn telemetry(
        &self,
        participant_id: Option<String>,
        limit: u32,
        before_sequence: Option<u64>,
    ) -> Result<supervisor::telemetry::Snapshot, AttachError> {
        Ok(self
            .telemetry
            .query(supervisor::telemetry::SnapshotRequest {
                participant_id,
                limit,
                before_sequence,
            })
            .await?)
    }

    pub async fn follow_telemetry(
        &self,
    ) -> Result<StreamReceiver<supervisor::endpoint::telemetry::FollowEndpoint>, AttachError> {
        Ok(
            StreamReceiver::new(&self.bus, &supervisor::topic::client().telemetry().follow())
                .await?,
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Readiness {
    Ready,
    Pending,
}

fn classify_readiness(snapshot: &Snapshot) -> Result<Readiness, AttachError> {
    match snapshot.lifecycle {
        phoxal_api::supervisor::snapshot::Lifecycle::Ready
        | phoxal_api::supervisor::snapshot::Lifecycle::Degraded => Ok(Readiness::Ready),
        phoxal_api::supervisor::snapshot::Lifecycle::Failed => {
            let detail = snapshot.failure.as_ref().map_or_else(
                || "no reason was reported".to_string(),
                |failure| format!("{:?}: {}", failure.reason, failure.detail.as_str()),
            );
            Err(AttachError::ReadinessFailed(detail))
        }
        phoxal_api::supervisor::snapshot::Lifecycle::Stopping
        | phoxal_api::supervisor::snapshot::Lifecycle::Stopped => {
            Err(AttachError::StoppedBeforeReady)
        }
        phoxal_api::supervisor::snapshot::Lifecycle::Starting => Ok(Readiness::Pending),
    }
}

/// Unique owner of one attached transport and every task derived from it.
pub struct Attachment {
    endpoint: String,
    owner: BusOwner,
    port: AttachmentPort,
    _identity: KeyLivelinessObserver,
    tasks: JoinSet<()>,
}

impl std::fmt::Debug for Attachment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Attachment")
            .field("endpoint", &self.endpoint)
            .field("execution", &self.port.execution())
            .finish_non_exhaustive()
    }
}

impl Attachment {
    /// Attach to the one execution reachable at `config.endpoint`.
    ///
    /// A robot and a client interoperate exactly when their trains share a
    /// compatibility line, so that is what the gate below asks. The frozen
    /// bootstrap still reports each side's exact train, which is the
    /// provenance the refusal names.
    pub async fn open(config: &AttachmentConfig) -> Result<Self, AttachError> {
        let executions = BusOwner::probe_routers(&config.endpoint).await?;
        let execution = exactly_one_execution(&config.endpoint, &executions)?;
        let label = SourceLabel::new(config.label.clone())?;
        let (owner, bus) = BusOwner::open(BusConfig::for_external(
            execution,
            Some(label),
            vec![config.endpoint.clone()],
        ))
        .await?;

        // The frozen bootstrap answers before any ordinary endpoint is
        // touched: a peer on another line can decode this one reply and name
        // the disagreement, where a richer reply would only fail to parse. The
        // exact trains it reports are carried into the refusal; only the
        // decision is the line.
        let robot = framework(&bus).await?;
        ensure_compatible_framework(robot, FrameworkVersion::CURRENT)?;

        let stream =
            StreamReceiver::new(&bus, &supervisor::topic::client().snapshot().topic()).await?;
        let current = Querier::new(
            bus.clone(),
            &supervisor::topic::client().snapshot().current(),
            DEFAULT_QUERY_TIMEOUT,
        )?
        .query(supervisor::snapshot::CurrentRequest {})
        .await?
        .into_snapshot();
        current.validate()?;

        let info = Querier::new(
            bus.clone(),
            &supervisor::topic::client().info().topic(),
            DEFAULT_QUERY_TIMEOUT,
        )?
        .query(supervisor::info::InfoRequest {})
        .await?;

        let (snapshots_tx, snapshots) = watch::channel(Some(current.clone()));
        let mut tasks = JoinSet::new();
        tasks.spawn(pump_snapshots(stream, snapshots_tx, current.revision));

        let (disconnected_tx, disconnected) = watch::channel(false);
        let on_change = disconnected_tx.clone();
        let identity = bus
            .observe_liveliness_key(PRESENCE_KEY, move |status| {
                if status == LivelinessStatus::Lost {
                    let _ = on_change.send(true);
                }
            })
            .await?;
        if identity.initial() == LivelinessStatus::Lost {
            let _ = disconnected_tx.send(true);
        }

        let connected = Arc::new(Connected {
            execution,
            robot: info.robot,
            clock: info.clock,
            manual_drive: info.manual_drive,
        });
        let port = AttachmentPort {
            connected,
            command: Querier::new(
                bus.clone(),
                &supervisor::topic::client().command().topic(),
                DEFAULT_QUERY_TIMEOUT,
            )?,
            logs: Querier::new(
                bus.clone(),
                &supervisor::topic::client().logs().snapshot(),
                DEFAULT_QUERY_TIMEOUT,
            )?,
            telemetry: Querier::new(
                bus.clone(),
                &supervisor::topic::client().telemetry().snapshot(),
                DEFAULT_QUERY_TIMEOUT,
            )?,
            bus,
            snapshots,
            disconnected,
        };

        Ok(Self {
            endpoint: config.endpoint.clone(),
            owner,
            port,
            _identity: identity,
            tasks,
        })
    }

    #[must_use]
    pub fn port(&self) -> AttachmentPort {
        self.port.clone()
    }

    #[must_use]
    pub fn connected(&self) -> &Connected {
        self.port.connected()
    }

    #[must_use]
    pub fn execution(&self) -> ExecutionId {
        self.port.execution()
    }

    pub async fn close(mut self) -> Result<(), AttachError> {
        self.tasks.shutdown().await;
        let report = self.owner.close().await;
        if report.is_clean() {
            Ok(())
        } else {
            Err(AttachError::Close(report.to_string()))
        }
    }
}

/// Complete the frozen attachment bootstrap and report the robot's train.
///
/// A reply this client cannot decode is a compatibility answer in its own
/// right: the bootstrap is permanently stable, so the only reason it fails to
/// parse is a peer that speaks a different one.
async fn framework(bus: &BusHandle) -> Result<FrameworkVersion, AttachError> {
    let reply = Querier::new(
        bus.clone(),
        &supervisor::topic::client().connect().topic(),
        DEFAULT_QUERY_TIMEOUT,
    )?
    .query(ConnectRequest::V0 {})
    .await
    .map_err(|error| match error {
        QueryError::Decode(detail) => AttachError::UnreadableConnectReply { detail },
        other => AttachError::Query(other),
    })?;
    let ConnectReply::V0 { framework } = reply;
    Ok(framework)
}

fn ensure_compatible_framework(
    robot: FrameworkVersion,
    client: FrameworkVersion,
) -> Result<(), AttachError> {
    if robot.is_compatible_with(client) {
        Ok(())
    } else {
        Err(AttachError::IncompatibleFramework { robot, client })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SnapshotPumpDecision {
    Continue,
    Stop,
}

fn pump_snapshot_step(
    sender: &watch::Sender<Option<Snapshot>>,
    revision: &mut u64,
    snapshot: Option<Snapshot>,
) -> SnapshotPumpDecision {
    let Some(snapshot) = snapshot else {
        return SnapshotPumpDecision::Stop;
    };
    if snapshot.revision <= *revision {
        return SnapshotPumpDecision::Continue;
    }
    let installed_revision = snapshot.revision;
    if sender.send(Some(snapshot)).is_err() {
        return SnapshotPumpDecision::Stop;
    }
    *revision = installed_revision;
    SnapshotPumpDecision::Continue
}

async fn pump_snapshots(
    stream: StreamReceiver<supervisor::endpoint::snapshot::TopicEndpoint>,
    sender: watch::Sender<Option<Snapshot>>,
    mut revision: u64,
) {
    loop {
        let snapshot = stream.recv().await.ok().map(|observed| {
            let SnapshotDocument::V0(snapshot) = observed.body;
            snapshot
        });
        if pump_snapshot_step(&sender, &mut revision, snapshot) == SnapshotPumpDecision::Stop {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_api::supervisor::snapshot::{DaemonFailure, DaemonFailureReason, Lifecycle};

    fn snapshot(revision: u64, lifecycle: Lifecycle) -> Snapshot {
        Snapshot {
            revision,
            lifecycle,
            startup: Vec::new(),
            processes: Vec::new(),
            failure: None,
        }
    }

    #[test]
    fn framework_selection_accepts_both_directions_within_one_line() {
        for (older, newer) in [
            (
                FrameworkVersion::new(0, 59, 1),
                FrameworkVersion::new(0, 59, 9),
            ),
            (
                FrameworkVersion::new(1, 2, 1),
                FrameworkVersion::new(1, 9, 9),
            ),
        ] {
            assert!(ensure_compatible_framework(older, newer).is_ok());
            assert!(ensure_compatible_framework(newer, older).is_ok());
        }
    }

    #[test]
    fn framework_selection_preserves_exact_incompatible_versions() {
        let robot = FrameworkVersion::new(0, 59, 7);
        let client = FrameworkVersion::new(0, 60, 3);

        let error = ensure_compatible_framework(robot, client).expect_err("lines differ");
        assert!(matches!(
            error,
            AttachError::IncompatibleFramework {
                robot: actual_robot,
                client: actual_client,
            } if actual_robot == robot && actual_client == client
        ));
    }

    #[test]
    fn unreadable_frozen_bootstrap_reply_remains_a_framework_mismatch() {
        let error = AttachError::UnreadableConnectReply {
            detail: "phoxal/supervisor-connect/v1".to_string(),
        };

        assert!(error.is_framework_mismatch());
    }

    #[test]
    fn ready_and_degraded_snapshots_succeed() {
        for lifecycle in [Lifecycle::Ready, Lifecycle::Degraded] {
            assert_eq!(
                classify_readiness(&snapshot(1, lifecycle)).expect("ready lifecycle"),
                Readiness::Ready
            );
        }
    }

    #[test]
    fn failed_readiness_preserves_its_reason_and_detail() {
        let mut failed = snapshot(1, Lifecycle::Failed);
        failed.failure = Some(DaemonFailure::new(
            DaemonFailureReason::LaunchFailed,
            "participant launch plan was rejected",
        ));

        let error = classify_readiness(&failed).expect_err("failed lifecycle");
        assert!(matches!(
            error,
            AttachError::ReadinessFailed(detail)
                if detail == "LaunchFailed: participant launch plan was rejected"
        ));
    }

    #[test]
    fn stopping_and_stopped_snapshots_fail_before_readiness() {
        for lifecycle in [Lifecycle::Stopping, Lifecycle::Stopped] {
            assert!(matches!(
                classify_readiness(&snapshot(1, lifecycle)),
                Err(AttachError::StoppedBeforeReady)
            ));
        }
    }

    #[test]
    fn starting_snapshot_remains_pending() {
        assert_eq!(
            classify_readiness(&snapshot(1, Lifecycle::Starting)).expect("starting lifecycle"),
            Readiness::Pending
        );
    }

    #[test]
    fn snapshot_pump_installs_only_strictly_newer_revisions() {
        let (sender, receiver) = watch::channel(Some(snapshot(7, Lifecycle::Starting)));
        let mut revision = 7;

        assert_eq!(
            pump_snapshot_step(&sender, &mut revision, Some(snapshot(8, Lifecycle::Ready)),),
            SnapshotPumpDecision::Continue
        );
        assert_eq!(revision, 8);
        assert_eq!(
            receiver.borrow().as_ref().map(|snapshot| snapshot.revision),
            Some(8)
        );
    }

    #[test]
    fn snapshot_pump_ignores_duplicate_and_out_of_order_revisions() {
        let (sender, receiver) = watch::channel(Some(snapshot(7, Lifecycle::Ready)));
        let mut revision = 7;

        for observed in [7, 6] {
            assert_eq!(
                pump_snapshot_step(
                    &sender,
                    &mut revision,
                    Some(snapshot(observed, Lifecycle::Starting)),
                ),
                SnapshotPumpDecision::Continue
            );
        }
        assert_eq!(revision, 7);
        assert_eq!(
            receiver.borrow().as_ref().map(|snapshot| snapshot.revision),
            Some(7)
        );
    }

    #[test]
    fn snapshot_pump_stops_for_a_closed_source_or_watch_receiver() {
        let (sender, receiver) = watch::channel(Some(snapshot(7, Lifecycle::Starting)));
        let mut revision = 7;

        assert_eq!(
            pump_snapshot_step(&sender, &mut revision, None),
            SnapshotPumpDecision::Stop
        );
        drop(receiver);
        assert_eq!(
            pump_snapshot_step(&sender, &mut revision, Some(snapshot(8, Lifecycle::Ready)),),
            SnapshotPumpDecision::Stop
        );
        assert_eq!(revision, 7);
    }
}
