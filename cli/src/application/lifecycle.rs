//! The execution lifecycle commands.
//!
//! ```text
//! run     build a fresh bundle, launch its framework supervisor, attach
//! start   the same, but wait for readiness and exit
//! attach  existing execution only: no build, no mutation
//! stop    existing execution only
//! status  existing execution only
//! logs    existing execution only
//! ```
//!
//! `run` creates a fresh execution and never silently attaches to one that is
//! already live; `attach` is the explicit existing-execution path. Nothing
//! here can supervise a graph: the supervisor is a separate executable and the
//! only channel to it is the supervisor API.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use phoxal_client::supervisor::execution::{Lifecycle, Snapshot};
use phoxal_client::{ClientError, ConnectError, ConnectOptions, Connection, DisconnectReason};

use super::session::{self, Detachable};
use super::supervisor::{self, LaunchedSupervisor};
use crate::attach::{CLIENT_PARTICIPANT, Session};
use crate::cli::context::AppContext;
use crate::lock::{ProjectLock, ProjectLockIdentity, ProjectOperation};

/// How long a client waits for a freshly launched supervisor to answer connect.
const HANDSHAKE_BUDGET: Duration = Duration::from_secs(60);

/// How long `start` waits for the graph to reach readiness after it answers.
const READINESS_BUDGET: Duration = Duration::from_secs(5 * 60);

/// How often a launch is re-probed while waiting for the supervisor to come up.
const PROBE_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DriversMode {
    On,
    Off,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunOptions {
    pub(crate) drivers: DriversMode,
    pub(crate) drivers_subset: Vec<String>,
}

/// Where an execution is, and how this client names it.
#[derive(Debug, Clone)]
pub(crate) struct Target {
    /// The project or installed root the bundle lives under.
    pub(crate) project: PathBuf,
    /// The Zenoh endpoint the supervisor binds and this client dials.
    pub(crate) endpoint: String,
}

impl Target {
    /// Resolve the deterministic endpoint for a local project or installed
    /// root.
    pub(crate) fn resolve(explicit: Option<&Path>, fallback: &Path) -> Result<Self> {
        let runtime = phoxal_cli_project::resolve_target(explicit, fallback)?;
        Ok(Self {
            project: runtime.logical_root,
            endpoint: runtime.zenoh_endpoint,
        })
    }

    /// A remote or otherwise explicitly named endpoint, with no local project
    /// to resolve it from.
    pub(crate) fn at_endpoint(endpoint: String, project: PathBuf) -> Self {
        Self { project, endpoint }
    }
}

// ---------------------------------------------------------------------------
// run / start
// ---------------------------------------------------------------------------

pub(crate) async fn run_command(
    app: &AppContext,
    requested_target: Option<&Path>,
    options: RunOptions,
) -> Result<()> {
    let target = Target::resolve(requested_target, app.project.root())?;
    let launched = build_publish_and_launch(app, &target, options).await?;
    let session = await_attachment(&target, launched, HANDSHAKE_BUDGET).await?;
    let outcome = session::drive(app, &target.project, session, Detachable::Yes).await?;
    report_outcome(&target, outcome)
}

pub(crate) async fn start_command(
    app: &AppContext,
    requested_target: Option<&Path>,
    options: RunOptions,
) -> Result<()> {
    let target = Target::resolve(requested_target, app.project.root())?;
    let launched = build_publish_and_launch(app, &target, options).await?;
    let session = await_attachment(&target, launched, HANDSHAKE_BUDGET).await?;
    await_readiness(&session, READINESS_BUDGET).await?;
    let display = target.project.display();
    session.shutdown().await;
    app.ui.info(format!(
        "robot instance ready; attach with `phoxal attach {display}` or stop with `phoxal stop {display}`"
    ));
    Ok(())
}

/// The shared front half of `run` and `start`.
///
/// The framework train selects and materializes the supervisor. The project
/// selects the framework it builds against, and the installed CLI product
/// version says nothing about that, so a product-version difference is not a
/// reason to refuse a robot's work. Launch resolves the supervisor staged with
/// the robot release, and a release without one fails there, naming the binary
/// it could not run.
///
/// The live check is a real handshake, not a socket-file probe: an execution
/// that answers connect is live, and `run` refuses rather than silently
/// attaching to it. Only then is the build lock taken, so a refused run never
/// blocks the running execution's own operations.
async fn build_publish_and_launch(
    app: &AppContext,
    target: &Target,
    options: RunOptions,
) -> Result<LaunchedSupervisor> {
    refuse_if_live(target).await?;
    let _lock = ProjectLock::acquire(ProjectLockIdentity::resolve(
        &target.project,
        ProjectOperation::Run,
    ))
    .context("failed to acquire the project build lock")?;
    // Re-check under the lock: the window between the probe and the lock is
    // exactly where a concurrent `phoxal run` would have started a supervisor.
    refuse_if_live(target).await?;
    // The handshake above is the friendly message; the supervisor lock is the
    // authority. A supervisor that has taken its lock but has not yet answered
    // connect is live, and this is what closes that startup window.
    crate::lock::refuse_while_execution_is_live(&target.project)?;

    let runtime_target =
        phoxal_cli_project::resolve_target(Some(&target.project), &target.project)?;
    let (reporter, signal_task) =
        crate::cli::output::progress::cancellable_preparation_reporter(app.ui);
    let offline = app.offline;
    let prepared = tokio::task::spawn_blocking(move || {
        phoxal_cli_project::prepare_run(phoxal_cli_project::PrepareRunRequest {
            target: runtime_target,
            drivers: phoxal_cli_project::DriverRequest {
                mode: match options.drivers {
                    DriversMode::On => phoxal_cli_project::DriverMode::On,
                    DriversMode::Off => phoxal_cli_project::DriverMode::Off,
                },
                subset: options.drivers_subset,
            },
            offline,
            reporter,
        })
    })
    .await?;
    signal_task.abort();
    let prepared = prepared?;

    app.ui.info(format!(
        "launching the deployment release at {}",
        prepared.release.root.display()
    ));
    supervisor::spawn(&prepared.release)
}

/// Fail with the commands that actually apply when an execution already
/// answers at this endpoint.
async fn refuse_if_live(target: &Target) -> Result<()> {
    if probe(&target.endpoint).await?.is_none() {
        return Ok(());
    }
    bail!(already_live_message(target))
}

/// The refusal `run` earns when an execution already answers.
///
/// It names both commands that actually apply. Silently attaching would be the
/// one behavior `run` must never have: the operator asked for a fresh
/// execution of the code they just changed.
fn already_live_message(target: &Target) -> String {
    let display = target.project.display();
    format!(
        "an execution is already live at {} - `run` always creates a fresh one and never attaches \
         to an existing execution. Attach to it with `phoxal attach {display}`, or end it with \
         `phoxal stop {display}` and run again",
        target.endpoint
    )
}

/// Whether an execution answers connect at `endpoint`.
///
/// A completed handshake is the readiness signal. The lock file's presence and
/// the socket file's existence both survive a killed supervisor; a reply does not.
///
/// A robot that answered and disagreed about the framework is emphatically not
/// "no execution here": reading it as one would start a second supervisor beside a
/// robot that is already running.
async fn probe(endpoint: &str) -> Result<Option<Connection>> {
    match Connection::connect(&ConnectOptions::new(endpoint, CLIENT_PARTICIPANT)).await {
        Ok(connection) => Ok(Some(connection)),
        Err(error) if classify_connect_error(&error) == ConnectFailure::RetryableAbsence => {
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

/// Whether an opening failure means no execution has appeared yet or is a
/// concrete failure that must keep its structured evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectFailure {
    RetryableAbsence,
    Fatal,
}

const fn classify_connect_error(error: &ConnectError) -> ConnectFailure {
    match error {
        ConnectError::NoExecution { .. } => ConnectFailure::RetryableAbsence,
        ConnectError::MultipleExecutions { .. }
        | ConnectError::SourceLabel(_)
        | ConnectError::IncompatibleFramework { .. }
        | ConnectError::UnreadableBootstrap { .. }
        | ConnectError::SupervisorUnavailable
        | ConnectError::Snapshot(_)
        | ConnectError::Bus(_)
        | ConnectError::Query(_) => ConnectFailure::Fatal,
    }
}

fn classify_connect_failure(error: &anyhow::Error) -> ConnectFailure {
    error
        .downcast_ref::<ConnectError>()
        .map_or(ConnectFailure::Fatal, classify_connect_error)
}

/// Watch the launched supervisor until it answers connect, or until it exits.
///
/// Before the handshake completes there is nothing but process facts and
/// stderr to go on, so an early exit is reported with the supervisor's own
/// diagnostics rather than as a timeout.
async fn await_attachment(
    target: &Target,
    mut launched: LaunchedSupervisor,
    budget: Duration,
) -> Result<Session> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if let Some(status) = launched.exited()? {
            bail!(launched.early_exit_message(status));
        }
        match Session::open(&target.endpoint, target.project.display().to_string()).await {
            Ok(session) => return Ok(session),
            Err(error) => {
                // Only "nothing announced yet" is startup latency. Once an
                // execution was discovered, ambiguity, identity loss, invalid
                // state, or a protocol/transport failure is already an answer.
                if classify_connect_failure(&error) == ConnectFailure::Fatal {
                    return Err(error);
                }
                if tokio::time::Instant::now() >= deadline {
                    if let Some(status) = launched.exited()? {
                        bail!(launched.early_exit_message(status));
                    }
                    let diagnostics = launched.diagnostics();
                    let diagnostics = diagnostics.trim();
                    let hint = if diagnostics.is_empty() {
                        String::new()
                    } else {
                        format!("; phoxal-supervisor reported: {diagnostics}")
                    };
                    return Err(error).with_context(|| {
                        format!(
                            "timed out after {}s waiting for phoxal-supervisor to answer at {}{hint}",
                            budget.as_secs(),
                            target.endpoint
                        )
                    });
                }
                tokio::time::sleep(PROBE_INTERVAL).await;
            }
        }
    }
}

/// Wait for the graph to reach readiness, or fail with the reason it did not.
async fn await_readiness(session: &Session, budget: Duration) -> Result<()> {
    tokio::time::timeout(budget, session.wait_ready())
        .await
        .with_context(|| {
            format!(
                "timed out after {}s waiting for the graph to become ready",
                budget.as_secs()
            )
        })??;
    Ok(())
}

// ---------------------------------------------------------------------------
// attach / stop / status / logs
// ---------------------------------------------------------------------------

/// Resolve an existing execution, either from a local project or from an
/// explicitly named endpoint. This path never builds and never mutates.
async fn open_existing(
    app: &AppContext,
    requested_target: Option<&Path>,
    endpoint: Option<String>,
) -> Result<(Target, Session)> {
    let target = match endpoint {
        Some(endpoint) => Target::at_endpoint(
            endpoint,
            requested_target.map_or_else(|| app.project.root().to_path_buf(), Path::to_path_buf),
        ),
        None => Target::resolve(requested_target, app.project.root())?,
    };
    let session = Session::open(&target.endpoint, target.project.display().to_string())
        .await
        .map_err(|error| describe_missing_execution(error, &target))?;
    Ok((target, session))
}

/// Explain a genuinely absent execution as "nothing is running here".
///
/// Every failure after discovery already carries the authoritative account of
/// what happened; telling the operator to start another execution would bury
/// that evidence under advice for a different problem.
fn describe_missing_execution(error: anyhow::Error, target: &Target) -> anyhow::Error {
    if classify_connect_failure(&error) == ConnectFailure::Fatal {
        return error;
    }
    error.context(format!(
        "no execution answered at {}. Start one with `phoxal run {}`",
        target.endpoint,
        target.project.display()
    ))
}

pub(crate) async fn attach_command(
    app: &AppContext,
    requested_target: Option<&Path>,
    endpoint: Option<String>,
) -> Result<()> {
    let (target, session) = open_existing(app, requested_target, endpoint).await?;
    let outcome = session::drive(app, &target.project, session, Detachable::Yes).await?;
    report_outcome(&target, outcome)
}

pub(crate) async fn stop_command(
    app: &AppContext,
    requested_target: Option<&Path>,
    endpoint: Option<String>,
) -> Result<()> {
    let (target, session) = open_existing(app, requested_target, endpoint).await?;
    let outcome = match session.ports.supervisor.stop().await {
        Ok(phoxal_client::supervisor::execution::CommandOutcome::Accepted { .. }) => {
            await_terminal(&session).await
        }
        Ok(phoxal_client::supervisor::execution::CommandOutcome::Rejected { reason }) => {
            Err(anyhow!("the supervisor rejected the stop: {reason:?}"))
        }
        Err(error) => stop_command_failure(error, session.disconnect_reason()),
    };
    finish_stop(outcome, session.close().await)?;
    app.ui
        .info(format!("execution stopped at {}", target.endpoint));
    Ok(())
}

/// Interpret a command failure using the connection's structured terminal
/// evidence. A closed query or transport alone proves only that the command
/// failed; only the supervisor identity disappearing proves the execution the
/// command addressed is gone.
fn stop_command_failure(error: anyhow::Error, observed: Option<DisconnectReason>) -> Result<()> {
    let reported = error
        .downcast_ref::<ClientError>()
        .and_then(|error| match error {
            ClientError::Disconnected { reason } => Some(reason.clone()),
            _ => None,
        });
    match reported.or(observed) {
        Some(reason) => stop_disconnect_outcome(reason),
        None => Err(error),
    }
}

fn stop_disconnect_outcome(reason: DisconnectReason) -> Result<()> {
    match reason {
        DisconnectReason::SupervisorIdentityLost => Ok(()),
        reason => Err(ClientError::Disconnected { reason }.into()),
    }
}

fn stop_snapshot_outcome(snapshot: &Snapshot) -> Result<bool> {
    match snapshot.lifecycle {
        Lifecycle::Stopped => Ok(true),
        Lifecycle::Failed => bail!(failure_message(snapshot)),
        _ => Ok(false),
    }
}

fn finish_stop(outcome: Result<()>, close: Result<()>) -> Result<()> {
    match (outcome, close) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error.context(
            "the execution stopped, but the local client connection did not close cleanly",
        )),
        (Err(error), Err(close)) => Err(error.context(format!(
            "the stop failed and the local client connection also failed to close: {close:#}"
        ))),
    }
}

/// Wait for the execution to publish `Stopped`, or for its execution-scoped
/// supervisor identity to disappear. Every other terminal cause is a failure.
async fn await_terminal(session: &Session) -> Result<()> {
    let mut snapshots = session.snapshots();
    loop {
        let installed = snapshots.borrow_and_update().clone();
        if let Some(snapshot) = installed
            && stop_snapshot_outcome(&snapshot)?
        {
            return Ok(());
        }
        tokio::select! {
            reason = session.disconnected() => return stop_disconnect_outcome(reason),
            changed = snapshots.changed() => {
                if changed.is_err() {
                    return stop_disconnect_outcome(
                        session.disconnect_reason().unwrap_or(DisconnectReason::LifecycleEnded)
                    );
                }
            }
        }
    }
}

pub(crate) async fn status_command(
    app: &AppContext,
    requested_target: Option<&Path>,
    endpoint: Option<String>,
) -> Result<()> {
    let (_, session) = open_existing(app, requested_target, endpoint).await?;
    let snapshot = session
        .snapshot()
        .context("the supervisor answered connect but published no snapshot")?;
    for line in render_status(&snapshot, session.connected()) {
        println!("{line}");
    }
    session.shutdown().await;
    Ok(())
}

/// Render one snapshot as the plain status report.
///
/// It is a pure function of the snapshot on purpose: the whole status surface
/// is the authoritative document, so there is nothing to fetch and nothing to
/// derive from local state.
pub(crate) fn render_status(
    snapshot: &Snapshot,
    connected: &phoxal_client::Connected,
) -> Vec<String> {
    let mut lines = vec![
        format!("robot:     {}", connected.robot),
        format!("clock:     {:?}", connected.clock).to_lowercase(),
        format!("lifecycle: {:?}", snapshot.lifecycle).to_lowercase(),
        format!("revision:  {}", snapshot.revision),
    ];
    if let Some(failure) = &snapshot.failure {
        lines.push(format!(
            "failure:   {:?}: {}",
            failure.reason,
            failure.detail.as_str()
        ));
    }
    lines.push(format!("processes: {}", snapshot.processes.len()));
    for process in &snapshot.processes {
        let mut row = format!("  {:<24} {:?}", process.participant, process.state).to_lowercase();
        if let Some(pid) = process.pid {
            row.push_str(&format!(" pid={pid}"));
        }
        if process.restarts > 0 {
            row.push_str(&format!(" restarts={}", process.restarts));
        }
        if let Some(failure) = &process.failure {
            row.push_str(&format!(" failure={}", failure.detail.as_str()));
        }
        lines.push(row);
    }
    lines
}

pub(crate) async fn logs_command(
    app: &AppContext,
    requested_target: Option<&Path>,
    endpoint: Option<String>,
    participant: Option<String>,
    follow: bool,
) -> Result<()> {
    let (_, session) = open_existing(app, requested_target, endpoint).await?;
    let page = session.logs(participant.clone(), 256, None).await?;
    let records = page.records;
    if records.is_empty() {
        eprintln!("no retained log records");
    }
    for record in &records {
        println!("{}", render_log(record));
    }
    if follow {
        follow_logs(&session, participant.as_deref()).await;
    }
    session.shutdown().await;
    Ok(())
}

/// Print the live log stream until the supervisor stops or the operator does.
///
/// Both endings are clean. The stream closing means the execution ended, which
/// is what following it was for; Ctrl+C is the operator saying they have seen
/// enough. Neither is an error, and both leave through the same return so the
/// session is shut down rather than abandoned at process exit.
async fn follow_logs(session: &Session, participant: Option<&str>) {
    let stream = match session.follow_logs().await {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("failed to follow the log stream: {error:#}");
            return;
        }
    };
    loop {
        let observed = tokio::select! {
            observed = stream.recv() => observed,
            result = tokio::signal::ctrl_c() => {
                if let Err(error) = result {
                    tracing::debug!("failed to watch for Ctrl+C: {error}");
                }
                return;
            }
        };
        let Ok(observed) = observed else {
            // The supervisor's stream ended: the execution is over, which is the
            // end of the log, not a failure to read it.
            return;
        };
        let record = observed.body.record;
        if participant.is_none_or(|wanted| record.participant_id == wanted) {
            println!("{}", render_log(&record));
        }
    }
}

fn render_log(record: &phoxal_client::supervisor::logs::Record) -> String {
    let mut line = format!(
        "[{}] {:?}: {}",
        record.participant_id, record.level, record.message
    );
    if record.dropped > 0 {
        line.push_str(&format!(" (producer dropped {})", record.dropped));
    }
    if record.truncated > 0 {
        line.push_str(&format!(" (truncated {})", record.truncated));
    }
    line
}

// ---------------------------------------------------------------------------
// shared outcome reporting
// ---------------------------------------------------------------------------

fn failure_message(snapshot: &Snapshot) -> String {
    snapshot.failure.as_ref().map_or_else(
        || "the execution failed without reporting a reason".to_string(),
        |failure| format!("{:?}: {}", failure.reason, failure.detail.as_str()),
    )
}

pub(crate) fn report_outcome(
    target: &Target,
    outcome: phoxal_cli_ui::AttachmentOutcome,
) -> Result<()> {
    use phoxal_cli_ui::AttachmentOutcome;
    match outcome {
        AttachmentOutcome::Detached | AttachmentOutcome::ExecutionStopped => Ok(()),
        AttachmentOutcome::ExecutionFailed { reason } => match reason {
            Some(failure) => bail!("{:?}: {}", failure.reason, failure.detail.as_str()),
            None => bail!(
                "the execution at {} ended without reporting a reason; see the supervisor's own \
                 output for the exact error",
                target.endpoint
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use phoxal_client::supervisor::execution::{
        DesiredState, Detail, Process, ProcessState, SupervisorFailure, SupervisorFailureReason,
    };
    use phoxal_client::{BusError, BusFault, QueryError};
    use phoxal_runtime_contract::clock::Clock;
    use phoxal_runtime_contract::identity::{ParticipantId, ProducerId, RobotId};
    use phoxal_runtime_contract::metadata::ParticipantKind;

    use super::*;

    fn snapshot(lifecycle: Lifecycle) -> Snapshot {
        Snapshot {
            revision: 7,
            lifecycle,
            startup: Vec::new(),
            processes: vec![Process {
                participant: ParticipantId::new("base").expect("fixture participant"),
                kind: ParticipantKind::Service,
                component: None,
                desired: DesiredState::Running,
                state: ProcessState::Ready,
                pid: Some(4_242),
                producer: Some(
                    ProducerId::try_from((1_u128 << 124) | 43).expect("fixture producer"),
                ),
                restarts: 2,
                failure: None,
            }],
            failure: None,
        }
    }

    fn connected() -> phoxal_client::Connected {
        phoxal_client::Connected {
            execution: phoxal_runtime_contract::identity::ExecutionId::mint(),
            robot: RobotId::new("rover").expect("fixture robot"),
            clock: Clock::Simulated,
            manual_drive: None,
            framework: phoxal_runtime_contract::version::FrameworkVersion::CURRENT,
        }
    }

    /// `status` renders the authoritative snapshot and nothing else - there is
    /// no second source to reconcile it against.
    #[test]
    fn status_renders_the_snapshot_including_every_process_row() {
        let rendered = render_status(&snapshot(Lifecycle::Ready), &connected()).join("\n");
        assert!(rendered.contains("robot:     rover"), "{rendered}");
        assert!(rendered.contains("clock:     simulated"), "{rendered}");
        assert!(rendered.contains("lifecycle: ready"), "{rendered}");
        assert!(rendered.contains("revision:  7"), "{rendered}");
        assert!(rendered.contains("processes: 1"), "{rendered}");
        assert!(rendered.contains("base"), "{rendered}");
        assert!(rendered.contains("pid=4242"), "{rendered}");
        assert!(rendered.contains("restarts=2"), "{rendered}");
    }

    fn target() -> Target {
        Target::at_endpoint(
            "unixsock-stream//tmp/rover/.phoxal/run/supervisor.sock".to_string(),
            PathBuf::from("/tmp/rover"),
        )
    }

    fn mismatch() -> anyhow::Error {
        ConnectError::IncompatibleFramework {
            remote: phoxal_runtime_contract::version::FrameworkVersion::new(0, 57, 0),
            client: phoxal_runtime_contract::version::FrameworkVersion::new(0, 56, 2),
            refusal: phoxal_client::CompatibilityRefusal::RemoteNewer,
        }
        .into()
    }

    /// Only the discovery result that says exactly "none" is retryable. Every
    /// post-discovery, ambiguity, state, query, and transport error keeps its
    /// original evidence.
    #[test]
    fn only_no_execution_is_classified_as_retryable_absence() {
        let absent = ConnectError::NoExecution {
            endpoint: "unixsock-stream//tmp/rover.sock".to_string(),
        };
        assert_eq!(
            classify_connect_error(&absent),
            ConnectFailure::RetryableAbsence
        );

        let execution = phoxal_runtime_contract::identity::ExecutionId::mint();
        for fatal in [
            ConnectError::MultipleExecutions {
                endpoint: "tcp/127.0.0.1:7447".to_string(),
                count: 2,
                executions: vec![execution],
            },
            ConnectError::SupervisorUnavailable,
            ConnectError::UnreadableBootstrap {
                detail: "phoxal/supervisor-connect/v1".to_string(),
            },
            ConnectError::Snapshot(
                phoxal_client::supervisor::execution::SnapshotError::MissingSupervisorFailure,
            ),
            ConnectError::Query(QueryError::Unavailable),
            ConnectError::Bus(BusError::Closed),
        ] {
            assert_eq!(classify_connect_error(&fatal), ConnectFailure::Fatal);
            let rendered = format!(
                "{:#}",
                describe_missing_execution(anyhow::Error::new(fatal), &target())
            );
            assert!(
                !rendered.contains("Start one with"),
                "a concrete connection failure must stay intact: {rendered}"
            );
        }
        assert_eq!(classify_connect_failure(&mismatch()), ConnectFailure::Fatal);
        assert_eq!(
            classify_connect_failure(&anyhow::anyhow!("untyped opening failure")),
            ConnectFailure::Fatal
        );
    }

    /// A discovered but incompatible execution reaches the operator unchanged.
    #[test]
    fn a_mismatch_reaches_the_operator_without_the_start_one_advice() {
        let described = format!("{:#}", describe_missing_execution(mismatch(), &target()));
        assert!(described.contains("remote framework 0.57.0"), "{described}");
        assert!(described.contains("client framework 0.56.2"), "{described}");
        assert!(!described.contains("Start one with"), "{described}");
    }

    /// A true discovery miss earns the advice that names the command which
    /// would produce an execution.
    #[test]
    fn a_missing_execution_still_earns_the_advice_that_names_run() {
        let described = format!(
            "{:#}",
            describe_missing_execution(
                ConnectError::NoExecution {
                    endpoint: "unixsock-stream//tmp/rover.sock".to_string(),
                }
                .into(),
                &target(),
            )
        );
        assert!(
            described.contains("Start one with `phoxal run /tmp/rover`"),
            "{described}"
        );
    }

    /// `run` never silently attaches: the refusal names attach and stop, which
    /// are the two things the operator can actually do.
    #[test]
    fn a_live_execution_makes_run_fail_with_the_commands_that_apply() {
        let target = target();
        let message = already_live_message(&target);
        assert!(message.contains("already live"), "{message}");
        assert!(message.contains("never attaches"), "{message}");
        assert!(message.contains("phoxal attach /tmp/rover"), "{message}");
        assert!(message.contains("phoxal stop /tmp/rover"), "{message}");
        assert!(message.contains(&target.endpoint), "{message}");
    }

    /// `attach`, `stop`, `status`, and `logs` are existing-execution only.
    /// None of them may reach the build path, and the compiler is what proves
    /// it: the only function that builds takes the lock and calls
    /// `prepare_run`, and it is reachable from `run` and `start` alone.
    #[test]
    fn only_run_and_start_reach_the_build_path() {
        let building = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/application/lifecycle.rs"),
        )
        .expect("this module is readable");
        let (_, after) = building
            .split_once("async fn open_existing(")
            .expect("the existing-execution resolver exists");
        let existing_half = after
            .split_once("// shared outcome reporting")
            .map_or(after, |(before, _)| before);
        for forbidden in [
            "build_publish_and_launch",
            "prepare_run",
            "supervisor::spawn",
            "ProjectLock::acquire",
        ] {
            assert!(
                !existing_half.contains(forbidden),
                "the existing-execution commands must not reach `{forbidden}`"
            );
        }
        assert!(
            existing_half.contains("Session::open"),
            "the existing-execution commands attach and nothing else"
        );
    }

    /// Project work never gates on the two installed halves reporting the same
    /// product version: the project selects its framework, and the CLI product
    /// version is not a compatibility identity for it. `run` and `start` go
    /// straight from the requested target to resolving the project.
    #[test]
    fn run_and_start_do_not_gate_project_work_on_the_cli_product_version() {
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/application/lifecycle.rs"),
        )
        .expect("this module is readable");
        for command in [
            "pub(crate) async fn run_command(",
            "pub(crate) async fn start_command(",
        ] {
            let (_, body) = source.split_once(command).expect("the command exists");
            let (opening, _) = body
                .split_once("Target::resolve")
                .expect("it resolves a target");
            assert!(
                !opening.contains("crate::pair::"),
                "{command} must not consult the installed pair before doing project work"
            );
        }
    }

    /// Identity loss and an observed `Stopped` snapshot are the only two facts
    /// that prove the requested execution ended.
    #[test]
    fn stop_classification_preserves_every_disconnect_reason() {
        assert!(
            stop_disconnect_outcome(DisconnectReason::SupervisorIdentityLost).is_ok(),
            "the execution-scoped identity disappearing proves the execution ended"
        );

        for failure in [
            DisconnectReason::ConnectionClosed,
            DisconnectReason::SnapshotStreamFailed {
                detail: "snapshot subscriber closed".to_string(),
            },
            DisconnectReason::TransportFault {
                fault: BusFault::WorkerExited {
                    worker: "outbound-drain".to_string(),
                },
            },
            DisconnectReason::LifecycleEnded,
        ] {
            let expected = failure.clone();
            let error = stop_disconnect_outcome(failure)
                .expect_err("a non-identity disconnect does not prove a successful stop");
            assert!(
                matches!(
                    error.downcast_ref::<ClientError>(),
                    Some(ClientError::Disconnected { reason }) if reason == &expected
                ),
                "the structured disconnect cause must survive: {error:#}"
            );
        }

        assert!(stop_snapshot_outcome(&snapshot(Lifecycle::Stopped)).unwrap());
        assert!(!stop_snapshot_outcome(&snapshot(Lifecycle::Ready)).unwrap());
        assert!(stop_snapshot_outcome(&snapshot(Lifecycle::Failed)).is_err());
    }

    #[test]
    fn command_transport_failures_are_not_rewritten_as_successful_stops() {
        for error in [
            ClientError::Query(QueryError::Unavailable),
            ClientError::Bus(BusError::Closed),
        ] {
            let rendered = error.to_string();
            let surfaced = stop_command_failure(anyhow::Error::new(error), None)
                .expect_err("a transport closure does not prove the execution stopped");
            assert_eq!(surfaced.to_string(), rendered);
        }

        assert!(
            stop_command_failure(
                anyhow::Error::new(ClientError::Query(QueryError::Unavailable)),
                Some(DisconnectReason::SupervisorIdentityLost),
            )
            .is_ok(),
            "separately observed identity loss is authoritative"
        );
    }

    #[test]
    fn local_close_failures_cannot_be_reported_as_a_successful_stop() {
        let error = finish_stop(
            Ok(()),
            Err(anyhow::anyhow!("transport close report retained a failure")),
        )
        .expect_err("local close evidence must surface");
        assert!(
            format!("{error:#}").contains("local client connection did not close cleanly"),
            "{error:#}"
        );
    }

    #[test]
    fn a_failed_snapshot_renders_its_typed_reason_and_evidence() {
        let mut failed = snapshot(Lifecycle::Failed);
        failed.failure = Some(SupervisorFailure {
            reason: SupervisorFailureReason::ControlPlaneLost,
            detail: Detail::new("the world clock never became ready"),
        });
        let rendered = render_status(&failed, &connected()).join("\n");
        assert!(rendered.contains("ControlPlaneLost"), "{rendered}");
        assert!(
            rendered.contains("the world clock never became ready"),
            "{rendered}"
        );
        assert_eq!(
            failure_message(&failed),
            "ControlPlaneLost: the world clock never became ready"
        );
    }
}
