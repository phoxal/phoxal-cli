//! The startup that gates the dashboard.
//!
//! `run`, `start`, and `simulation webots run` all reach an attached execution
//! the same way: prepare the project, launch the framework supervisor, attach,
//! and wait for the graph. This module drives that sequence's *presentation* -
//! the checklist in [`crate::cli::output::welcome`] - and owns nothing about
//! the sequence itself, so the lifecycle and simulation semantics stay where
//! they are.
//!
//! The dashboard is entered only from [`Startup::ready`], i.e. only once the
//! supervisor published `Ready` or `Degraded`. Every other ending goes through
//! [`Startup::failed`], which renders the failed step, the reason, and the log
//! that has the whole story - and never lets the terminal application start.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{Result, bail};
use phoxal_client::supervisor::execution::{
    Lifecycle, ProcessState, Snapshot, StartupStepKind, StartupStepState,
};
use tokio_util::sync::CancellationToken;

use crate::attach::Session;
use crate::cli::context::AppContext;
use crate::cli::exit::ReportedExit;
use crate::cli::output::diagnostics::{DiagnosticLevel, RuntimeEvent};
use crate::cli::output::welcome::{Mode, StepId, Welcome};

/// How often the spinner advances and the live detail is repainted.
const TICK_INTERVAL: Duration = Duration::from_millis(100);

/// The exit status a terminal signal conventionally earns: 128 + SIGINT.
const CANCELLED_EXIT: u8 = 130;

/// How many captured diagnostics may queue behind the checklist before the
/// oldest are dropped. Startup is short and quiet; this only has to absorb a
/// burst of transport warnings.
const DIAGNOSTIC_CAPACITY: usize = 64;

pub(crate) struct Startup {
    welcome: Arc<Mutex<Welcome>>,
    mode: Mode,
    /// Cancelled by Ctrl+C. Preparation polls it through the reporter, and the
    /// graph wait selects on it.
    cancellation: CancellationToken,
    /// Ends the presenter's own background tasks.
    stop: CancellationToken,
    /// The presenter's background tasks, so quiescing can end them outright
    /// rather than only asking. A timer left alive into the runtime's own
    /// shutdown panics on a driver that is already gone.
    tasks: Vec<tokio::task::JoinHandle<()>>,
    /// The startup log, plus every process log this mode may have produced.
    logs: Vec<PathBuf>,
    /// Set once the terminal has been handed to the dashboard (or to the
    /// failure block), so nothing renders a second startup over it.
    handed_over: AtomicBool,
}

impl Startup {
    /// Print the brand and start the checklist for `project`.
    pub(crate) fn begin(app: &AppContext, project: &Path, mode: Mode) -> Self {
        let paths = phoxal_cli_host::paths::RuntimePaths::for_root(project);
        let log = paths.startup_log();
        // A project that cannot hold a log still gets a checklist: the log is
        // evidence, not a precondition for starting a robot.
        let _ = std::fs::create_dir_all(paths.volatile_root());
        let welcome = Arc::new(Mutex::new(Welcome::start(
            app.output.interactive,
            app.output.theme,
            app.ui,
            project,
            mode,
            Some(&log),
        )));

        let stop = CancellationToken::new();
        let cancellation = CancellationToken::new();
        let mut tasks = vec![tokio::spawn(watch_interrupt(
            cancellation.clone(),
            stop.clone(),
        ))];
        // An animated checklist owns one line it keeps rewriting, so everything
        // that would otherwise write straight to stderr - a Zenoh retry
        // warning, a `tracing::warn!` from a feed - is routed into the
        // checklist instead of landing on top of it.
        //
        // A plain, non-interactive checklist owns nothing and must NOT install
        // this: its own lines go out through the same `Ui`, so routing would
        // feed them back into the checklist that just printed them.
        if app.output.interactive {
            let (diagnostics_tx, diagnostics_rx) = tokio::sync::mpsc::channel(DIAGNOSTIC_CAPACITY);
            crate::cli::output::diagnostics::install(diagnostics_tx);
            tasks.push(tokio::spawn(animate(
                Arc::clone(&welcome),
                diagnostics_rx,
                stop.clone(),
            )));
        }

        let mut logs = vec![log, paths.supervisor_log()];
        if mode == Mode::Webots {
            logs.push(paths.webots_log());
        }

        Self {
            welcome,
            mode,
            cancellation,
            stop,
            tasks,
            logs,
            handed_over: AtomicBool::new(false),
        }
    }

    fn welcome(&self) -> MutexGuard<'_, Welcome> {
        self.welcome.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The reporter project preparation drives the first two steps through.
    pub(crate) fn reporter(&self) -> Arc<dyn phoxal_cli_project::Reporter> {
        Arc::new(PreparationReporter {
            welcome: Arc::clone(&self.welcome),
            cancellation: self.cancellation.clone(),
            state: Mutex::new(PreparationState::default()),
        })
    }

    pub(crate) fn step(&self, id: StepId, detail: impl Into<String>) {
        self.welcome().begin(id, detail);
    }

    pub(crate) fn detail(&self, id: StepId, detail: impl Into<String>) {
        self.welcome().detail(id, detail);
    }

    pub(crate) fn complete(&self, id: StepId, detail: impl Into<String>) {
        self.welcome().complete(id, detail);
    }

    /// Settle the supervisor line from what the freshly attached execution has
    /// already published.
    ///
    /// A simulation settles it here rather than from the bootstrap sequence: a
    /// simulated router only finishes once the world clock exists, and the
    /// world clock is the step *after* this one.
    pub(crate) fn attached(&self, session: &Session) {
        let detail = session
            .snapshot()
            .as_ref()
            .map_or_else(|| "attached".to_string(), supervisor_detail);
        self.complete(StepId::Supervisor, detail);
    }

    /// Watch the attached execution until its graph is usable.
    ///
    /// `Ready` and `Degraded` both hand over: a degraded graph is attachable,
    /// and the dashboard is where an operator sees which participant is down.
    pub(crate) async fn await_graph(&self, session: &Session, budget: Duration) -> Result<()> {
        let mut snapshots = session.snapshots();
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            let observed = snapshots.borrow_and_update().clone();
            if let Some(snapshot) = observed {
                match self.observe(&snapshot) {
                    Progress::Ready => return Ok(()),
                    Progress::Failed(reason) => bail!(reason),
                    Progress::Waiting => {}
                }
            }
            tokio::select! {
                changed = snapshots.changed() => {
                    if changed.is_err() {
                        bail!("the supervisor stopped publishing snapshots before the graph was ready");
                    }
                }
                reason = session.disconnected() => {
                    bail!("the attachment ended before the graph was ready: {reason}")
                }
                () = self.cancellation.cancelled() => bail!("startup cancelled"),
                () = tokio::time::sleep_until(deadline) => bail!(
                    "timed out after {}s waiting for the robot graph to become ready",
                    budget.as_secs()
                ),
            }
        }
    }

    /// Fold one published snapshot into the checklist.
    fn observe(&self, snapshot: &Snapshot) -> Progress {
        let mut welcome = self.welcome();
        // A simulation's supervisor step is already settled by the attach
        // handshake: its router waits for the world clock, which only exists
        // once Webots - the step after it - is up.
        if self.mode == Mode::Native && !welcome.settled(StepId::Supervisor) {
            if step_state(snapshot, StartupStepKind::Bundle) == Some(StartupStepState::Done)
                && step_state(snapshot, StartupStepKind::Router) == Some(StartupStepState::Done)
            {
                welcome.complete(StepId::Supervisor, supervisor_detail(snapshot));
            } else {
                welcome.detail(StepId::Supervisor, active_startup_detail(snapshot));
            }
        }
        if welcome.settled(StepId::Supervisor) {
            let detail = graph_detail(snapshot);
            if welcome.running(StepId::RobotGraph) {
                welcome.detail(StepId::RobotGraph, detail);
            } else if !welcome.settled(StepId::RobotGraph) {
                welcome.begin(StepId::RobotGraph, detail);
            }
        }
        match snapshot.lifecycle {
            Lifecycle::Ready | Lifecycle::Degraded => {
                welcome.complete(StepId::RobotGraph, graph_detail(snapshot));
                Progress::Ready
            }
            Lifecycle::Failed => Progress::Failed(failure_detail(snapshot)),
            Lifecycle::Stopping | Lifecycle::Stopped => {
                Progress::Failed("the execution ended before its graph became ready".to_string())
            }
            Lifecycle::Starting => Progress::Waiting,
        }
    }

    /// Hand the terminal over: the graph is up and the dashboard takes it from
    /// here.
    pub(crate) fn ready(&self) {
        self.handed_over.store(true, Ordering::SeqCst);
        self.quiesce();
        self.welcome().close();
    }

    /// Render the ending a failed startup earns, and replace the error with an
    /// already-reported exit so it is not printed a second time.
    ///
    /// After [`Self::ready`] this is a no-op passthrough: an error from the
    /// live session is the session's to report, not the startup's.
    pub(crate) fn failed(&self, error: anyhow::Error) -> anyhow::Error {
        if self.handed_over.swap(true, Ordering::SeqCst) {
            return error;
        }
        self.quiesce();
        let mut welcome = self.welcome();
        if self.cancellation.is_cancelled() {
            welcome.report_cancelled();
            return ReportedExit(CANCELLED_EXIT).into();
        }
        welcome.report_failure(&format!("{error:#}"), &self.logs);
        ReportedExit(1).into()
    }

    /// Stop the presenter's background work and give stderr back to the
    /// process. Idempotent.
    fn quiesce(&self) {
        crate::cli::output::diagnostics::uninstall();
        self.stop.cancel();
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Drop for Startup {
    fn drop(&mut self) {
        self.quiesce();
    }
}

enum Progress {
    Waiting,
    Ready,
    Failed(String),
}

/// Advance the spinner, and fold captured diagnostics in as permanent lines.
async fn animate(
    welcome: Arc<Mutex<Welcome>>,
    mut diagnostics: tokio::sync::mpsc::Receiver<RuntimeEvent>,
    stop: CancellationToken,
) {
    let mut ticker = tokio::time::interval(TICK_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut open = true;
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            _ = ticker.tick() => {
                welcome.lock().unwrap_or_else(PoisonError::into_inner).tick();
            }
            event = diagnostics.recv(), if open => match event {
                Some(event) => {
                    let mut welcome = welcome.lock().unwrap_or_else(PoisonError::into_inner);
                    match event {
                        RuntimeEvent::Diagnostic { level, message, .. } if level > DiagnosticLevel::Info => {
                            welcome.note(&message);
                        }
                        RuntimeEvent::Diagnostic { message, .. } => welcome.record(&message),
                        RuntimeEvent::PhaseStarted { id, label } => {
                            welcome.record(&format!("phase {id} started: {label}"));
                        }
                        RuntimeEvent::PhaseFinished { id, outcome, elapsed } => {
                            welcome.record(&format!("phase {id} finished: {outcome:?} in {elapsed:?}"));
                        }
                    }
                }
                None => open = false,
            },
        }
    }
}

async fn watch_interrupt(cancellation: CancellationToken, stop: CancellationToken) {
    tokio::select! {
        () = stop.cancelled() => {}
        result = tokio::signal::ctrl_c() => {
            if result.is_ok() {
                cancellation.cancel();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// snapshot -> checklist
// ---------------------------------------------------------------------------

fn step_state(snapshot: &Snapshot, kind: StartupStepKind) -> Option<StartupStepState> {
    snapshot
        .startup
        .iter()
        .find(|step| step.kind == kind)
        .map(|step| step.state)
}

fn step_detail(snapshot: &Snapshot, kind: StartupStepKind) -> Option<&str> {
    snapshot
        .startup
        .iter()
        .find(|step| step.kind == kind)
        .and_then(|step| step.detail.as_ref())
        .map(phoxal_client::supervisor::execution::Detail::as_str)
}

/// What the supervisor is doing right now, from its own startup sequence.
fn active_startup_detail(snapshot: &Snapshot) -> String {
    snapshot
        .startup
        .iter()
        .find(|step| step.state == StartupStepState::Active)
        .map_or_else(
            || "waiting for the supervisor".to_string(),
            |step| match step.kind {
                StartupStepKind::Bundle => "opening the bundle".to_string(),
                StartupStepKind::Router => "starting the router".to_string(),
                StartupStepKind::Participants => "launching participants".to_string(),
            },
        )
}

/// The supervisor line's evidence: its bundle is open and its router is up.
/// The router's own detail is `<execution> on <endpoint>`; the endpoint is the
/// half that is worth the width.
fn supervisor_detail(snapshot: &Snapshot) -> String {
    let router = step_detail(snapshot, StartupStepKind::Router)
        .map(|detail| detail.rsplit(" on ").next().unwrap_or(detail).to_string())
        .unwrap_or_else(|| "router".to_string());
    format!("bundle · {router}")
}

fn graph_detail(snapshot: &Snapshot) -> String {
    let total = snapshot.processes.len();
    let ready = snapshot
        .processes
        .iter()
        .filter(|process| process.state == ProcessState::Ready)
        .count();
    if total == 0 {
        return step_detail(snapshot, StartupStepKind::Participants)
            .unwrap_or("waiting for participants")
            .to_string();
    }
    let counted = format!("{ready}/{total} participants ready");
    if snapshot.lifecycle == Lifecycle::Degraded {
        return format!("{counted} · degraded");
    }
    counted
}

fn failure_detail(snapshot: &Snapshot) -> String {
    snapshot.failure.as_ref().map_or_else(
        || "the execution failed without reporting a reason".to_string(),
        |failure| format!("{:?}: {}", failure.reason, failure.detail.as_str()),
    )
}

// ---------------------------------------------------------------------------
// preparation -> checklist
// ---------------------------------------------------------------------------

#[derive(Default)]
struct PreparationState {
    train: Option<String>,
}

struct PreparationReporter {
    welcome: Arc<Mutex<Welcome>>,
    cancellation: CancellationToken,
    state: Mutex<PreparationState>,
}

impl PreparationReporter {
    fn welcome(&self) -> MutexGuard<'_, Welcome> {
        self.welcome.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn project_detail(&self) -> String {
        let train = self
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .train
            .clone();
        train.map_or_else(
            || "robot.yaml".to_string(),
            |train| format!("robot.yaml · framework {train}"),
        )
    }
}

/// The one phase that belongs to the `Project` line. Everything after it -
/// resolving, compiling, installing, staging, publishing - is one runtime
/// preparation as far as an operator is concerned.
const PROJECT_PHASE: &str = "validate";

impl phoxal_cli_project::Reporter for PreparationReporter {
    fn report(&self, event: phoxal_cli_project::PreparationEvent) {
        use phoxal_cli_project::PreparationEvent as Event;
        let mut welcome = self.welcome();
        match event {
            Event::Info(message) | Event::Success(message) => welcome.record(&message),
            Event::Warn(message) | Event::Error(message) => welcome.note(&message),
            // Raw dependency output is the log's, not the terminal's: it is a
            // wall of cargo text that would bury the checklist, and it is
            // exactly what a failed preparation is read back from.
            Event::CommandLine(line) => {
                welcome.record(&line);
                welcome.detail(StepId::PrepareRuntime, line.trim());
            }
            Event::PhaseStarted { id, label } => {
                if id.to_string() == PROJECT_PHASE {
                    welcome.begin(StepId::Project, label);
                } else {
                    welcome.begin(StepId::PrepareRuntime, label);
                }
            }
            Event::PhaseFinished { id, outcome, .. } => {
                welcome.record(&format!("phase {id}: {outcome:?}"));
                if id.to_string() == PROJECT_PHASE
                    && outcome == phoxal_cli_project::PhaseOutcome::Succeeded
                {
                    drop(welcome);
                    let detail = self.project_detail();
                    self.welcome().complete(StepId::Project, detail);
                }
            }
            Event::ProjectResolved { train } => {
                welcome.record(&format!("framework train {train}"));
                drop(welcome);
                self.state
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .train = Some(train);
            }
            Event::SourceBuildPlanned { artifacts, .. } => {
                welcome.detail(
                    StepId::PrepareRuntime,
                    format!("compiling {artifacts} binaries"),
                );
            }
            Event::SourceBuildArtifactCompleted { completed, total } => {
                welcome.detail(
                    StepId::PrepareRuntime,
                    format!("compiled {completed}/{total} binaries"),
                );
            }
            Event::RegistryInstallGroupStarted {
                current,
                total,
                packages,
            } => {
                welcome.detail(
                    StepId::PrepareRuntime,
                    format!("installing {packages} packages ({current}/{total})"),
                );
            }
            Event::SourceBuildGroupStarted { .. }
            | Event::SourceBuildGroupFinished { .. }
            | Event::RegistryInstallGroupFinished { .. } => {}
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_client::supervisor::execution::{
        DesiredState, Detail, Process, StartupStep, SupervisorFailure, SupervisorFailureReason,
    };
    use phoxal_runtime_contract::identity::{ParticipantId, ProducerId};
    use phoxal_runtime_contract::metadata::ParticipantKind;

    fn step(kind: StartupStepKind, state: StartupStepState, detail: Option<&str>) -> StartupStep {
        StartupStep {
            kind,
            state,
            detail: detail.map(Detail::new),
            elapsed_ms: Some(900),
        }
    }

    fn process(participant: &str, state: ProcessState) -> Process {
        Process {
            participant: ParticipantId::new(participant).expect("fixture participant"),
            kind: ParticipantKind::Service,
            component: None,
            desired: DesiredState::Running,
            state,
            pid: Some(11),
            producer: (state == ProcessState::Ready)
                .then(|| ProducerId::try_from((1_u128 << 124) | 7).expect("fixture producer")),
            restarts: 0,
            failure: None,
        }
    }

    fn snapshot(lifecycle: Lifecycle) -> Snapshot {
        Snapshot {
            revision: 3,
            lifecycle,
            startup: vec![
                step(
                    StartupStepKind::Bundle,
                    StartupStepState::Done,
                    Some("/tmp/rover/.phoxal/release/bundle"),
                ),
                step(
                    StartupStepKind::Router,
                    StartupStepState::Done,
                    Some("01JABCDEF on unixsock-stream//tmp/rover/.phoxal/run/supervisor.sock"),
                ),
                step(
                    StartupStepKind::Participants,
                    StartupStepState::Active,
                    None,
                ),
            ],
            processes: vec![
                process("base", ProcessState::Ready),
                process("drive", ProcessState::Starting),
            ],
            failure: None,
        }
    }

    /// The supervisor line names its bundle and the endpoint its router bound,
    /// not the execution id that would eat the whole column.
    #[test]
    fn the_supervisor_line_keeps_the_router_endpoint() {
        let detail = supervisor_detail(&snapshot(Lifecycle::Starting));
        assert_eq!(
            detail,
            "bundle · unixsock-stream//tmp/rover/.phoxal/run/supervisor.sock"
        );
    }

    /// The graph line counts what an operator is waiting for, and says when a
    /// ready graph is nevertheless missing something.
    #[test]
    fn the_graph_line_counts_ready_participants_and_flags_degraded() {
        assert_eq!(
            graph_detail(&snapshot(Lifecycle::Starting)),
            "1/2 participants ready"
        );
        assert_eq!(
            graph_detail(&snapshot(Lifecycle::Degraded)),
            "1/2 participants ready · degraded"
        );

        let mut empty = snapshot(Lifecycle::Starting);
        empty.processes.clear();
        empty.startup[2].detail = Some(Detail::new("waiting for the world clock"));
        assert_eq!(graph_detail(&empty), "waiting for the world clock");
    }

    /// A supervisor that reported a typed failure explains itself; one that
    /// did not still says something true.
    #[test]
    fn a_failed_execution_keeps_its_typed_reason() {
        let mut failed = snapshot(Lifecycle::Failed);
        failed.failure = Some(SupervisorFailure {
            reason: SupervisorFailureReason::LaunchFailed,
            detail: Detail::new("drive never became ready"),
        });
        assert_eq!(
            failure_detail(&failed),
            "LaunchFailed: drive never became ready"
        );

        let mut silent = snapshot(Lifecycle::Failed);
        silent.failure = None;
        assert!(failure_detail(&silent).contains("without reporting a reason"));
    }

    /// While the supervisor is still coming up, the line says which of its own
    /// steps is running rather than a blank wait.
    #[test]
    fn the_supervisor_line_tracks_the_running_bootstrap_step() {
        let mut starting = snapshot(Lifecycle::Starting);
        starting.startup[1].state = StartupStepState::Active;
        starting.startup[2].state = StartupStepState::Pending;
        assert_eq!(active_startup_detail(&starting), "starting the router");

        let mut settled = snapshot(Lifecycle::Starting);
        for step in &mut settled.startup {
            step.state = StartupStepState::Pending;
        }
        assert_eq!(
            active_startup_detail(&settled),
            "waiting for the supervisor"
        );
    }
}
