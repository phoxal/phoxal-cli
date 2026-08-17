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
use phoxal_cli_observation::{GraphSplit, LocalRuntimeState, LocalRuntimes};
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

    /// Watch the attached execution until its runtimes are up.
    ///
    /// `Ready` and `Degraded` both hand over: a degraded graph is attachable,
    /// and the dashboard is where an operator sees which runtime is missing.
    /// `Degraded` is also the honest outcome of `--drivers off`, so refusing it
    /// would refuse a mode the CLI offers.
    pub(crate) async fn await_graph(
        &self,
        session: &Session,
        local: &crate::attach::LocalRuntimeFacts,
        budget: Duration,
    ) -> Result<()> {
        let mut snapshots = session.snapshots();
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            let observed = snapshots.borrow_and_update().clone();
            if let Some(snapshot) = observed
                && self.observe(&snapshot, &local.read()) == Progress::Ready
            {
                return Ok(());
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
                    "{}",
                    self.timeout_message(session.snapshot().as_ref(), &local.read(), budget)
                ),
            }
        }
    }

    /// What a startup timeout has to say.
    ///
    /// A bare "timed out" is useless when the client is the one that launched
    /// the runtimes: it knows exactly which of them never appeared and, for the
    /// ones that died, what they exited with and where their log is.
    fn timeout_message(
        &self,
        snapshot: Option<&Snapshot>,
        local: &LocalRuntimes,
        budget: Duration,
    ) -> String {
        let mut message = format!(
            "timed out after {}s waiting for the robot's runtimes to appear",
            budget.as_secs()
        );
        let Some(snapshot) = snapshot else {
            return message;
        };
        let absent = GraphSplit::from(snapshot)
            .absent
            .iter()
            .map(|participant| match local.get(participant) {
                Some(runtime) => match &runtime.state {
                    LocalRuntimeState::Running => format!("{participant} (started, no lease)"),
                    LocalRuntimeState::Exited { status } => {
                        format!("{participant} ({status}, see {})", runtime.log.display())
                    }
                },
                None => format!("{participant} (not started by this session)"),
            })
            .collect::<Vec<_>>();
        if !absent.is_empty() {
            message.push_str(&format!(": {}", absent.join(", ")));
        }
        message
    }

    /// Fold one published snapshot into the checklist.
    fn observe(&self, snapshot: &Snapshot, local: &LocalRuntimes) -> Progress {
        let mut welcome = self.welcome();
        // The supervisor's own bootstrap is the same in every mode now: it
        // reads the manifest and opens its router, and it waits for nothing
        // else - not for a world clock, not for a participant. So there is no
        // mode-specific settling here.
        if !welcome.settled(StepId::Supervisor) {
            if step_state(snapshot, StartupStepKind::Bundle) == Some(StartupStepState::Done)
                && step_state(snapshot, StartupStepKind::Router) == Some(StartupStepState::Done)
            {
                welcome.complete(StepId::Supervisor, supervisor_detail(snapshot));
            } else {
                welcome.detail(StepId::Supervisor, active_startup_detail(snapshot));
            }
        }
        if welcome.settled(StepId::Supervisor) {
            let detail = graph_detail(snapshot, local);
            if welcome.running(StepId::Runtimes) {
                welcome.detail(StepId::Runtimes, detail);
            } else if !welcome.settled(StepId::Runtimes) {
                welcome.begin(StepId::Runtimes, detail);
            }
        }
        if launch_is_up(snapshot, local) {
            welcome.complete(StepId::Runtimes, graph_detail(snapshot, local));
            return Progress::Ready;
        }
        Progress::Waiting
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Progress {
    Waiting,
    Ready,
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
                StartupStepKind::Bundle => "reading the manifest".to_string(),
                StartupStepKind::Router => "starting the router".to_string(),
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

/// Whether what this session started is up.
///
/// The supervisor's lifecycle answers a different question - is the *robot*
/// whole - and the two diverge exactly where they should: `--drivers off`
/// launches no driver, so the robot is permanently incomplete and `Starting`
/// is the supervisor's honest answer forever. What the operator is waiting for
/// is the runtimes this client actually started, so that is what is waited on;
/// an attachment that started nothing has only the supervisor's answer and
/// uses it.
fn launch_is_up(snapshot: &Snapshot, local: &LocalRuntimes) -> bool {
    if matches!(snapshot.lifecycle, Lifecycle::Ready | Lifecycle::Degraded) {
        return true;
    }
    if local.is_empty() {
        return false;
    }
    snapshot
        .processes
        .iter()
        .filter(|process| local.contains_key(&process.participant))
        .all(|process| process.state == ProcessState::Present)
        && local.keys().all(|launched| {
            snapshot
                .processes
                .iter()
                .any(|process| &process.participant == launched)
        })
}

/// The runtimes line: how many of the robot's expected runtimes are present,
/// how many of them this session started, and what happened to the ones that
/// are not there.
fn graph_detail(snapshot: &Snapshot, local: &LocalRuntimes) -> String {
    let split = GraphSplit::from(snapshot);
    let total = split.expected();
    if total == 0 {
        return "waiting for the runtime set".to_string();
    }
    let mut detail = format!("{}/{total} present", split.present);
    // A launch that deliberately started a subset says so, because otherwise
    // "12/20" reads as eight runtimes that failed rather than eight nobody
    // asked for.
    let not_started = snapshot
        .processes
        .iter()
        .filter(|process| !local.contains_key(&process.participant))
        .count();
    if !local.is_empty() && not_started > 0 {
        detail.push_str(&format!(" · {not_started} not started"));
    }
    // An exited child is the one thing worth the width: it is the difference
    // between "still coming up" and "will never come up".
    let exited = split
        .absent
        .iter()
        .filter_map(|participant| {
            let runtime = local.get(participant)?;
            match &runtime.state {
                LocalRuntimeState::Exited { status } => Some(format!("{participant} {status}")),
                LocalRuntimeState::Running => None,
            }
        })
        .collect::<Vec<_>>();
    if !exited.is_empty() {
        detail.push_str(&format!(" · {}", exited.join(", ")));
    }
    detail
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
    use phoxal_cli_observation::LocalRuntime;
    use phoxal_client::supervisor::execution::{Detail, Process, StartupStep};
    use phoxal_runtime_contract::identity::{ParticipantId, ProducerId, RobotId};
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
            state,
            producer: (state == ProcessState::Present)
                .then(|| ProducerId::try_from((1_u128 << 124) | 7).expect("fixture producer")),
        }
    }

    fn running(participant: &str) -> LocalRuntimes {
        LocalRuntimes::from([(
            ParticipantId::new(participant).expect("fixture participant"),
            LocalRuntime {
                state: LocalRuntimeState::Running,
                log: PathBuf::from(format!("/tmp/rover/.phoxal/run/log/{participant}.log")),
            },
        )])
    }

    fn exited(participant: &str, status: &str) -> LocalRuntimes {
        LocalRuntimes::from([(
            ParticipantId::new(participant).expect("fixture participant"),
            LocalRuntime {
                state: LocalRuntimeState::Exited {
                    status: status.to_string(),
                },
                log: PathBuf::from(format!("/tmp/rover/.phoxal/run/log/{participant}.log")),
            },
        )])
    }

    fn snapshot(lifecycle: Lifecycle) -> Snapshot {
        Snapshot {
            robot: RobotId::new("rover").expect("fixture robot"),
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
            ],
            processes: vec![
                process("base", ProcessState::Present),
                process("drive", ProcessState::Absent),
            ],
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

    /// The runtimes line counts what an operator is waiting for, and - because
    /// this client launched them - names the one that already died rather than
    /// leaving it indistinguishable from one still coming up.
    #[test]
    fn the_runtimes_line_counts_presence_and_names_a_child_that_already_died() {
        assert_eq!(
            graph_detail(&snapshot(Lifecycle::Starting), &LocalRuntimes::new()),
            "1/2 present"
        );
        assert_eq!(
            graph_detail(
                &snapshot(Lifecycle::Starting),
                &exited("drive", "exit status: 1")
            ),
            "1/2 present · 1 not started · drive exit status: 1"
        );

        let mut empty = snapshot(Lifecycle::Starting);
        empty.processes.clear();
        assert_eq!(
            graph_detail(&empty, &LocalRuntimes::new()),
            "waiting for the runtime set"
        );
    }

    /// A launch that started a subset is up when its own subset is up.
    ///
    /// `--drivers off` leaves the robot permanently incomplete, so the
    /// supervisor's lifecycle stays `Starting` forever - correctly, because it
    /// answers "is the robot whole". Gating the dashboard on that would make
    /// the mode unusable, so the gate is what this client started.
    #[test]
    fn a_partial_launch_is_up_when_its_own_runtimes_are_present() {
        let mut snapshot = snapshot(Lifecycle::Starting);
        snapshot.processes = vec![
            process("base", ProcessState::Present),
            process("left_drive", ProcessState::Absent),
        ];
        let launched = running("base");

        assert!(
            launch_is_up(&snapshot, &launched),
            "the one runtime this session started is present"
        );
        assert_eq!(
            graph_detail(&snapshot, &launched),
            "1/2 present · 1 not started"
        );

        // A runtime this session *did* start and that is not there keeps it
        // waiting, whatever the rest of the robot looks like.
        let both = running("base")
            .into_iter()
            .chain(running("left_drive"))
            .collect();
        assert!(!launch_is_up(&snapshot, &both));

        // An attachment started nothing, so it has only the supervisor's
        // answer and waits for it.
        assert!(!launch_is_up(&snapshot, &LocalRuntimes::new()));
        let mut ready = snapshot.clone();
        ready.lifecycle = Lifecycle::Degraded;
        assert!(launch_is_up(&ready, &LocalRuntimes::new()));
    }

    /// A timeout from the process that started the runtimes must say which of
    /// them never appeared, and for the ones that died, what they exited with
    /// and where to read the rest.
    #[test]
    fn a_startup_timeout_names_every_absent_runtime_and_what_this_client_saw() {
        let welcome = Welcome::start(
            false,
            phoxal_cli_ui::Theme::new(phoxal_cli_ui::ColorCapability::None),
            crate::cli::output::plain::Ui::new(false, false),
            Path::new("/tmp/rover"),
            Mode::Native,
            None,
        );
        let startup = Startup {
            welcome: Arc::new(Mutex::new(welcome)),
            cancellation: CancellationToken::new(),
            stop: CancellationToken::new(),
            tasks: Vec::new(),
            logs: Vec::new(),
            handed_over: AtomicBool::new(false),
        };

        let message = startup.timeout_message(
            Some(&snapshot(Lifecycle::Starting)),
            &exited("drive", "exit status: 1"),
            Duration::from_secs(30),
        );
        assert!(message.contains("timed out after 30s"), "{message}");
        assert!(message.contains("drive (exit status: 1"), "{message}");
        assert!(message.contains("log/drive.log"), "{message}");

        let unstarted = startup.timeout_message(
            Some(&snapshot(Lifecycle::Starting)),
            &LocalRuntimes::new(),
            Duration::from_secs(30),
        );
        assert!(
            unstarted.contains("drive (not started by this session)"),
            "{unstarted}"
        );
    }

    /// While the supervisor is still coming up, the line says which of its own
    /// steps is running rather than a blank wait.
    #[test]
    fn the_supervisor_line_tracks_the_running_bootstrap_step() {
        let mut starting = snapshot(Lifecycle::Starting);
        starting.startup[1].state = StartupStepState::Active;
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
