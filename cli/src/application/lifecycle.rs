//! The execution lifecycle commands.
//!
//! ```text
//! run     build a fresh bundle, launch the supervisor and every runtime, attach
//! start   the same, but return once the graph is up and leave it running
//! attach  existing execution only: no build, no launch, no mutation
//! stop    end the session this project's `.phoxal/run/session.json` records
//! status  existing execution only
//! logs    existing execution only
//! ```
//!
//! `run` creates a fresh execution and never silently attaches to one that is
//! already live; `attach` is the explicit existing-execution path.
//!
//! The supervisor observes and starts nothing, so the process that launches the
//! robot's runtimes is this one. That is what makes `stop` a local operation:
//! there is no stop command on the supervisor, and there could not be - it
//! never started anything to stop. What ends a session is signalling the pids
//! this client wrote down when it started them.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use phoxal_cli_observation::GraphSplit;
use phoxal_client::supervisor::execution::Snapshot;
use phoxal_client::{BusError, ConnectError, ConnectOptions, Connection};

use super::launcher::{
    self, LaunchedRuntime, OwnedSession, RecordedProcess, RecordedRuntime, Selection, SessionRecord,
};
use super::session::{self, Detachable, SessionOwnership};
use super::startup::Startup;
use super::summary::{SessionSummary, attachment_ending};
use super::supervisor::{self, LaunchedSupervisor};
use crate::attach::{CLIENT_PARTICIPANT, LocalRuntimeFacts, Session};
use crate::cli::context::AppContext;
use crate::cli::exit::ReportedExit;
use crate::cli::output::welcome::{Mode, StepId};
use crate::lock::{ProjectLock, ProjectLockIdentity, ProjectOperation};

/// How long a client waits for a freshly launched supervisor to answer connect.
const HANDSHAKE_BUDGET: Duration = Duration::from_secs(60);

/// How long `run`/`start` wait for the launched runtimes to appear.
const READINESS_BUDGET: Duration = Duration::from_secs(5 * 60);

/// How often a launch is re-probed while waiting for the supervisor to come up.
const PROBE_INTERVAL: Duration = Duration::from_millis(100);

/// How often this client re-reads its own children while a session runs.
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Which runtimes a launch starts, as the command line states it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunOptions {
    pub(crate) drivers_off: bool,
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

    pub(crate) fn paths(&self) -> phoxal_cli_host::paths::RuntimePaths {
        phoxal_cli_host::paths::RuntimePaths::for_root(&self.project)
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
    let launched = match open_fresh_execution(app, &target, &options, &startup, false).await {
        Ok(launched) => launched,
        Err(error) => return Err(startup.failed(error)),
    };
    // The dashboard takes the terminal only from here: the graph is up, and
    // the checklist has already given the terminal back.
    startup.ready();
    let outcome = drive_launched_session(app, &target, launched).await?;
    report_outcome(app, &target, &outcome)
}

pub(crate) async fn start_command(
    app: &AppContext,
    requested_target: Option<&Path>,
    options: RunOptions,
) -> Result<()> {
    let target = Target::resolve(requested_target, app.project.root())?;
    let startup = Startup::begin(app, &target.project, Mode::Native);
    let launched = match open_fresh_execution(app, &target, &options, &startup, false).await {
        Ok(launched) => launched,
        Err(error) => return Err(startup.failed(error)),
    };
    startup.ready();
    let absent = launched.absent_runtimes();
    // Detach the session's client half only. The children are in their own
    // process groups with no inherited pipes, so they survive this process
    // exiting - which is the whole point of `start`.
    launched.session.shutdown().await;
    let display = target.project.display();
    // The robot is left running, so its runtimes' own account of themselves is
    // still reachable: a runtime logs over the bus, and the supervisor is the
    // one holding it.
    if let Some(absent) = absent {
        app.ui.warn(format!(
            "started degraded; not present: {absent}. `phoxal logs <participant>` says why"
        ));
    }
    app.ui.info(format!(
        "robot instance ready; attach with `phoxal attach {display}` or stop with `phoxal stop \
         {display}`"
    ));
    Ok(())
}

/// One launched session: everything this client started, plus its attachment.
pub(crate) struct LaunchedSession {
    pub(crate) session: Session,
    supervisor: LaunchedSupervisor,
    /// Re-reads this client's own children so the dashboard and the startup
    /// checklist can say why an absent runtime is absent.
    pub(crate) children: tokio::task::JoinHandle<()>,
    record: SessionRecord,
    /// The bundle every runtime of this session was launched against. It is a
    /// live-session fact, not part of the record `stop` reads: what ends a
    /// session is signalling pids.
    bundle: PathBuf,
    paths: phoxal_cli_host::paths::RuntimePaths,
    /// Held for the whole session: the bundle being executed is the one this
    /// command just published, and a concurrent build would replace it.
    _lock: ProjectLock,
}

impl LaunchedSession {
    /// The runtimes the supervisor does not see, named for the operator, or
    /// `None` when the whole robot is up.
    fn absent_runtimes(&self) -> Option<String> {
        self.session
            .snapshot()
            .and_then(|snapshot| GraphSplit::from(&snapshot).absent_line())
    }

    pub(crate) fn owned(&self) -> OwnedSession {
        OwnedSession::new(self.record.clone(), self.paths.clone())
    }

    /// The bundle every runtime of this session was launched against.
    pub(crate) fn bundle(&self) -> &Path {
        &self.bundle
    }
}

/// Build, launch, attach, and wait - the whole road from a project to a running
/// robot an operator can be shown.
async fn open_fresh_execution(
    app: &AppContext,
    target: &Target,
    options: &RunOptions,
    startup: &Startup,
    simulation: bool,
) -> Result<LaunchedSession> {
    let launched = launch_execution(app, target, options, startup, simulation).await?;
    await_session_ready(launched, startup).await
}

/// Everything up to and including the attachment: build, publish, start the
/// supervisor, start the runtimes, record them, attach.
///
/// It stops short of waiting for the runtimes so a caller that has to put
/// something between the launch and the wait can. A simulation is exactly that
/// caller: nothing becomes present until Webots is stepping, and Webots is
/// staged against the bundle this function just published.
pub(crate) async fn launch_execution(
    app: &AppContext,
    target: &Target,
    options: &RunOptions,
    startup: &Startup,
    simulation: bool,
) -> Result<LaunchedSession> {
    let (release, lock) = build_and_publish(app, target, startup).await?;
    let selection = resolve_selection(&release.bundle, options, simulation)?;

    startup.step(StepId::Supervisor, "waiting for the supervisor");
    let paths = target.paths();
    let mut supervisor = supervisor::spawn(&release, &paths.supervisor_log())?;
    // The supervisor has to be answering before a runtime is started: it runs
    // the router every runtime dials, and a runtime that starts first would
    // only spend its first seconds failing to connect.
    await_supervisor(target, &mut supervisor, HANDSHAKE_BUDGET).await?;
    startup.complete(StepId::Supervisor, format!("router on {}", target.endpoint));

    let runtimes = launcher::launch(
        &release.bundle,
        &target.endpoint,
        simulation,
        &selection,
        &paths,
    )?;
    let record = SessionRecord {
        endpoint: target.endpoint.clone(),
        supervisor: RecordedProcess {
            pid: supervisor.pid(),
            log: paths.supervisor_log(),
        },
        runtimes: runtimes
            .iter()
            .map(|runtime| RecordedRuntime {
                participant: runtime.participant.clone(),
                pid: runtime.pid(),
                log: runtime.log.clone(),
            })
            .collect(),
    };
    // Written before the wait, not after: a startup that times out or is
    // interrupted has still left processes running, and `phoxal stop` has to
    // be able to end them.
    record.write(&paths)?;

    // The child watcher starts before the wait, so the checklist and every
    // later frame read live facts about this client's own processes rather
    // than a snapshot taken once.
    let local = LocalRuntimeFacts::default();
    let children = tokio::spawn(watch_children(runtimes, local.clone()));
    let launched = LaunchedSession {
        session: Session::open(
            &target.endpoint,
            target.project.display().to_string(),
            local,
        )
        .await?,
        supervisor,
        children,
        record,
        bundle: release.bundle,
        paths,
        _lock: lock,
    };

    Ok(launched)
}

/// Wait for the launched runtimes to appear, ending the session if they do not.
///
/// Everything this client started is running by here and its record is on
/// disk, so a failed wait stops it rather than leaving the operator to find a
/// half-started robot.
pub(crate) async fn await_session_ready(
    launched: LaunchedSession,
    startup: &Startup,
) -> Result<LaunchedSession> {
    if let Err(error) = startup
        .await_graph(
            &launched.session,
            launched.session.local(),
            READINESS_BUDGET,
        )
        .await
    {
        let cleanup = launched.owned().stop().await;
        launched.children.abort();
        launched.session.shutdown().await;
        return Err(match cleanup {
            Ok(()) => error,
            Err(cleanup) => error.context(format!("session cleanup also failed: {cleanup:#}")),
        });
    }
    Ok(launched)
}

/// Which runtimes this launch starts, validated against the bundle's own set.
fn resolve_selection(bundle: &Path, options: &RunOptions, simulation: bool) -> Result<Selection> {
    let available = phoxal_cli_project::bundle_runtimes(bundle)?
        .into_iter()
        .filter(|runtime| runtime.role == phoxal_cli_project::RuntimeRole::Driver)
        .map(|runtime| runtime.participant_id)
        .collect::<BTreeSet<_>>();
    if simulation {
        // Webots simulates the components, so a physical driver beside it would
        // be a second thing driving the same hardware model.
        anyhow::ensure!(
            !options.drivers_off && options.drivers_subset.is_empty(),
            "a simulation never starts component drivers; --drivers/--driver do not apply"
        );
        return Ok(Selection::DriversOff);
    }
    Selection::resolve(
        options.drivers_off,
        options.drivers_subset.clone(),
        &available,
    )
}

/// The shared front half of `run` and `start`: refuse a live execution, take
/// the lock, and publish the release this session will execute.
async fn build_and_publish(
    app: &AppContext,
    target: &Target,
    startup: &Startup,
) -> Result<(phoxal_cli_project::ReleaseLayout, ProjectLock)> {
    refuse_if_live(target).await?;
    let lock = ProjectLock::acquire(ProjectLockIdentity::resolve(
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
            offline,
            reporter,
        })
    })
    .await??;
    startup.complete(StepId::PrepareRuntime, staged_detail(&prepared.release));
    Ok((prepared.release, lock))
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

/// Drive a launched session's dashboard, watching its own supervisor as it
/// goes, and end whatever is still running when it returns.
async fn drive_launched_session(
    app: &AppContext,
    target: &Target,
    launched: LaunchedSession,
) -> Result<phoxal_cli_ui::AttachmentOutcome> {
    let LaunchedSession {
        session,
        supervisor,
        children,
        record,
        paths,
        ..
    } = launched;
    let owned = OwnedSession::new(record, paths.clone());
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    let (stop_watch_tx, stop_watch_rx) = tokio::sync::oneshot::channel();
    let supervisor = std::sync::Arc::new(tokio::sync::Mutex::new(supervisor));
    let watcher = tokio::spawn(supervisor::watch_owned(
        std::sync::Arc::clone(&supervisor),
        exit_tx,
        stop_watch_rx,
    ));

    let outcome = session::drive(
        app,
        &target.project,
        session,
        Detachable::Yes,
        SessionOwnership {
            owned: Some(owned.clone()),
            supervisor_exit: Some(exit_rx),
        },
    )
    .await;
    let _ = stop_watch_tx.send(());
    let _ = watcher.await;
    children.abort();
    let _ = children.await;

    let outcome = outcome?;
    // Detaching leaves everything running - that is what detaching means. Every
    // other ending means the session is over, so nothing of it is left behind.
    if outcome != phoxal_cli_ui::AttachmentOutcome::Detached {
        owned.stop().await.context("failed to end the session")?;
    }
    Ok(outcome)
}

/// Keep this client's view of its own children fresh while the dashboard runs.
async fn watch_children(mut runtimes: Vec<LaunchedRuntime>, local: LocalRuntimeFacts) {
    loop {
        let mut values = phoxal_cli_observation::LocalRuntimes::new();
        for runtime in &mut runtimes {
            let state = match runtime.exited() {
                Ok(None) => phoxal_cli_observation::LocalRuntimeState::Running,
                Ok(Some(status)) => phoxal_cli_observation::LocalRuntimeState::Exited {
                    status: status.to_string(),
                },
                Err(error) => phoxal_cli_observation::LocalRuntimeState::Exited {
                    status: format!("could not be inspected: {error}"),
                },
            };
            let Ok(participant) =
                phoxal_runtime_contract::identity::ParticipantId::new(runtime.participant.clone())
            else {
                continue;
            };
            values.insert(
                participant,
                phoxal_cli_observation::LocalRuntime {
                    state,
                    log: runtime.log.clone(),
                },
            );
        }
        local.replace(values);
        tokio::time::sleep(CHILD_POLL_INTERVAL).await;
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
/// Nothing else has been started yet, so the only evidence available is process
/// facts and the supervisor's own log: an early exit is reported with those
/// rather than as a timeout.
pub(crate) async fn await_supervisor(
    target: &Target,
    launched: &mut LaunchedSupervisor,
    budget: Duration,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if let Some(status) = launched.exited()? {
            bail!(launched.early_exit_message(status));
        }
        match Connection::connect(&ConnectOptions::new(&target.endpoint, CLIENT_PARTICIPANT)).await
        {
            Ok(connection) => {
                // This probe exists to learn that the supervisor is answering;
                // the session opens its own attachment.
                let _ = connection.close().await;
                return Ok(());
            }
            Err(error) => {
                if classify_launch_failure(&error) == ConnectFailure::Fatal {
                    return Err(error.into());
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
                    return Err(anyhow::Error::new(error).context(format!(
                        "timed out after {}s waiting for phoxal-supervisor to answer at {}{hint}",
                        budget.as_secs(),
                        target.endpoint
                    )));
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
    let session = Session::open(
        &target.endpoint,
        target.project.display().to_string(),
        LocalRuntimeFacts::default(),
    )
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
    // An attachment owns nothing: it launched no process, so it cannot stop
    // one, and the dashboard does not offer a key that would only refuse.
    let outcome = session::drive(
        app,
        &target.project,
        session,
        Detachable::Yes,
        SessionOwnership::default(),
    )
    .await?;
    report_outcome(app, &target, &outcome)
}

/// End the session this project recorded, from any terminal.
///
/// There is no remote stop to fall back on: the supervisor starts nothing and
/// therefore stops nothing. A project with no session record has nothing this
/// CLI started, and says so rather than pretending to have ended something.
pub(crate) async fn stop_command(app: &AppContext, requested_target: Option<&Path>) -> Result<()> {
    let target = Target::resolve(requested_target, app.project.root())?;
    let paths = target.paths();
    let Some(record) = SessionRecord::read(&paths) else {
        bail!(
            "no session record at {}; this CLI did not start anything here. `stop` ends the \
             processes `phoxal run`/`phoxal start` recorded - a robot started some other way is \
             stopped the same way it was started",
            launcher::record_path(&paths).display()
        )
    };
    launcher::stop_session(&record, &paths).await?;
    app.ui
        .info(format!("session stopped at {}", record.endpoint));
    Ok(())
}

pub(crate) async fn status_command(
    app: &AppContext,
    requested_target: Option<&Path>,
    endpoint: Option<String>,
) -> Result<()> {
    let (target, session) = open_existing(app, requested_target, endpoint).await?;
    let snapshot = session
        .snapshot()
        .context("the supervisor answered connect but published no snapshot")?;
    let record = SessionRecord::read(&target.paths());
    for line in render_status(&snapshot, session.connected(), record.as_ref()) {
        println!("{line}");
    }
    session.shutdown().await;
    Ok(())
}

/// Render one snapshot as the plain status report.
///
/// The snapshot half is a pure function of the authoritative document. The
/// local half - which of these runtimes this machine started, and where their
/// logs are - comes from the session record, and is simply absent for a robot
/// this CLI did not launch.
pub(crate) fn render_status(
    snapshot: &Snapshot,
    connected: &phoxal_client::Connected,
    record: Option<&SessionRecord>,
) -> Vec<String> {
    let mut lines = vec![
        format!("robot:     {}", connected.robot),
        format!("lifecycle: {:?}", snapshot.lifecycle).to_lowercase(),
        format!("revision:  {}", snapshot.revision),
    ];
    lines.push(format!("processes: {}", snapshot.processes.len()));
    for process in &snapshot.processes {
        let mut row = format!("  {:<24} {:?}", process.participant, process.state).to_lowercase();
        if let Some(log) = record.and_then(|record| record.log_for(process.participant.as_str())) {
            row.push_str(&format!(" log={}", log.display()));
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

/// Close a dashboard session with the one block that says what happened.
pub(crate) fn report_outcome(
    app: &AppContext,
    target: &Target,
    outcome: &phoxal_cli_ui::AttachmentOutcome,
) -> Result<()> {
    use phoxal_cli_ui::AttachmentOutcome;
    let failed = matches!(outcome, AttachmentOutcome::ExecutionEnded { .. });
    let paths = target.paths();
    SessionSummary::new(attachment_ending(outcome), vec![paths.supervisor_log()])
        .print(&target.project, app.output.theme);
    if failed {
        return Err(ReportedExit(1).into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use phoxal_client::supervisor::execution::{Lifecycle, Process, ProcessState};
    use phoxal_client::{BusError, QueryError};
    use phoxal_runtime_contract::identity::{ParticipantId, ProducerId, RobotId};
    use phoxal_runtime_contract::metadata::ParticipantKind;

    use super::*;

    fn snapshot(lifecycle: Lifecycle) -> Snapshot {
        Snapshot {
            revision: 7,
            lifecycle,
            processes: vec![Process {
                participant: ParticipantId::new("base").expect("fixture participant"),
                kind: ParticipantKind::Service,
                state: ProcessState::Present,
                producer: Some(
                    ProducerId::try_from((1_u128 << 124) | 43).expect("fixture producer"),
                ),
            }],
        }
    }

    fn connected() -> phoxal_client::Connected {
        phoxal_client::Connected {
            execution: phoxal_runtime_contract::identity::ExecutionId::mint(),
            robot: RobotId::new("rover").expect("fixture robot"),
            framework: phoxal_runtime_contract::version::FrameworkVersion::CURRENT,
        }
    }

    fn record() -> SessionRecord {
        SessionRecord {
            endpoint: "unixsock-stream//tmp/rover/.phoxal/run/supervisor.sock".to_string(),
            supervisor: RecordedProcess {
                pid: 1,
                log: PathBuf::from("/tmp/rover/.phoxal/run/supervisor.log"),
            },
            runtimes: vec![RecordedRuntime {
                participant: "base".to_string(),
                pid: 2,
                log: PathBuf::from("/tmp/rover/.phoxal/run/log/base.log"),
            }],
        }
    }

    /// `status` renders the authoritative snapshot, and adds the log path only
    /// when this machine is the one that started the runtime.
    #[test]
    fn status_renders_the_snapshot_and_only_locally_launched_logs() {
        let remote = render_status(&snapshot(Lifecycle::Ready), &connected(), None).join("\n");
        assert!(remote.contains("robot:     rover"), "{remote}");
        assert!(remote.contains("lifecycle: ready"), "{remote}");
        assert!(remote.contains("revision:  7"), "{remote}");
        assert!(remote.contains("processes: 1"), "{remote}");
        assert!(remote.contains("base"), "{remote}");
        assert!(remote.contains("present"), "{remote}");
        assert!(!remote.contains("log="), "{remote}");

        let local = render_status(
            &snapshot(Lifecycle::Degraded),
            &connected(),
            Some(&record()),
        )
        .join("\n");
        assert!(local.contains("lifecycle: degraded"), "{local}");
        assert!(
            local.contains("log=/tmp/rover/.phoxal/run/log/base.log"),
            "{local}"
        );
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
            "build_and_publish",
            "prepare_run",
            "supervisor::spawn",
            "launcher::launch",
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
}
