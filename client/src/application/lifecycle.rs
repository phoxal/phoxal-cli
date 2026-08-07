//! The execution lifecycle commands.
//!
//! ```text
//! run     build a fresh bundle, launch phoxald on it, attach
//! start   the same, but wait for readiness and exit
//! attach  existing execution only: no build, no mutation
//! stop    existing execution only
//! status  existing execution only
//! logs    existing execution only
//! ```
//!
//! `run` creates a fresh execution and never silently attaches to one that is
//! already live; `attach` is the explicit existing-execution path. Nothing
//! here can supervise a graph: the daemon is a separate executable and the
//! only channel to it is the supervisor API.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use phoxal_supervisor_api::{Lifecycle, Snapshot};
use phoxal_supervisor_client::{AttachError, Attachment, AttachmentConfig};

use super::daemon::{self, LaunchedDaemon};
use super::session::{self, Detachable};
use crate::attach::{CLIENT_PARTICIPANT, Session};
use crate::cli::AppContext;
use crate::lock::{ProjectLock, ProjectLockIdentity, ProjectOperation};

/// How long a client waits for a freshly launched daemon to answer connect.
const HANDSHAKE_BUDGET: Duration = Duration::from_secs(60);

/// How long `start` waits for the graph to reach readiness after it answers.
const READINESS_BUDGET: Duration = Duration::from_secs(5 * 60);

/// How often a launch is re-probed while waiting for the daemon to come up.
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
    /// The Zenoh endpoint the daemon binds and this client dials.
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
    crate::pair::require_exact()?;
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
    crate::pair::require_exact()?;
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
/// Both callers confirm the exact `phoxal` + `phoxald` pair before they even
/// resolve a project: a build no matching daemon can execute is wasted work,
/// and the operator's real problem is a broken installation rather than their
/// robot.
///
/// The live check is a real handshake, not a socket-file probe: an execution
/// that answers connect is live, and `run` refuses rather than silently
/// attaching to it. Only then is the build lock taken, so a refused run never
/// blocks the running execution's own operations.
async fn build_publish_and_launch(
    app: &AppContext,
    target: &Target,
    options: RunOptions,
) -> Result<LaunchedDaemon> {
    refuse_if_live(target).await?;
    let _lock = ProjectLock::acquire(ProjectLockIdentity::resolve(
        &target.project,
        ProjectOperation::Run,
    ))
    .context("failed to acquire the project build lock")?;
    // Re-check under the lock: the window between the probe and the lock is
    // exactly where a concurrent `phoxal run` would have started a daemon.
    refuse_if_live(target).await?;
    // The handshake above is the friendly message; the supervisor lock is the
    // authority. A daemon that has taken its lock but has not yet answered
    // connect is live, and this is what closes that startup window.
    crate::lock::refuse_while_execution_is_live(&target.project)?;

    let runtime_target =
        phoxal_cli_project::resolve_target(Some(&target.project), &target.project)?;
    let (reporter, signal_task) =
        crate::cli::output::progress::cancellable_preparation_reporter(app.ui);
    let prepared = phoxal_cli_project::prepare_run(phoxal_cli_project::PrepareRunRequest {
        target: runtime_target,
        run: phoxal_cli_core::project::launch_plan::RunIdentity::mint_or_adopt(None),
        drivers: phoxal_cli_project::DriverRequest {
            mode: match options.drivers {
                DriversMode::On => phoxal_cli_project::DriverMode::On,
                DriversMode::Off => phoxal_cli_project::DriverMode::Off,
            },
            subset: options.drivers_subset,
        },
        offline: app.offline,
        reporter,
    })
    .await;
    signal_task.abort();
    let prepared = prepared?;

    app.ui.info(format!(
        "launching the supervisor on {}",
        prepared.staged_root.display()
    ));
    daemon::spawn(&prepared.staged_root)
}

/// Fail with the commands that actually apply when an execution already
/// answers at this endpoint.
async fn refuse_if_live(target: &Target) -> Result<()> {
    if probe(&target.endpoint).await.is_none() {
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
/// the socket file's existence both survive a killed daemon; a reply does not.
async fn probe(endpoint: &str) -> Option<Attachment> {
    Attachment::open(&AttachmentConfig::new(endpoint, CLIENT_PARTICIPANT))
        .await
        .ok()
}

/// Watch the launched daemon until it answers connect, or until it exits.
///
/// Before the handshake completes there is nothing but process facts and
/// stderr to go on, so an early exit is reported with the daemon's own
/// diagnostics rather than as a timeout.
async fn await_attachment(
    target: &Target,
    mut launched: LaunchedDaemon,
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
                // A handshake that failed because the daemon disagrees with
                // this client is permanent: retrying cannot make an
                // incompatible pair compatible.
                if let Some(attach) = error.downcast_ref::<AttachError>()
                    && matches!(attach, AttachError::Incompatible { .. })
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
                        format!("; phoxald reported: {diagnostics}")
                    };
                    return Err(error).with_context(|| {
                        format!(
                            "timed out after {}s waiting for phoxald to answer at {}{hint}",
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
    let deadline = tokio::time::Instant::now() + budget;
    let mut snapshots = session.snapshots();
    loop {
        let installed = snapshots.borrow_and_update().clone();
        if let Some(snapshot) = installed {
            match snapshot.lifecycle {
                // Degraded is ready enough to hand back: an optional
                // participant that failed does not make the robot unavailable.
                Lifecycle::Ready | Lifecycle::Degraded => return Ok(()),
                Lifecycle::Failed => bail!(failure_message(&snapshot)),
                Lifecycle::Stopping | Lifecycle::Stopped => {
                    bail!("the execution stopped before it became ready")
                }
                Lifecycle::Starting => {}
            }
        }
        tokio::select! {
            () = session.disconnected() => {
                bail!("the supervisor's identity token was lost before the graph became ready")
            }
            _ = tokio::time::sleep_until(deadline) => {
                bail!(
                    "timed out after {}s waiting for the graph to become ready",
                    budget.as_secs()
                )
            }
            changed = snapshots.changed() => {
                changed.context("the supervisor snapshot stream ended before readiness")?;
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
        .with_context(|| {
            format!(
                "no execution answered at {}. Start one with `phoxal run {}`",
                target.endpoint,
                target.project.display()
            )
        })?;
    Ok((target, session))
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
        Ok(phoxal_supervisor_api::CommandOutcome::Accepted) => await_terminal(&session).await,
        Ok(phoxal_supervisor_api::CommandOutcome::Rejected { reason }) => {
            session.shutdown().await;
            bail!("the supervisor rejected the stop: {reason:?}")
        }
        // The daemon answers `Stop` before it tears down, but the answer still
        // has to cross a session the teardown is closing. A stop that ends the
        // execution it was aimed at is the outcome that was asked for, not an
        // error to print at the operator.
        Err(error) if ended_before_answering(&error) => Ok(()),
        Err(error) => {
            session.shutdown().await;
            return Err(error);
        }
    };
    session.shutdown().await;
    outcome?;
    app.ui
        .info(format!("execution stopped at {}", target.endpoint));
    Ok(())
}

/// Whether a failed command means the execution ended rather than that the
/// command failed.
///
/// Only the two shapes that say exactly that: the reply stream closed with no
/// reply at all, and the bus session itself closed. A timeout is deliberately
/// not one of them - a daemon that never answered may still be wedged and
/// running, and reporting that as a clean stop would be a lie.
fn ended_before_answering(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<AttachError>(),
        Some(
            AttachError::Query(phoxal_bus::QueryError::Unavailable)
                | AttachError::Bus(phoxal_bus::BusError::Closed)
        )
    )
}

/// Wait for the execution to reach a terminal snapshot, or for its identity
/// token to go - a daemon that vanished mid-shutdown has stopped either way.
async fn await_terminal(session: &Session) -> Result<()> {
    let mut snapshots = session.snapshots();
    loop {
        let installed = snapshots.borrow_and_update().clone();
        if let Some(snapshot) = installed {
            match snapshot.lifecycle {
                Lifecycle::Stopped => return Ok(()),
                Lifecycle::Failed => bail!(failure_message(&snapshot)),
                _ => {}
            }
        }
        tokio::select! {
            () = session.disconnected() => return Ok(()),
            changed = snapshots.changed() => {
                if changed.is_err() {
                    return Ok(());
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
    for line in render_status(&snapshot) {
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
pub(crate) fn render_status(snapshot: &Snapshot) -> Vec<String> {
    let mut lines = vec![
        format!(
            "robot:     {}/{}",
            snapshot.robot.namespace, snapshot.robot.id
        ),
        format!("clock:     {:?}", snapshot.mode).to_lowercase(),
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
        let mut row =
            format!("  {:<24} {:?}", process.key.to_string(), process.state).to_lowercase();
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
    let participant = participant.map(phoxal_supervisor_api::Name::new);
    let page = session.logs(participant.clone(), 0, None).await?;
    let phoxal_supervisor_api::supervisor::logs::Snapshot::V0 { records, .. } = page;
    if records.is_empty() {
        eprintln!("no retained log records");
    }
    for record in &records {
        println!("{}", render_log(record));
    }
    if follow {
        follow_logs(&session, participant.as_ref()).await;
    }
    session.shutdown().await;
    Ok(())
}

/// Print the live log stream until the daemon stops or the operator does.
///
/// Both endings are clean. The stream closing means the execution ended, which
/// is what following it was for; Ctrl+C is the operator saying they have seen
/// enough. Neither is an error, and both leave through the same return so the
/// session is shut down rather than abandoned at process exit.
async fn follow_logs(session: &Session, participant: Option<&phoxal_supervisor_api::Name>) {
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
            // The daemon's stream ended: the execution is over, which is the
            // end of the log, not a failure to read it.
            return;
        };
        let phoxal_supervisor_api::supervisor::logs::Follow::V0 { record, .. } = observed.body;
        if participant.is_none_or(|wanted| &record.participant == wanted) {
            println!("{}", render_log(&record));
        }
    }
}

fn render_log(record: &phoxal_supervisor_api::LogRecord) -> String {
    let mut line = format!(
        "[{}] {:?}: {}",
        record.participant,
        record.level,
        record.message.as_str()
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
    use phoxal_runtime_contract::ProducerId;
    use phoxal_supervisor_api::{
        DaemonFailure, DaemonFailureReason, DesiredState, Detail, ExecutionMode, Name, Process,
        ProcessKey, ProcessState, RobotIdentity, StartupRequirement,
    };

    use super::*;

    fn snapshot(lifecycle: Lifecycle) -> Snapshot {
        Snapshot {
            revision: 7,
            robot: RobotIdentity {
                id: Name::new("rover"),
                namespace: Name::new("lab"),
            },
            mode: ExecutionMode::Simulated,
            lifecycle,
            startup: Vec::new(),
            processes: vec![Process {
                key: ProcessKey::Driver {
                    instance: Name::new("base"),
                },
                component: None,
                startup: StartupRequirement::Required,
                desired: DesiredState::Running,
                state: ProcessState::Ready,
                pid: Some(4_242),
                producer: Some(ProducerId::try_from(0x2b).expect("fixture producer")),
                restarts: 2,
                failure: None,
            }],
            failure: None,
        }
    }

    /// `status` renders the authoritative snapshot and nothing else - there is
    /// no second source to reconcile it against.
    #[test]
    fn status_renders_the_snapshot_including_every_process_row() {
        let rendered = render_status(&snapshot(Lifecycle::Ready)).join("\n");
        assert!(rendered.contains("robot:     lab/rover"), "{rendered}");
        assert!(rendered.contains("clock:     simulated"), "{rendered}");
        assert!(rendered.contains("lifecycle: ready"), "{rendered}");
        assert!(rendered.contains("revision:  7"), "{rendered}");
        assert!(rendered.contains("processes: 1"), "{rendered}");
        assert!(rendered.contains("driver:base"), "{rendered}");
        assert!(rendered.contains("pid=4242"), "{rendered}");
        assert!(rendered.contains("restarts=2"), "{rendered}");
    }

    /// `run` never silently attaches: the refusal names attach and stop, which
    /// are the two things the operator can actually do.
    #[test]
    fn a_live_execution_makes_run_fail_with_the_commands_that_apply() {
        let target = Target::at_endpoint(
            "unixsock-stream//tmp/rover/.phoxal/run/supervisor.sock".to_string(),
            PathBuf::from("/tmp/rover"),
        );
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
            "daemon::spawn",
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

    /// The pair is confirmed before a project is even resolved, let alone
    /// built: `run` and `start` are the two commands that need a `phoxald`, and
    /// a missing or mismatched one is an installation problem the operator must
    /// hear about first.
    #[test]
    fn run_and_start_confirm_the_exact_pair_before_resolving_anything() {
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
                opening.contains("crate::pair::require_exact()?"),
                "{command} must confirm the pair before it resolves a project"
            );
        }
    }

    /// The client half of the stop contract: a stop whose answer was lost to
    /// the daemon's own teardown is the outcome that was asked for, and a
    /// command that failed for any other reason still is a failure.
    #[test]
    fn a_stop_answer_lost_to_teardown_is_the_execution_ending_not_a_failure() {
        for ended in [
            AttachError::Query(phoxal_bus::QueryError::Unavailable),
            AttachError::Bus(phoxal_bus::BusError::Closed),
        ] {
            let rendered = ended.to_string();
            assert!(
                ended_before_answering(&anyhow::Error::new(ended)),
                "{rendered} means the execution ended"
            );
        }

        // A wedged daemon that never answered may still be running: reporting
        // that as a clean stop would be a lie, so it stays an error.
        assert!(!ended_before_answering(&anyhow::Error::new(
            AttachError::Query(phoxal_bus::QueryError::Timeout(
                phoxal_bus::QueryFailure::deadline_exceeded("query deadline exceeded")
            ))
        )));
        // So does a failure that is not an attachment error at all.
        assert!(!ended_before_answering(&anyhow::anyhow!("something else")));
    }

    #[test]
    fn a_failed_snapshot_renders_its_typed_reason_and_evidence() {
        let mut failed = snapshot(Lifecycle::Failed);
        failed.failure = Some(DaemonFailure {
            reason: DaemonFailureReason::WorldClockMissing,
            detail: Detail::new("the world clock never became ready"),
        });
        let rendered = render_status(&failed).join("\n");
        assert!(rendered.contains("WorldClockMissing"), "{rendered}");
        assert!(
            rendered.contains("the world clock never became ready"),
            "{rendered}"
        );
        assert_eq!(
            failure_message(&failed),
            "WorldClockMissing: the world clock never became ready"
        );
    }
}
