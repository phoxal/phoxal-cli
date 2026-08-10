use phoxal_bus::{
    BusConfig, BusHandle, BusOwner, DEFAULT_QUERY_TIMEOUT, KeyLivelinessObserver, LivelinessStatus,
    Querier, SourceLabel, StreamReceiver,
};
use phoxal_runtime_contract::identity::{ExecutionId, ParticipantId, ProducerId};
use phoxal_runtime_contract::version::RobotApiVersion;
use phoxal_supervisor_api::{
    Clock, Command, CommandOutcome, PRESENCE_KEY, RobotId, Snapshot, SnapshotDocument, payload,
    supervisor,
};
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
    pub robot_api: RobotApiVersion,
    pub manual_drive: Option<payload::runtime::ManualDrive>,
}

/// A cloneable set of operations on one uniquely-owned attachment.
#[derive(Clone)]
pub struct AttachmentPort {
    connected: Arc<Connected>,
    bus: BusHandle,
    snapshots: watch::Receiver<Option<Snapshot>>,
    disconnected: watch::Receiver<bool>,
    command: Querier<payload::command::Request, payload::command::Reply>,
    logs: Querier<payload::log::SnapshotRequest, payload::log::Snapshot>,
    telemetry: Querier<payload::telemetry::SnapshotRequest, payload::telemetry::Snapshot>,
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
                match snapshot.lifecycle {
                    phoxal_supervisor_api::Lifecycle::Ready
                    | phoxal_supervisor_api::Lifecycle::Degraded => return Ok(snapshot),
                    phoxal_supervisor_api::Lifecycle::Failed => {
                        let detail = snapshot.failure.as_ref().map_or_else(
                            || "no reason was reported".to_string(),
                            |failure| format!("{:?}: {}", failure.reason, failure.detail.as_str()),
                        );
                        return Err(AttachError::ReadinessFailed(detail));
                    }
                    phoxal_supervisor_api::Lifecycle::Stopping
                    | phoxal_supervisor_api::Lifecycle::Stopped => {
                        return Err(AttachError::StoppedBeforeReady);
                    }
                    phoxal_supervisor_api::Lifecycle::Starting => {}
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
            .query(payload::command::Request::V0 { command })
            .await
            .map(|reply| match reply {
                payload::command::Reply::V0 { outcome } => outcome,
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

    pub async fn stop(&self) -> Result<CommandOutcome, AttachError> {
        self.command(Command::Stop).await
    }

    pub async fn logs(
        &self,
        participant_id: Option<String>,
        limit: u32,
        before_sequence: Option<u64>,
    ) -> Result<payload::log::Snapshot, AttachError> {
        Ok(self
            .logs
            .query(payload::log::SnapshotRequest {
                participant_id,
                limit,
                before_sequence,
            })
            .await?)
    }

    pub async fn follow_logs(
        &self,
    ) -> Result<StreamReceiver<supervisor::endpoint::log::FollowEndpoint>, AttachError> {
        Ok(StreamReceiver::new(&self.bus, &supervisor::topic::client().log().follow()).await?)
    }

    pub async fn telemetry(
        &self,
        participant_id: Option<String>,
        limit: u32,
        before_sequence: Option<u64>,
    ) -> Result<payload::telemetry::Snapshot, AttachError> {
        Ok(self
            .telemetry
            .query(payload::telemetry::SnapshotRequest {
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

        let stream =
            StreamReceiver::new(&bus, &supervisor::topic::client().snapshot().topic()).await?;
        let current = Querier::new(
            bus.clone(),
            &supervisor::topic::client().snapshot().current(),
            DEFAULT_QUERY_TIMEOUT,
        )?
        .query(payload::snapshot::CurrentRequest {})
        .await?
        .into_snapshot();
        current.validate()?;

        let info = Querier::new(
            bus.clone(),
            &supervisor::topic::client().runtime().info(),
            DEFAULT_QUERY_TIMEOUT,
        )?
        .query(payload::runtime::InfoRequest {})
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
            robot_api: info.robot_api,
            manual_drive: info.manual_drive,
        });
        let port = AttachmentPort {
            connected,
            command: Querier::new(
                bus.clone(),
                &supervisor::topic::client().control().topic(),
                DEFAULT_QUERY_TIMEOUT,
            )?,
            logs: Querier::new(
                bus.clone(),
                &supervisor::topic::client().log().snapshot(),
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

async fn pump_snapshots(
    stream: StreamReceiver<supervisor::endpoint::snapshot::TopicEndpoint>,
    sender: watch::Sender<Option<Snapshot>>,
    mut revision: u64,
) {
    loop {
        let Ok(observed) = stream.recv().await else {
            return;
        };
        let SnapshotDocument::V0(snapshot) = observed.body;
        if snapshot.revision > revision {
            revision = snapshot.revision;
            if sender.send(Some(snapshot)).is_err() {
                return;
            }
        }
    }
}
