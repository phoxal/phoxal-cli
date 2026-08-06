//! # phoxal-supervisor-client
//!
//! Attachment and session behavior for the Phoxal supervisor contract, over
//! `phoxal-bus`. This is the reusable client the TUI and the future Operator
//! share; it owns no rendering, no input devices, and no project knowledge.
//!
//! # The attachment state machine
//!
//! ```text
//!            ┌──────────────┐
//!            │  Unattached  │
//!            └──────┬───────┘
//!                   │ open(endpoint)
//!                   ▼
//!   1. probe the endpoint for directly connected routers   ── zero or many ──┐
//!   2. exactly one router, converted to its ExecutionId    ── not an id  ────┤
//!   3. open a bus session rooted at phoxal/{execution}                       │
//!   4. query supervisor/connect                            ── no responder ──┤
//!   5. validate api + every consumed client-visible schema ── mismatch ──────┤
//!   6. subscribe supervisor/snapshot                                         │
//!   7. query supervisor/snapshot/current, keep the highest revision          │
//!   8. watch the supervisor/identity liveliness token                        │
//!                   │                                                        │
//!                   ▼                                                        ▼
//!            ┌──────────────┐   token lost    ┌───────────────┐      ┌──────────────┐
//!            │   Attached   │────────────────▶│  Disconnected │      │    Failed    │
//!            └──────────────┘                 └───────┬───────┘      └──────────────┘
//!                                                     │ open() again
//!                                                     ▼
//!                             same execution → a reconnection to the same run
//!                             new execution  → a NEW attachment, surfaced as such
//! ```
//!
//! Steps 1-2 are the whole of "direct attachment" for organization#978: an
//! endpoint means one robot-owned router. Selecting among several behind a
//! shared fabric is organization#989.
//!
//! Reconnection is not a resumption. There is no session id, no resume cursor,
//! and no partial re-handshake: [`Attachment::open`] runs the same eight steps
//! from the top. If the execution that answers is a different one, the caller
//! is told so through [`AttachError::ExecutionChanged`] (or by reading
//! [`Attachment::execution`]) and must treat it as a new run - a fresh daemon
//! has a fresh key space, and nothing from the previous attachment carries
//! over.

pub mod bundle;
pub mod compat;
pub mod error;
pub mod fabric;
pub mod router;
pub mod snapshot;

pub use bundle::check_path;
pub use compat::Expectations;
pub use error::AttachError;
pub use fabric::{IdentityWatch, SupervisorFabric};
pub use router::{RouterId, exactly_one_execution};
pub use snapshot::{SnapshotFeed, SnapshotTracker};

use std::sync::Arc;

use phoxal_bus::{
    AskQuery, Bus, BusConfig, DEFAULT_QUERY_TIMEOUT, Querier, Subscribe, Subscriber, Topic,
};
use phoxal_runtime_contract::{ExecutionId, ProducerId};
use phoxal_supervisor_api::{
    BundleGetOutcome, Command, CommandOutcome, ExecutionMode, Name, ProcessKey, RobotIdentity,
    Snapshot, identity_key, supervisor,
};
use tokio::sync::watch;
use tokio::task::JoinSet;

/// Inbound ring depth for the snapshot stream. A snapshot is complete state, so
/// a backlog is never useful; the depth exists only to absorb a burst while the
/// pump task is between polls.
const SNAPSHOT_DEPTH: usize = 8;

/// Inbound ring depth for a follow stream, where every record matters.
const FOLLOW_DEPTH: usize = 1024;

/// How to attach.
#[derive(Clone, Debug)]
pub struct AttachmentConfig {
    /// The robot-owned Zenoh endpoint - locally the daemon's
    /// `.phoxal/run/supervisor.sock`.
    pub endpoint: String,
    /// This client's diagnostic label in bus metadata. Never identity.
    pub participant: String,
    /// The surfaces this client will open, and therefore validate.
    pub expectations: Expectations,
}

impl AttachmentConfig {
    /// A full session (every surface) at `endpoint`.
    #[must_use]
    pub fn new(endpoint: impl Into<String>, participant: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            participant: participant.into(),
            expectations: Expectations::full(),
        }
    }

    /// Validate only the named surfaces.
    #[must_use]
    pub fn expecting(mut self, expectations: Expectations) -> Self {
        self.expectations = expectations;
        self
    }
}

/// What the supervisor told this client at connect. Fixed for the attachment's
/// life: a change means a new execution, which is a new attachment.
#[derive(Clone, Debug)]
pub struct Connected {
    pub execution: ExecutionId,
    pub robot: RobotIdentity,
    pub mode: ExecutionMode,
}

/// The outcome of a re-handshake after a disconnection.
///
/// A daemon restart mints a fresh execution and therefore a fresh key space, so
/// "the endpoint answered again" is not the same fact as "the run I was
/// watching is back". This type refuses to let a caller conflate them.
#[derive(Debug)]
pub enum Reattachment {
    /// The same execution answered: the run continued and the previous
    /// snapshot revisions are still comparable.
    Resumed(Attachment),
    /// A different execution answered. Everything from the previous attachment
    /// - revisions, producers, process rows - belongs to a run that has ended.
    NewExecution {
        previous: ExecutionId,
        attachment: Attachment,
    },
}

impl Reattachment {
    /// Take the attachment only if it is the same run, and report the change
    /// otherwise.
    ///
    /// # Errors
    ///
    /// [`AttachError::ExecutionChanged`] when a different execution answered.
    pub fn require_same_execution(self) -> Result<Attachment, AttachError> {
        match self {
            Self::Resumed(attachment) => Ok(attachment),
            Self::NewExecution {
                previous,
                attachment,
            } => Err(AttachError::ExecutionChanged {
                expected: previous,
                found: attachment.execution(),
            }),
        }
    }

    /// The attachment, whichever execution answered.
    #[must_use]
    pub fn into_attachment(self) -> Attachment {
        match self {
            Self::Resumed(attachment) | Self::NewExecution { attachment, .. } => attachment,
        }
    }
}

/// A live attachment to one supervisor execution.
///
/// Dropping it detaches: every owned task is aborted and the session closes.
/// The daemon is unaffected - detaching is not stopping.
pub struct Attachment {
    endpoint: String,
    connected: Connected,
    bus: Bus,
    snapshots: watch::Receiver<Option<Snapshot>>,
    disconnected: watch::Receiver<bool>,
    command: Querier<supervisor::command::Request, supervisor::command::Reply>,
    bundle: Querier<supervisor::bundle::GetRequest, supervisor::bundle::GetReply>,
    logs: Querier<supervisor::logs::SnapshotRequest, supervisor::logs::Snapshot>,
    telemetry: Querier<supervisor::telemetry::SnapshotRequest, supervisor::telemetry::Snapshot>,
    /// Aborted on drop, which is what makes detaching a drop.
    _tasks: JoinSet<()>,
}

impl std::fmt::Debug for Attachment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Attachment")
            .field("endpoint", &self.endpoint)
            .field("execution", &self.connected.execution)
            .finish_non_exhaustive()
    }
}

impl Attachment {
    /// Run the whole handshake and return a live attachment.
    ///
    /// # Errors
    ///
    /// Any step of the state machine in this crate's docs: no or several
    /// routers, a router that is not an execution, an unreachable or
    /// incompatible supervisor, or a transport failure.
    pub async fn open(
        config: &AttachmentConfig,
        fabric: Arc<dyn SupervisorFabric>,
    ) -> Result<Self, AttachError> {
        let endpoint = config.endpoint.as_str();

        // 1-2. The endpoint names exactly one execution, and the execution is
        // the router: nothing is looked up, the identity is read off the link.
        let routers = fabric.directly_connected_routers(endpoint).await?;
        let execution = exactly_one_execution(endpoint, &routers)?;

        // 3. Only now can a session be opened: the key root IS the execution.
        let bus = Bus::open(BusConfig {
            execution,
            participant: config.participant.clone(),
            connect_endpoints: vec![endpoint.to_string()],
        })
        .await?;

        // 4-5. Compatibility is settled here and nowhere else.
        let connect = Querier::new(
            bus.clone(),
            &supervisor::topic::client().connect().topic(),
            DEFAULT_QUERY_TIMEOUT,
        )?;
        let supervisor::connect::Reply::V0 {
            robot,
            api,
            schemas,
            mode,
        } = connect.query(supervisor::connect::Request::V0 {}).await?;
        config.expectations.validate(endpoint, &api, &schemas)?;

        // 6. Subscribe BEFORE querying, so no revision can be published into
        // the gap between the two.
        let stream: Subscriber<supervisor::snapshot::Update> = Subscriber::new(
            &bus,
            &supervisor::topic::client().snapshot().topic(),
            SNAPSHOT_DEPTH,
        )
        .await?;

        // 7. Then take the current document and install whichever is highest.
        let current = Querier::new(
            bus.clone(),
            &supervisor::topic::client().snapshot().current(),
            DEFAULT_QUERY_TIMEOUT,
        )?;
        let supervisor::snapshot::Current::V0(installed) = current
            .query(supervisor::snapshot::CurrentRequest::V0 {})
            .await?;

        let mut feed = SnapshotFeed::new();
        feed.accept(installed);
        let snapshots = feed.subscribe();

        let mut tasks = JoinSet::new();
        tasks.spawn(pump_snapshots(stream, feed));

        // 8. Token loss is the disconnection signal.
        let watch_handle = fabric
            .watch_identity(&bus, &identity_key(execution))
            .await?;
        let (disconnected_tx, disconnected) = watch::channel(false);
        tasks.spawn(watch_identity(watch_handle, disconnected_tx));

        Ok(Self {
            endpoint: endpoint.to_string(),
            connected: Connected {
                execution,
                robot,
                mode,
            },
            command: Querier::new(
                bus.clone(),
                &supervisor::topic::client().command().topic(),
                DEFAULT_QUERY_TIMEOUT,
            )?,
            bundle: Querier::new(
                bus.clone(),
                &supervisor::topic::client().bundle().get(),
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
            _tasks: tasks,
        })
    }

    /// Re-run the whole handshake after a disconnection.
    ///
    /// There is nothing to resume: no session id, no cursor, no partial
    /// re-handshake. What this adds over [`Attachment::open`] is the one
    /// judgment a reconnecting caller must not skip - whether the execution
    /// that answered is the one it left.
    ///
    /// # Errors
    ///
    /// As [`Attachment::open`].
    pub async fn reattach(
        config: &AttachmentConfig,
        fabric: Arc<dyn SupervisorFabric>,
        previous: ExecutionId,
    ) -> Result<Reattachment, AttachError> {
        let attachment = Self::open(config, fabric).await?;
        if attachment.execution() == previous {
            Ok(Reattachment::Resumed(attachment))
        } else {
            Ok(Reattachment::NewExecution {
                previous,
                attachment,
            })
        }
    }

    /// What connect established.
    #[must_use]
    pub const fn connected(&self) -> &Connected {
        &self.connected
    }

    /// The execution this attachment is bound to. A different value on a later
    /// attachment is a different run.
    #[must_use]
    pub const fn execution(&self) -> ExecutionId {
        self.connected.execution
    }

    /// The underlying session, for a caller that also opens robot-domain
    /// topics on the same execution.
    #[must_use]
    pub const fn bus(&self) -> &Bus {
        &self.bus
    }

    /// Observe the highest-revision snapshot. Cancellation-safe.
    #[must_use]
    pub fn snapshots(&self) -> watch::Receiver<Option<Snapshot>> {
        self.snapshots.clone()
    }

    /// The most recent snapshot, without awaiting.
    #[must_use]
    pub fn snapshot(&self) -> Option<Snapshot> {
        self.snapshots.borrow().clone()
    }

    /// Whether the supervisor's identity token has been lost.
    #[must_use]
    pub fn is_disconnected(&self) -> bool {
        *self.disconnected.borrow()
    }

    /// Resolve when the supervisor's identity token is lost.
    ///
    /// Cancellation-safe: `watch::Receiver::changed` consumes nothing when its
    /// `select!` arm loses.
    pub async fn disconnected(&self) {
        let mut receiver = self.disconnected.clone();
        while !*receiver.borrow_and_update() {
            if receiver.changed().await.is_err() {
                // The watcher went away, which is itself loss of evidence.
                return;
            }
        }
    }

    /// Send a command and get the supervisor's typed outcome.
    ///
    /// # Errors
    ///
    /// A transport or query failure. A *rejected* command is not an error - it
    /// is a [`CommandOutcome::Rejected`] with a reason the caller renders.
    pub async fn command(&self, command: Command) -> Result<CommandOutcome, AttachError> {
        let supervisor::command::Reply::V0 { outcome } = self
            .command
            .query(supervisor::command::Request::V0 { command })
            .await?;
        Ok(outcome)
    }

    /// Restart one process, fenced on the producer the caller last observed.
    ///
    /// The fence is not optional: pass the producer from the snapshot row that
    /// prompted the restart, and the supervisor rejects the command if that
    /// incarnation is already gone.
    ///
    /// # Errors
    ///
    /// As [`Attachment::command`].
    pub async fn restart(
        &self,
        process: ProcessKey,
        expected_producer: ProducerId,
    ) -> Result<CommandOutcome, AttachError> {
        self.command(Command::Restart {
            process,
            expected_producer,
        })
        .await
    }

    /// End the execution.
    ///
    /// # Errors
    ///
    /// As [`Attachment::command`].
    pub async fn stop(&self) -> Result<CommandOutcome, AttachError> {
        self.command(Command::Stop).await
    }

    /// Read one file from the finalized bundle.
    ///
    /// The path is checked locally first with the same rules the supervisor
    /// applies, so an obviously bad path never reaches the wire and is reported
    /// with the same reason enumeration either way.
    ///
    /// # Errors
    ///
    /// [`AttachError::InvalidBundlePath`] for a path this side already refuses,
    /// or a transport failure. A `Missing`, `InvalidPath`, or `TooLarge` answer
    /// from the supervisor is a successful outcome the caller matches on.
    pub async fn bundle_file(&self, path: &str) -> Result<BundleGetOutcome, AttachError> {
        let path = bundle::check_path(path)?;
        let supervisor::bundle::GetReply::V0 { outcome } = self
            .bundle
            .query(supervisor::bundle::GetRequest::V0 { path })
            .await?;
        Ok(outcome)
    }

    /// One backward page of the supervisor's bounded log history.
    ///
    /// # Errors
    ///
    /// A transport or query failure.
    pub async fn logs(
        &self,
        participant: Option<Name>,
        limit: u32,
        before_sequence: Option<u64>,
    ) -> Result<supervisor::logs::Snapshot, AttachError> {
        Ok(self
            .logs
            .query(supervisor::logs::SnapshotRequest::V0 {
                participant,
                limit,
                before_sequence,
            })
            .await?)
    }

    /// The live log stream that continues a [`Attachment::logs`] page.
    ///
    /// # Errors
    ///
    /// A transport failure while declaring the subscription.
    pub async fn follow_logs(&self) -> Result<Subscriber<supervisor::logs::Follow>, AttachError> {
        Ok(Subscriber::new(
            &self.bus,
            &supervisor::topic::client().logs().follow(),
            FOLLOW_DEPTH,
        )
        .await?)
    }

    /// One backward page of the supervisor's bounded runtime history.
    ///
    /// # Errors
    ///
    /// A transport or query failure.
    pub async fn telemetry(
        &self,
        participant: Option<Name>,
        limit: u32,
        before_sequence: Option<u64>,
    ) -> Result<supervisor::telemetry::Snapshot, AttachError> {
        Ok(self
            .telemetry
            .query(supervisor::telemetry::SnapshotRequest::V0 {
                participant,
                limit,
                before_sequence,
            })
            .await?)
    }

    /// The live runtime stream.
    ///
    /// # Errors
    ///
    /// A transport failure while declaring the subscription.
    pub async fn follow_telemetry(
        &self,
    ) -> Result<Subscriber<supervisor::telemetry::Follow>, AttachError> {
        Ok(Subscriber::new(
            &self.bus,
            &supervisor::topic::client().telemetry().follow(),
            FOLLOW_DEPTH,
        )
        .await?)
    }

    /// Detach: abort the owned tasks and close the session.
    ///
    /// # Errors
    ///
    /// A transport failure while closing.
    pub async fn close(mut self) -> Result<(), AttachError> {
        self._tasks.shutdown().await;
        self.bus.close().await?;
        Ok(())
    }
}

/// Install every pushed snapshot that is actually newer.
///
/// No `select!`: the only exit is the stream ending, and the task is aborted
/// with its `JoinSet` when the attachment drops, so there is nothing to race.
async fn pump_snapshots(stream: Subscriber<supervisor::snapshot::Update>, mut feed: SnapshotFeed) {
    loop {
        match stream.recv().await {
            Ok(observed) => {
                let supervisor::snapshot::Update::V0(snapshot) = observed.body;
                feed.accept(snapshot);
            }
            Err(error) => {
                tracing::debug!(
                    target: "phoxal.supervisor.client",
                    %error,
                    "the supervisor snapshot stream ended"
                );
                return;
            }
        }
    }
}

async fn watch_identity(mut watch: IdentityWatch, disconnected: watch::Sender<bool>) {
    watch.lost().await;
    let _ = disconnected.send(true);
}

/// The typed topics this client opens, exposed so a caller can assert the
/// contract it is bound to without reaching through the generated tree.
#[must_use]
pub fn connect_topic() -> Topic<AskQuery<supervisor::connect::Request, supervisor::connect::Reply>>
{
    supervisor::topic::client().connect().topic()
}

/// The snapshot stream this client subscribes before querying.
#[must_use]
pub fn snapshot_topic() -> Topic<Subscribe<supervisor::snapshot::Update>> {
    supervisor::topic::client().snapshot().topic()
}
