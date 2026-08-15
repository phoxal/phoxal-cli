use std::{future::Future, sync::Arc};

use phoxal_bus::{
    AskQuery, BusConfig, BusFault, BusHandle, BusOwner, ClientPublishContract,
    ClientReceiveContract, DEFAULT_QUERY_TIMEOUT, EventContract, EventReceiver,
    KeyLivelinessObserver, LivelinessStatus, Publish, Querier, QueryEndpointDescriptor, QueryError,
    SampleDeliveryContract, SampleReceiver, SetpointContract, SetpointPublisher, SourceLabel,
    StateDeliveryContract, StateView, StreamContract, StreamDeliveryContract, StreamPublisher,
    StreamReceiver, Subscribe, Topic,
};
use phoxal_protocol::supervisor;
use phoxal_protocol::supervisor::connect::{ConnectReply, ConnectRequest, PRESENCE_KEY};
use phoxal_protocol::supervisor::execution::{
    Command, CommandOutcome, Lifecycle, Snapshot, SnapshotDocument,
};
use phoxal_protocol::supervisor::info::ManualDrive;
use phoxal_runtime_contract::clock::Clock;
use phoxal_runtime_contract::identity::{ExecutionId, ParticipantId, ProducerId, RobotId};
use phoxal_runtime_contract::version::FrameworkVersion;
use tokio::sync::{oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};

use crate::error::{ClientError, CloseError, CompatibilityRefusal, ConnectError, DisconnectReason};
use crate::router::exactly_one_execution;

/// Inputs for one direct connection to one execution.
#[derive(Clone, Debug)]
pub struct ConnectOptions {
    pub endpoint: String,
    pub label: String,
}

impl ConnectOptions {
    #[must_use]
    pub fn new(endpoint: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            label: label.into(),
        }
    }
}

/// Immutable facts established while connecting.
#[derive(Clone, Debug)]
pub struct Connected {
    pub execution: ExecutionId,
    pub robot: RobotId,
    pub clock: Clock,
    pub manual_drive: Option<ManualDrive>,
    pub framework: FrameworkVersion,
}

/// Cloneable operations borrowed from one uniquely-owned [`Connection`].
///
/// A client cannot create, replace, or close the underlying session. Once the
/// supervisor identity, snapshot stream, or bus worker is lost, or the owner
/// begins close, existing typed handles become terminal and new operations
/// return [`ClientError::Disconnected`].
#[derive(Clone)]
pub struct Client {
    connected: Arc<Connected>,
    bus: BusHandle,
    snapshots: watch::Receiver<Option<Snapshot>>,
    terminal: watch::Receiver<Option<DisconnectReason>>,
    command: Querier<supervisor::command::Request, supervisor::command::Reply>,
    logs: Querier<supervisor::logs::SnapshotRequest, supervisor::logs::Snapshot>,
    telemetry: Querier<supervisor::telemetry::SnapshotRequest, supervisor::telemetry::Snapshot>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Client")
            .field("execution", &self.connected.execution)
            .field("terminal", &self.disconnect_reason())
            .finish_non_exhaustive()
    }
}

impl Client {
    #[must_use]
    pub fn connected(&self) -> &Connected {
        &self.connected
    }

    #[must_use]
    pub fn execution(&self) -> ExecutionId {
        self.connected.execution
    }

    /// A receiver for the newest owner-published state.
    pub async fn state_view<E>(
        &self,
        topic: Topic<Subscribe<E>>,
    ) -> Result<StateView<E>, ClientError>
    where
        E: StateDeliveryContract + ClientReceiveContract,
    {
        self.ensure_connected()?;
        Ok(StateView::new(&self.bus, &topic).await?)
    }

    /// A bounded ordered receiver for owner-published samples.
    pub async fn sample_receiver<E>(
        &self,
        topic: Topic<Subscribe<E>>,
    ) -> Result<SampleReceiver<E>, ClientError>
    where
        E: SampleDeliveryContract + ClientReceiveContract,
    {
        self.ensure_connected()?;
        Ok(SampleReceiver::new(&self.bus, &topic).await?)
    }

    /// An ordered receiver for owner-published events.
    pub async fn event_receiver<E>(
        &self,
        topic: Topic<Subscribe<E>>,
    ) -> Result<EventReceiver<E>, ClientError>
    where
        E: EventContract + ClientReceiveContract,
    {
        self.ensure_connected()?;
        Ok(EventReceiver::new(&self.bus, &topic).await?)
    }

    /// An ordered receiver for `Stream<_, Out>` chunks.
    pub async fn stream_receiver<E>(
        &self,
        topic: Topic<Subscribe<E>>,
    ) -> Result<StreamReceiver<E>, ClientError>
    where
        E: StreamContract + StreamDeliveryContract + ClientReceiveContract,
    {
        self.ensure_connected()?;
        Ok(StreamReceiver::new(&self.bus, &topic).await?)
    }

    /// A newest-actionable publisher for an owner-consumed setpoint.
    pub fn setpoint_publisher<E>(
        &self,
        topic: Topic<Publish<E>>,
    ) -> Result<SetpointPublisher<E>, ClientError>
    where
        E: SetpointContract + ClientPublishContract,
    {
        self.ensure_connected()?;
        Ok(SetpointPublisher::new(self.bus.clone(), &topic)?)
    }

    /// An ordered publisher for `Stream<_, In>` chunks.
    pub fn stream_publisher<E>(
        &self,
        topic: Topic<Publish<E>>,
    ) -> Result<StreamPublisher<E>, ClientError>
    where
        E: StreamContract + StreamDeliveryContract + ClientPublishContract,
    {
        self.ensure_connected()?;
        Ok(StreamPublisher::new(self.bus.clone(), &topic)?)
    }

    /// A bounded request/reply handle using the framework's current default
    /// query timeout.
    pub fn querier<E>(
        &self,
        topic: Topic<AskQuery<E>>,
    ) -> Result<Querier<E::Request, E::Response>, ClientError>
    where
        E: QueryEndpointDescriptor,
    {
        self.ensure_connected()?;
        Ok(Querier::new(
            self.bus.clone(),
            &topic,
            DEFAULT_QUERY_TIMEOUT,
        )?)
    }

    #[must_use]
    pub fn snapshots(&self) -> watch::Receiver<Option<Snapshot>> {
        self.snapshots.clone()
    }

    #[must_use]
    pub fn snapshot(&self) -> Option<Snapshot> {
        self.snapshots.borrow().clone()
    }

    /// The first terminal reason observed for this connection.
    #[must_use]
    pub fn disconnect_reason(&self) -> Option<DisconnectReason> {
        terminal_reason(&self.terminal)
    }

    /// Resolve when this connection becomes terminal.
    ///
    /// The returned value is the same first cause retained by
    /// [`Self::disconnect_reason`].
    pub async fn disconnected(&self) -> DisconnectReason {
        let mut terminal = self.terminal.clone();
        wait_for_terminal(&mut terminal).await
    }

    /// Wait until the execution can accept work.
    ///
    /// Timeout policy remains with the application.
    pub async fn wait_ready(&self) -> Result<Snapshot, ClientError> {
        let mut snapshots = self.snapshots();
        let mut terminal = self.terminal.clone();
        loop {
            ensure_receiver_connected(&terminal)?;
            if let Some(snapshot) = snapshots.borrow_and_update().clone() {
                match classify_installed_readiness(&terminal, &snapshot)? {
                    Readiness::Ready => {
                        ensure_receiver_connected(&terminal)?;
                        return Ok(snapshot);
                    }
                    Readiness::Pending => {}
                }
            }
            tokio::select! {
                biased;
                reason = wait_for_terminal(&mut terminal) => {
                    return Err(ClientError::Disconnected { reason });
                }
                changed = snapshots.changed() => {
                    if changed.is_err() {
                        let reason = wait_for_terminal(&mut terminal).await;
                        return Err(ClientError::Disconnected { reason });
                    }
                }
            }
        }
    }

    pub async fn command(&self, command: Command) -> Result<CommandOutcome, ClientError> {
        self.ensure_connected()?;
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
    ) -> Result<CommandOutcome, ClientError> {
        self.command(Command::Restart {
            participant,
            expected_producer: Some(expected_producer),
        })
        .await
    }

    /// End the execution, fenced on the newest installed snapshot revision.
    pub async fn stop(&self) -> Result<CommandOutcome, ClientError> {
        self.command(Command::Stop {
            expected_revision: self.revision()?,
        })
        .await
    }

    pub async fn logs(
        &self,
        participant_id: Option<String>,
        limit: u32,
        before_sequence: Option<u64>,
    ) -> Result<supervisor::logs::Snapshot, ClientError> {
        self.ensure_connected()?;
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
    ) -> Result<StreamReceiver<supervisor::endpoint::logs::FollowEndpoint>, ClientError> {
        self.stream_receiver(supervisor::topic::client().logs().follow())
            .await
    }

    pub async fn telemetry(
        &self,
        participant_id: Option<String>,
        limit: u32,
        before_sequence: Option<u64>,
    ) -> Result<supervisor::telemetry::Snapshot, ClientError> {
        self.ensure_connected()?;
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
    ) -> Result<StreamReceiver<supervisor::endpoint::telemetry::FollowEndpoint>, ClientError> {
        self.stream_receiver(supervisor::topic::client().telemetry().follow())
            .await
    }

    fn revision(&self) -> Result<u64, ClientError> {
        self.ensure_connected()?;
        self.snapshots
            .borrow()
            .as_ref()
            .map(|snapshot| snapshot.revision)
            .ok_or(ClientError::NoSnapshotRevision)
    }

    fn ensure_connected(&self) -> Result<(), ClientError> {
        ensure_receiver_connected(&self.terminal)
    }
}

/// Unique owner of one connection and all work derived from it.
///
/// `Connection` is intentionally not `Clone`. Dropping it requests close;
/// [`Connection::close`] additionally waits for deterministic close evidence.
pub struct Connection {
    endpoint: String,
    client: Client,
    close: Option<oneshot::Sender<()>>,
    terminal: watch::Sender<Option<DisconnectReason>>,
    lifecycle: Option<JoinHandle<Result<(), CloseError>>>,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Connection")
            .field("endpoint", &self.endpoint)
            .field("execution", &self.client.execution())
            .finish_non_exhaustive()
    }
}

impl Connection {
    /// Resolve one endpoint to one execution, complete the frozen compatibility
    /// bootstrap, and establish the current protocol connection.
    pub async fn connect(options: &ConnectOptions) -> Result<Self, ConnectError> {
        let executions = BusOwner::probe_routers(&options.endpoint).await?;
        let execution = exactly_one_execution(&options.endpoint, &executions)?;
        let label = SourceLabel::new(options.label.clone())?;
        let (owner, bus) = BusOwner::open(BusConfig::for_external(
            execution,
            Some(label),
            vec![options.endpoint.clone()],
        ))
        .await?;

        let initialized = match initialize(&bus, execution).await {
            Ok(initialized) => initialized,
            Err(error) => {
                let _ = owner.close().await;
                return Err(error);
            }
        };

        let Initialized {
            connected,
            snapshots,
            terminal,
            terminal_tx,
            identity,
            tasks,
            command,
            logs,
            telemetry,
        } = initialized;
        let client = Client {
            connected,
            bus,
            snapshots,
            terminal: terminal.clone(),
            command,
            logs,
            telemetry,
        };
        let (close_tx, close_rx) = oneshot::channel();
        let lifecycle_bus = client.bus.clone();
        let lifecycle = tokio::spawn(run_lifecycle(
            owner,
            lifecycle_bus,
            identity,
            tasks,
            terminal,
            terminal_tx.clone(),
            close_rx,
        ));

        Ok(Self {
            endpoint: options.endpoint.clone(),
            client,
            close: Some(close_tx),
            terminal: terminal_tx,
            lifecycle: Some(lifecycle),
        })
    }

    #[must_use]
    pub fn client(&self) -> Client {
        self.client.clone()
    }

    #[must_use]
    pub fn connected(&self) -> &Connected {
        self.client.connected()
    }

    #[must_use]
    pub fn execution(&self) -> ExecutionId {
        self.client.execution()
    }

    /// Close the connection and wait for its private lifecycle task to flush
    /// and join the transport.
    pub async fn close(mut self) -> Result<(), CloseError> {
        request_close(&self.terminal, &mut self.close);
        let Some(lifecycle) = self.lifecycle.take() else {
            return Ok(());
        };
        lifecycle.await.map_err(|error| CloseError::Lifecycle {
            detail: error.to_string(),
        })?
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        request_close(&self.terminal, &mut self.close);
    }
}

struct Initialized {
    connected: Arc<Connected>,
    snapshots: watch::Receiver<Option<Snapshot>>,
    terminal: watch::Receiver<Option<DisconnectReason>>,
    terminal_tx: watch::Sender<Option<DisconnectReason>>,
    identity: KeyLivelinessObserver,
    tasks: JoinSet<SnapshotPumpExit>,
    command: Querier<supervisor::command::Request, supervisor::command::Reply>,
    logs: Querier<supervisor::logs::SnapshotRequest, supervisor::logs::Snapshot>,
    telemetry: Querier<supervisor::telemetry::SnapshotRequest, supervisor::telemetry::Snapshot>,
}

async fn initialize(bus: &BusHandle, execution: ExecutionId) -> Result<Initialized, ConnectError> {
    let framework = remote_framework(bus).await?;
    ensure_compatible_framework(framework, FrameworkVersion::CURRENT)?;

    let stream = StreamReceiver::new(bus, &supervisor::topic::client().snapshot().topic()).await?;
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

    let (terminal_tx, terminal) = watch::channel(None);
    let on_change = terminal_tx.clone();
    let identity = bus
        .observe_liveliness_key(PRESENCE_KEY, move |status| {
            if status == LivelinessStatus::Lost {
                latch_terminal(&on_change, DisconnectReason::SupervisorIdentityLost);
            }
        })
        .await?;
    if identity.initial() == LivelinessStatus::Lost {
        return Err(ConnectError::SupervisorUnavailable);
    }

    Ok(Initialized {
        connected: Arc::new(Connected {
            execution,
            robot: info.robot,
            clock: info.clock,
            manual_drive: info.manual_drive,
            framework,
        }),
        snapshots,
        terminal,
        terminal_tx,
        identity,
        tasks,
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
    })
}

async fn remote_framework(bus: &BusHandle) -> Result<FrameworkVersion, ConnectError> {
    let reply = Querier::new(
        bus.clone(),
        &supervisor::topic::client().connect().topic(),
        DEFAULT_QUERY_TIMEOUT,
    )?
    .query(ConnectRequest::V0 {})
    .await
    .map_err(|error| match error {
        QueryError::Decode(detail) => ConnectError::UnreadableBootstrap { detail },
        other => ConnectError::Query(other),
    })?;
    let ConnectReply::V0 { framework } = reply;
    Ok(framework)
}

pub(crate) fn ensure_compatible_framework(
    remote: FrameworkVersion,
    client: FrameworkVersion,
) -> Result<(), ConnectError> {
    if remote.is_compatible_with(client) {
        return Ok(());
    }
    let refusal = if version_key(remote) > version_key(client) {
        CompatibilityRefusal::RemoteNewer
    } else {
        CompatibilityRefusal::ClientNewer
    };
    Err(ConnectError::IncompatibleFramework {
        remote,
        client,
        refusal,
    })
}

const fn version_key(version: FrameworkVersion) -> (u16, u16, u16) {
    (version.major(), version.minor(), version.patch())
}

async fn run_lifecycle(
    owner: BusOwner,
    bus: BusHandle,
    identity: KeyLivelinessObserver,
    mut tasks: JoinSet<SnapshotPumpExit>,
    mut terminal: watch::Receiver<Option<DisconnectReason>>,
    terminal_tx: watch::Sender<Option<DisconnectReason>>,
    close: oneshot::Receiver<()>,
) -> Result<(), CloseError> {
    let reason = wait_for_shutdown(&mut terminal, close, &mut tasks, bus.wait_for_fatal()).await;
    latch_terminal(&terminal_tx, reason);
    tasks.abort_all();
    drop(identity);
    let report = owner.close().await;
    tasks.shutdown().await;
    if report.is_clean() {
        Ok(())
    } else {
        Err(CloseError::Transport {
            detail: report.to_string(),
        })
    }
}

fn request_close(
    terminal: &watch::Sender<Option<DisconnectReason>>,
    close: &mut Option<oneshot::Sender<()>>,
) {
    latch_terminal(terminal, DisconnectReason::ConnectionClosed);
    if let Some(close) = close.take() {
        let _ = close.send(());
    }
}

fn latch_terminal(terminal: &watch::Sender<Option<DisconnectReason>>, reason: DisconnectReason) {
    terminal.send_if_modified(|current| {
        if current.is_some() {
            false
        } else {
            *current = Some(reason);
            true
        }
    });
}

fn terminal_reason(
    terminal: &watch::Receiver<Option<DisconnectReason>>,
) -> Option<DisconnectReason> {
    terminal.borrow().clone().or_else(|| {
        terminal
            .has_changed()
            .is_err()
            .then_some(DisconnectReason::LifecycleEnded)
    })
}

fn ensure_receiver_connected(
    terminal: &watch::Receiver<Option<DisconnectReason>>,
) -> Result<(), ClientError> {
    match terminal_reason(terminal) {
        Some(reason) => Err(ClientError::Disconnected { reason }),
        None => Ok(()),
    }
}

async fn wait_for_shutdown<F>(
    terminal: &mut watch::Receiver<Option<DisconnectReason>>,
    close: oneshot::Receiver<()>,
    tasks: &mut JoinSet<SnapshotPumpExit>,
    fatal: F,
) -> DisconnectReason
where
    F: Future<Output = BusFault>,
{
    if let Some(reason) = terminal_reason(terminal) {
        return reason;
    }
    tokio::pin!(fatal);
    tokio::select! {
        biased;
        reason = wait_for_terminal(terminal) => reason,
        fault = &mut fatal => DisconnectReason::TransportFault { fault },
        result = tasks.join_next() => snapshot_task_reason(result),
        _ = close => DisconnectReason::ConnectionClosed,
    }
}

async fn wait_for_terminal(
    terminal: &mut watch::Receiver<Option<DisconnectReason>>,
) -> DisconnectReason {
    loop {
        if let Some(reason) = terminal_reason(terminal) {
            return reason;
        }
        if terminal.changed().await.is_err() {
            return DisconnectReason::LifecycleEnded;
        }
    }
}

fn snapshot_task_reason(
    result: Option<Result<SnapshotPumpExit, tokio::task::JoinError>>,
) -> DisconnectReason {
    match result {
        Some(Ok(SnapshotPumpExit::StreamFailed { detail })) => {
            DisconnectReason::SnapshotStreamFailed { detail }
        }
        Some(Ok(SnapshotPumpExit::ObserversDropped)) => DisconnectReason::ConnectionClosed,
        Some(Err(error)) => DisconnectReason::SnapshotStreamFailed {
            detail: format!("snapshot pump task failed: {error}"),
        },
        None => DisconnectReason::SnapshotStreamFailed {
            detail: "snapshot pump task was absent".to_string(),
        },
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Readiness {
    Ready,
    Pending,
}

fn classify_readiness(snapshot: &Snapshot) -> Result<Readiness, ClientError> {
    match snapshot.lifecycle {
        Lifecycle::Ready | Lifecycle::Degraded => Ok(Readiness::Ready),
        Lifecycle::Failed => Err(ClientError::ReadinessFailed {
            failure: snapshot.failure.clone(),
        }),
        Lifecycle::Stopping | Lifecycle::Stopped => Err(ClientError::StoppedBeforeReady),
        Lifecycle::Starting => Ok(Readiness::Pending),
    }
}

fn classify_installed_readiness(
    terminal: &watch::Receiver<Option<DisconnectReason>>,
    snapshot: &Snapshot,
) -> Result<Readiness, ClientError> {
    ensure_receiver_connected(terminal)?;
    classify_readiness(snapshot)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SnapshotPumpDecision {
    Continue,
    ObserversDropped,
}

fn pump_snapshot_step(
    sender: &watch::Sender<Option<Snapshot>>,
    revision: &mut u64,
    snapshot: Snapshot,
) -> SnapshotPumpDecision {
    if snapshot.revision <= *revision {
        return SnapshotPumpDecision::Continue;
    }
    let installed_revision = snapshot.revision;
    if sender.send(Some(snapshot)).is_err() {
        return SnapshotPumpDecision::ObserversDropped;
    }
    *revision = installed_revision;
    SnapshotPumpDecision::Continue
}

#[derive(Debug, Eq, PartialEq)]
enum SnapshotPumpExit {
    StreamFailed { detail: String },
    ObserversDropped,
}

async fn pump_snapshots(
    stream: StreamReceiver<supervisor::endpoint::snapshot::TopicEndpoint>,
    sender: watch::Sender<Option<Snapshot>>,
    mut revision: u64,
) -> SnapshotPumpExit {
    loop {
        let observed = match stream.recv().await {
            Ok(observed) => observed,
            Err(error) => {
                return SnapshotPumpExit::StreamFailed {
                    detail: error.to_string(),
                };
            }
        };
        let SnapshotDocument::V0(snapshot) = observed.body;
        if pump_snapshot_step(&sender, &mut revision, snapshot)
            == SnapshotPumpDecision::ObserversDropped
        {
            return SnapshotPumpExit::ObserversDropped;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_protocol::supervisor::execution::{
        Lifecycle, SupervisorFailure, SupervisorFailureReason,
    };

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
                FrameworkVersion::new(0, 60, 1),
                FrameworkVersion::new(0, 60, 9),
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
    fn ready_and_degraded_snapshots_succeed() {
        for lifecycle in [Lifecycle::Ready, Lifecycle::Degraded] {
            assert_eq!(
                classify_readiness(&snapshot(1, lifecycle)).expect("ready lifecycle"),
                Readiness::Ready
            );
        }
    }

    #[test]
    fn failed_readiness_preserves_structured_failure() {
        let failure = SupervisorFailure::new(
            SupervisorFailureReason::LaunchFailed,
            "participant launch plan was rejected",
        );
        let mut failed = snapshot(1, Lifecycle::Failed);
        failed.failure = Some(failure.clone());

        assert!(matches!(
            classify_readiness(&failed),
            Err(ClientError::ReadinessFailed { failure: Some(actual) })
                if actual.reason == failure.reason && actual.detail == failure.detail
        ));
    }

    #[test]
    fn snapshot_pump_installs_only_strictly_newer_revisions() {
        let (sender, receiver) = watch::channel(Some(snapshot(7, Lifecycle::Starting)));
        let mut revision = 7;

        assert_eq!(
            pump_snapshot_step(&sender, &mut revision, snapshot(8, Lifecycle::Ready)),
            SnapshotPumpDecision::Continue
        );
        for observed in [8, 6] {
            assert_eq!(
                pump_snapshot_step(
                    &sender,
                    &mut revision,
                    snapshot(observed, Lifecycle::Starting),
                ),
                SnapshotPumpDecision::Continue
            );
        }
        assert_eq!(revision, 8);
        assert_eq!(
            receiver.borrow().as_ref().map(|value| value.revision),
            Some(8)
        );
    }

    #[tokio::test]
    async fn explicit_close_is_latched_before_the_lifecycle_is_woken() {
        let (terminal_tx, mut terminal) = watch::channel(None);
        let (close_tx, close_rx) = oneshot::channel();
        let mut tasks = JoinSet::new();
        tasks.spawn(std::future::pending::<SnapshotPumpExit>());
        let mut close = Some(close_tx);
        request_close(&terminal_tx, &mut close);

        assert_eq!(
            terminal_reason(&terminal),
            Some(DisconnectReason::ConnectionClosed)
        );

        assert_eq!(
            wait_for_shutdown(&mut terminal, close_rx, &mut tasks, std::future::pending(),).await,
            DisconnectReason::ConnectionClosed
        );
    }

    #[tokio::test]
    async fn identity_loss_is_latched_and_not_overwritten_by_close() {
        let (terminal_tx, mut terminal) = watch::channel(None);
        let (_close_tx, close_rx) = oneshot::channel();
        let mut tasks = JoinSet::new();
        tasks.spawn(std::future::pending::<SnapshotPumpExit>());
        latch_terminal(&terminal_tx, DisconnectReason::SupervisorIdentityLost);
        latch_terminal(&terminal_tx, DisconnectReason::ConnectionClosed);

        assert_eq!(
            wait_for_shutdown(&mut terminal, close_rx, &mut tasks, std::future::pending(),).await,
            DisconnectReason::SupervisorIdentityLost
        );
    }

    #[tokio::test]
    async fn snapshot_stream_failure_preserves_its_detail() {
        let (_terminal_tx, mut terminal) = watch::channel(None);
        let (_close_tx, close_rx) = oneshot::channel();
        let mut tasks = JoinSet::new();
        tasks.spawn(async {
            SnapshotPumpExit::StreamFailed {
                detail: "subscriber closed".to_string(),
            }
        });

        assert_eq!(
            wait_for_shutdown(&mut terminal, close_rx, &mut tasks, std::future::pending(),).await,
            DisconnectReason::SnapshotStreamFailed {
                detail: "subscriber closed".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn transport_fault_is_a_terminal_lifecycle_reason() {
        let (_terminal_tx, mut terminal) = watch::channel(None);
        let (_close_tx, close_rx) = oneshot::channel();
        let mut tasks = JoinSet::new();
        tasks.spawn(std::future::pending::<SnapshotPumpExit>());
        let fault = BusFault::WorkerExited {
            worker: "outbound-drain".to_string(),
        };

        assert_eq!(
            wait_for_shutdown(&mut terminal, close_rx, &mut tasks, {
                let fault = fault.clone();
                async move { fault }
            })
            .await,
            DisconnectReason::TransportFault { fault }
        );
    }

    #[test]
    fn terminal_state_preempts_a_cached_ready_snapshot() {
        let (terminal_tx, terminal) = watch::channel(None);
        latch_terminal(&terminal_tx, DisconnectReason::SupervisorIdentityLost);

        assert!(matches!(
            classify_installed_readiness(&terminal, &snapshot(1, Lifecycle::Ready)),
            Err(ClientError::Disconnected {
                reason: DisconnectReason::SupervisorIdentityLost,
            })
        ));
    }
}
