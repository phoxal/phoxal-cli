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
use phoxal_client::{
    BusError, ClientError, ConnectError, ConnectOptions, Connection, DisconnectReason,
};

use super::session::{self, Detachable};
use super::startup::Startup;
use super::summary::{SessionSummary, attachment_ending};
use super::supervisor::{self, LaunchedSupervisor};
use crate::attach::{CLIENT_PARTICIPANT, Session};
use crate::cli::context::AppContext;
use crate::cli::exit::ReportedExit;
use crate::cli::output::welcome::{Mode, StepId};
use crate::lock::{ProjectLock, ProjectLockIdentity, ProjectOperation};

/// How long a client waits for a freshly launched supervisor to answer connect.
const HANDSHAKE_BUDGET: Duration = Duration::from_secs(60);

/// How long `start` waits for the graph to reach readiness after it answers.
const READINESS_BUDGET: Duration = Duration::from_secs(5 * 60);

/// A stopped execution must not leave the one-shot CLI blocked forever while
/// a broken transport tears down its local session.
const SESSION_CLOSE_BUDGET: Duration = Duration::from_secs(5);

/// A lost final bus observation must not leave the one-shot stop command
/// waiting forever after the supervisor accepted the command.
const STOP_TERMINAL_BUDGET: Duration = Duration::from_secs(5);

/// How often a launch is re-probed while waiting for the supervisor to come up.
const PROBE_INTERVAL: Duration = Duration::from_millis(100);

/// How long a supervisor that just dropped its identity is given to actually
/// exit, so its own account of why can be quoted instead of the transport's.
const SUPERVISOR_EXIT_EVIDENCE_BUDGET: Duration = Duration::from_secs(5);

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
    let startup = Startup::begin(app, &target.project, Mode::Native);
    let session = match open_fresh_execution(app, &target, options, &startup).await {
        Ok(session) => session,
        Err(error) => return Err(startup.failed(error)),
    };
    // The dashboard takes the terminal only from here: the graph is up, and
    // the checklist has already given the terminal back.
    startup.ready();
    let outcome = session::drive(app, &target.project, session, Detachable::Yes).await?;
    report_outcome(app, &target, outcome)
}

pub(crate) async fn start_command(
    app: &AppContext,
    requested_target: Option<&Path>,
    options: RunOptions,
) -> Result<()> {
    let target = Target::resolve(requested_target, app.project.root())?;
    let startup = Startup::begin(app, &target.project, Mode::Native);
    let session = match open_fresh_execution(app, &target, options, &startup).await {
        Ok(session) => session,
        Err(error) => return Err(startup.failed(error)),
    };
    startup.ready();
    let display = target.project.display();
    session.shutdown().await;
    app.ui.info(format!(
        "robot instance ready; attach with `phoxal attach {display}` or stop with `phoxal stop {display}`"
    ));
    Ok(())
}

/// Build, launch, attach, and wait for the graph - the whole road from a
/// project to an execution an operator can be shown.
async fn open_fresh_execution(
    app: &AppContext,
    target: &Target,
    options: RunOptions,
    startup: &Startup,
) -> Result<Session> {
    let launched = build_publish_and_launch(app, target, options, startup).await?;
    startup.step(StepId::Supervisor, "waiting for the supervisor");
    let (session, mut launched) = await_attachment(target, launched, HANDSHAKE_BUDGET).await?;
    if let Err(error) = startup.await_graph(&session, READINESS_BUDGET).await {
        let error = explain_with_supervisor_exit(error, &mut launched).await;
        // Close the attachment before reporting. An abandoned session tears its
        // transport down during process shutdown instead, which is where the
        // "subscriber closed" noise and a lost graceful detach come from.
        session.shutdown().await;
        return Err(error);
    }
    Ok(session)
}

/// Prefer the supervisor's own account when the graph wait ended because the
/// supervisor went away.
///
/// What the client observes in that moment is its transport losing the
/// supervisor's identity token - true, and useless: it names the consequence.
/// The supervisor writes the cause to its log on the way out, so the exit is
/// awaited briefly first: the identity is dropped as the supervisor begins
/// unwinding, and the process itself is a moment behind. A supervisor that is
/// still running after the bound did not cause this, and the original evidence
/// stands.
async fn explain_with_supervisor_exit(
    error: anyhow::Error,
    launched: &mut LaunchedSupervisor,
) -> anyhow::Error {
    let deadline = tokio::time::Instant::now() + SUPERVISOR_EXIT_EVIDENCE_BUDGET;
    loop {
        match launched.exited() {
            Ok(Some(status)) => return anyhow!(launched.startup_exit_message(status)),
            Ok(None) => {}
            Err(_) => return error,
        }
        if tokio::time::Instant::now() >= deadline {
            return error;
        }
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
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
    startup: &Startup,
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
    let reporter = startup.reporter();
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
    .await??;

    startup.complete(StepId::PrepareRuntime, staged_detail(&prepared.release));
    supervisor::spawn(
        &prepared.release,
        &phoxal_cli_host::paths::RuntimePaths::for_root(&target.project).supervisor_log(),
    )
}

/// What the prepared release actually contains, counted from the release
/// itself rather than from the build events that produced it: an unchanged
/// project skips every build event and still stages the same binaries.
pub(crate) fn staged_detail(release: &phoxal_cli_project::ReleaseLayout) -> String {
    match std::fs::read_dir(release.bundle.join("bin")) {
        Ok(entries) => format!("{} binaries staged", entries.count()),
        Err(_) => "release ready".to_string(),
    }
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
        Err(error) => {
            let error = anyhow::Error::new(error);
            if classify_connect_at(endpoint, &error) == ConnectFailure::RetryableAbsence {
                return Ok(None);
            }
            Err(error)
        }
    }
}

/// Whether a transport failure at a local endpoint is simply nothing being
/// there.
///
/// A `unixsock-stream` endpoint *is* a filesystem path: with no socket file at
/// it, nothing has ever bound it, and the transport failure that follows is
/// that absence rather than a fault. The transport cannot tell the two apart -
/// it reports the same "unable to connect to any of [...]" for a missing socket
/// and for a broken one - so this checks the one local fact that decides it.
///
/// Both halves are required. Only a *transport* failure qualifies, because
/// every other opening failure means an execution was already discovered, and
/// a discovered execution's evidence must survive. Without this, the first
/// probe of a project that has never run - and every probe of a supervisor that
/// has not yet bound its socket - reads as a hard failure instead of as the
/// startup latency it is.
fn is_absent_local_endpoint(endpoint: &str, error: &anyhow::Error) -> bool {
    if !matches!(
        error.downcast_ref::<ConnectError>(),
        Some(ConnectError::Bus(BusError::Transport(_)))
    ) {
        return false;
    }
    endpoint
        .strip_prefix("unixsock-stream/")
        .is_some_and(|path| !Path::new(path).exists())
}

/// Classify an opening failure against the endpoint it was addressed to.
fn classify_connect_at(endpoint: &str, error: &anyhow::Error) -> ConnectFailure {
    match classify_connect_failure(error) {
        ConnectFailure::RetryableAbsence => ConnectFailure::RetryableAbsence,
        ConnectFailure::Fatal if is_absent_local_endpoint(endpoint, error) => {
            ConnectFailure::RetryableAbsence
        }
        ConnectFailure::Fatal => ConnectFailure::Fatal,
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

/// The same question, asked while waiting for a supervisor this client just
/// launched and can see is still alive.
///
/// The bar is different here, and deliberately lower. A supervisor comes up in
/// stages - it binds its socket, opens its router, then declares the supervisor
/// API - so "nothing announced yet", "the supervisor API has no responder yet",
/// and a query or transport that found nobody are all the same fact: not yet.
/// Treating them as answers is what turns a lost race with a starting
/// supervisor into a failed `run`. Only a permanent fact about a discovered
/// execution - a version disagreement, an unreadable bootstrap, two executions
/// at one endpoint - ends the wait before the budget does.
const fn classify_launch_failure(error: &ConnectError) -> ConnectFailure {
    match error {
        ConnectError::NoExecution { .. }
        | ConnectError::SupervisorUnavailable
        | ConnectError::Query(_)
        | ConnectError::Bus(_) => ConnectFailure::RetryableAbsence,
        ConnectError::MultipleExecutions { .. }
        | ConnectError::SourceLabel(_)
        | ConnectError::IncompatibleFramework { .. }
        | ConnectError::UnreadableBootstrap { .. }
        | ConnectError::Snapshot(_) => ConnectFailure::Fatal,
    }
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
) -> Result<(Session, LaunchedSupervisor)> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if let Some(status) = launched.exited()? {
            bail!(launched.early_exit_message(status));
        }
        match Session::open(&target.endpoint, target.project.display().to_string()).await {
            Ok(session) => return Ok((session, launched)),
            Err(error) => {
                // The child is alive, so "not answering yet" is latency; only a
                // permanent fact about a discovered execution is already an
                // answer.
                if error
                    .downcast_ref::<ConnectError>()
                    .map_or(ConnectFailure::Fatal, classify_launch_failure)
                    == ConnectFailure::Fatal
                {
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
    if classify_connect_at(&target.endpoint, &error) == ConnectFailure::Fatal {
        return error;
    }
    let missing = format!(
        "no execution answered at {}. Start one with `phoxal run {}`",
        target.endpoint,
        target.project.display()
    );
    // An endpoint with no socket file has already been fully explained. The
    // transport's own account of failing to reach it adds a crates.io source
    // path and no information, so it is dropped rather than appended.
    if is_absent_local_endpoint(&target.endpoint, &error) {
        return anyhow!(missing);
    }
    error.context(missing)
}

pub(crate) async fn attach_command(
    app: &AppContext,
    requested_target: Option<&Path>,
    endpoint: Option<String>,
) -> Result<()> {
    let (target, session) = open_existing(app, requested_target, endpoint).await?;
    let outcome = session::drive(app, &target.project, session, Detachable::Yes).await?;
    report_outcome(app, &target, outcome)
}

pub(crate) async fn stop_command(
    app: &AppContext,
    requested_target: Option<&Path>,
    endpoint: Option<String>,
) -> Result<()> {
    let (target, session) = open_existing(app, requested_target, endpoint).await?;
    let outcome = match session.ports.supervisor.stop().await {
        Ok(phoxal_client::supervisor::execution::CommandOutcome::Accepted { .. }) => {
            match tokio::time::timeout(STOP_TERMINAL_BUDGET, await_terminal(&session)).await {
                Ok(outcome) => outcome,
                Err(_) => confirm_accepted_stop(&target).await,
            }
        }
        Ok(phoxal_client::supervisor::execution::CommandOutcome::Rejected { reason }) => {
            Err(anyhow!("the supervisor rejected the stop: {reason:?}"))
        }
        Err(error) => stop_command_failure(error, session.disconnect_reason()),
    };
    let close = match tokio::time::timeout(SESSION_CLOSE_BUDGET, session.close()).await {
        Ok(close) => close,
        Err(_) => Err(anyhow!(
            "the execution stopped, but the local client connection did not close within {}s",
            SESSION_CLOSE_BUDGET.as_secs()
        )),
    };
    finish_stop(outcome, close)?;
    app.ui
        .info(format!("execution stopped at {}", target.endpoint));
    Ok(())
}

/// Confirm an accepted stop when the dying transport loses its final
/// liveliness notification. A fresh connection attempt is independent of the
/// original session: only an absent supervisor proves that the execution is
/// gone, while every other result remains an error.
async fn confirm_accepted_stop(target: &Target) -> Result<()> {
    let options = ConnectOptions::new(&target.endpoint, CLIENT_PARTICIPANT);
    match tokio::time::timeout(STOP_TERMINAL_BUDGET, Connection::connect(&options)).await {
        Ok(Err(ConnectError::NoExecution { .. } | ConnectError::SupervisorUnavailable)) => Ok(()),
        Ok(Err(error)) => Err(error).context(
            "the supervisor accepted the stop, but a fresh connection could not confirm its exit",
        ),
        Ok(Ok(connection)) => {
            let _ = tokio::time::timeout(SESSION_CLOSE_BUDGET, connection.close()).await;
            bail!("the supervisor accepted the stop, but the execution still answers")
        }
        Err(_) => bail!(
            "the supervisor accepted the stop, but a fresh connection did not finish within {}s",
            STOP_TERMINAL_BUDGET.as_secs()
        ),
    }
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

/// Close a dashboard session with the one block that says what happened.
///
/// The failure is reported *in* the block rather than as a second, separate
/// error line: the operator has just watched a terminal application vanish, and
/// one plain account of the ending - with the log that holds the rest - is the
/// whole answer.
pub(crate) fn report_outcome(
    app: &AppContext,
    target: &Target,
    outcome: phoxal_cli_ui::AttachmentOutcome,
) -> Result<()> {
    use phoxal_cli_ui::AttachmentOutcome;
    let failed = matches!(outcome, AttachmentOutcome::ExecutionFailed { .. });
    SessionSummary::new(
        attachment_ending(&outcome),
        vec![phoxal_cli_host::paths::RuntimePaths::for_root(&target.project).supervisor_log()],
    )
    .print(&target.project, app.output.theme);
    if failed {
        return Err(ReportedExit(1).into());
    }
    Ok(())
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

    /// A local endpoint with no socket file is nothing to connect to, not a
    /// failure to connect: it is both the first probe of a project that has
    /// never run and every probe of a supervisor that has not bound its socket
    /// yet. Only the transport failure qualifies - a discovered execution's
    /// evidence survives whatever the filesystem looks like.
    #[test]
    fn a_missing_local_socket_is_absence_rather_than_a_transport_failure() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("supervisor.sock");
        let endpoint = format!("unixsock-stream/{}", socket.display());
        let transport: anyhow::Error = ConnectError::Bus(BusError::Transport(
            "Unable to connect to any of [unixsock-stream//...]".to_string(),
        ))
        .into();

        assert_eq!(
            classify_connect_at(&endpoint, &transport),
            ConnectFailure::RetryableAbsence
        );

        // The same transport failure against a socket that exists is a real
        // failure: something is bound there and it did not answer.
        std::fs::write(&socket, b"").unwrap();
        assert_eq!(
            classify_connect_at(&endpoint, &transport),
            ConnectFailure::Fatal
        );
        std::fs::remove_file(&socket).unwrap();

        // A discovered execution keeps its evidence even with no socket file.
        assert_eq!(
            classify_connect_at(&endpoint, &mismatch()),
            ConnectFailure::Fatal
        );
        // A remote endpoint has no local fact to check.
        assert_eq!(
            classify_connect_at("tcp/127.0.0.1:7447", &transport),
            ConnectFailure::Fatal
        );
    }

    /// A supervisor comes up in stages, so a client that launched it and can
    /// see it running must keep waiting through every "not yet" - and must
    /// still stop at once for a permanent fact about what it found.
    #[test]
    fn a_launch_wait_retries_every_not_yet_and_stops_at_a_permanent_fact() {
        for pending in [
            ConnectError::NoExecution {
                endpoint: "unixsock-stream//tmp/rover.sock".to_string(),
            },
            ConnectError::SupervisorUnavailable,
            ConnectError::Query(QueryError::Unavailable),
            ConnectError::Bus(BusError::Closed),
        ] {
            assert_eq!(
                classify_launch_failure(&pending),
                ConnectFailure::RetryableAbsence,
                "{pending}"
            );
        }
        for permanent in [
            ConnectError::MultipleExecutions {
                endpoint: "tcp/127.0.0.1:7447".to_string(),
                count: 2,
                executions: vec![phoxal_runtime_contract::identity::ExecutionId::mint()],
            },
            ConnectError::IncompatibleFramework {
                remote: phoxal_runtime_contract::version::FrameworkVersion::new(0, 57, 0),
                client: phoxal_runtime_contract::version::FrameworkVersion::new(0, 56, 2),
                refusal: phoxal_client::CompatibilityRefusal::RemoteNewer,
            },
            ConnectError::UnreadableBootstrap {
                detail: "phoxal/supervisor-connect/v1".to_string(),
            },
        ] {
            assert_eq!(
                classify_launch_failure(&permanent),
                ConnectFailure::Fatal,
                "{permanent}"
            );
        }
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
