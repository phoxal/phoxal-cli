//! [`SessionController`]: the one lifecycle owner for `run`/`simulation run`.
//! It owns terminal acquisition/restoration, the
//! root [`CancellationToken`], the current [`SessionState`], the bounded
//! [`SessionEvent`] receiver, and owns the TUI selected by the command's hard
//! TTY gate - replacing the old `Stepper` + display-activation
//! handoff + supervisor-owned `Display` design.
//!
//! # Cancellation (fixes live-acceptance #4, finding A1)
//!
//! The controller starts the renderer FIRST, then the caller (`run`/
//! `simulation run`) drives preparation ([`SessionController::drive_prepare_phase`]),
//! any intermediate setup ([`SessionController::drive_setup`] - `simulation
//! run`'s Webots preflight/locking/staging/spawn-responder startup), and
//! finally supervision ([`SessionController::drive_supervision`]) through it.
//! All three share ONE underlying select loop
//! ([`SessionController::drive_cancelable`] for the first two) that polls TUI
//! input, `tokio::signal::ctrl_c()`, session events, and redraws THROUGHOUT -
//! not just once supervision begins. This matters because raw mode (a real
//! TUI session) disables the terminal's own `ISIG`, so Ctrl-C never becomes a
//! `SIGINT` at all; it arrives only as a crossterm key event. Before this
//! fix, `drive_preparation` polled only the (useless, under raw mode)
//! `ctrl_c()` signal and simulate's own intermediate setup was not driven
//! through the controller at all - a raw-mode Ctrl-C during a fresh-cache
//! build or Webots staging was simply never observed.
//!
//! During preparation/setup the first Ctrl-C cancels the root token, stops
//! every captured child, and waits for the owned worker before restoring the
//! terminal and exiting. This is required because raw mode turns Ctrl-C into
//! a key event rather than delivering SIGINT to the child's process group.
//! During supervision the first Ctrl-C instead cancels the root token
//! (letting the supervisor run its own orderly
//! `request_participant_stop`/`shutdown_all`) and waits for that to finish; a
//! second Ctrl-C forces an immediate exit if teardown is not prompt enough.
//!
//! # Cleanup (finding B1/D3)
//!
//! [`SessionController`] restores the terminal and uninstalls diagnostics
//! routing on EVERY exit path, not just an explicit call some branches used
//! to skip: [`Drop`] uninstalls diagnostics routing unconditionally, and
//! dropping the TUI field (either via `Drop` or the explicit process-exit
//! path's [`SessionController::teardown`]) restores the terminal through its
//! `TerminalGuard` chain. A terminal
//! draw/input failure inside [`SessionController::drive_supervision`] no
//! longer just tears down the renderer and returns: it cancels the root
//! token and AWAITS the supervisor's `JoinHandle` first (see
//! `finish_after_failure`), so a dropped `JoinHandle` never silently detaches
//! the supervisor (and every child it owns) in the background.
//!
//! # TUI
//!
//! A real TTY selects [`crate::tui::TuiDisplay`], activated immediately (before preparation
//!   even starts) rather than gated behind a stepper hand-off, so the
//!   alternate screen is the ONE surface from command start to shutdown.
//!   Every [`SessionEvent`] this controller applies (phase/diagnostic/session
//!   transitions) is forwarded into the TUI via
//!   [`crate::tui::TuiDisplay::apply_session_event`], which drives a
//!   dedicated startup surface (session metadata + one active phase row + a
//!   direct jump to Logs for errors) until the session reaches `Running`, then
//!   collapses to the existing runtime navigator -
//!   see `crate::tui::startup`'s module docs for the pure model.

use std::future::Future;
use std::io;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use crossterm::event::Event;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use phoxal_cli_ui::tui::{
    DisplayAction, TerminalGuard, TuiDisplay, install_panic_hook, render::TitleInfo,
};

const FORCED_REAP_TIMEOUT: Duration = Duration::from_secs(1);
const OWNED_TASK_CANCEL_TIMEOUT: Duration = Duration::from_secs(1);
use crate::supervisor::{BoardBackend, BoardSnapshot, SupervisorAction, SupervisorOutcome};
use crate::telemetry::TelemetryBackend;
use phoxal_cli_core::session::JoypadCommand;

use super::diagnostics;
use super::output::OutputContext;
use phoxal_cli_core::session::SessionMode;
use phoxal_cli_core::session::event::{
    DiagnosticLevel, DiagnosticSource, PhaseId, PhaseOutcome, SessionEvent,
};
use phoxal_cli_core::session::state::{FailReason, SessionState};

/// Bound on the [`SessionEvent`] channel: the only source of startup and
/// runtime transitions the renderer sees. Generous
/// enough that ordinary bursts (several stages finishing close together)
/// never drop an event; a slow/stuck consumer is a bug to fix, not a queue to
/// grow unboundedly for.
const EVENT_CHANNEL_CAPACITY: usize = 256;

fn session_title(project_root: &Path, mode: SessionMode) -> TitleInfo {
    let mut title = unknown_session_title(mode);
    let Ok(manifest_path) = phoxal_cli_core::project::resolver::discover_robot_yaml(project_root)
    else {
        return title;
    };
    let Ok(robot) = phoxal_cli_core::project::resolver::load_robot(&manifest_path) else {
        return title;
    };
    title.robot = robot.robot.id;
    title.namespace = robot.robot.namespace;
    title.channel = robot.artifacts.channel.as_str().to_string();
    title.manifest = display_manifest_path(&manifest_path, project_root);
    title
}

fn display_manifest_path(manifest_path: &Path, project_root: &Path) -> String {
    let display_path =
        pathdiff::diff_paths(manifest_path, project_root).unwrap_or_else(|| manifest_path.into());
    let display = display_path.display().to_string();
    if display_path.is_relative()
        && !matches!(
            display_path.components().next(),
            Some(std::path::Component::ParentDir)
        )
    {
        format!("./{display}")
    } else {
        display
    }
}

fn unknown_session_title(mode: SessionMode) -> TitleInfo {
    TitleInfo {
        robot: "unknown".to_string(),
        namespace: "unknown".to_string(),
        channel: "unknown".to_string(),
        manifest: "n/a".to_string(),
        mode,
        bus_endpoint: crate::supervisor::default_connect_endpoint(),
        started_at: std::time::SystemTime::now(),
        started_instant: std::time::Instant::now(),
    }
}

/// The one lifecycle owner for a `run`/`simulation run` session. See the
/// module docs.
pub struct SessionController {
    output: OutputContext,
    token: CancellationToken,
    state: SessionState,
    events_tx: mpsc::Sender<SessionEvent>,
    events_rx: mpsc::Receiver<SessionEvent>,
    tui: Option<Box<TuiDisplay>>,
    interrupts: tokio::signal::unix::Signal,
    terminates: tokio::signal::unix::Signal,
    hangups: tokio::signal::unix::Signal,
    /// Lets the TUI's `r restart` input reach the supervisor without the
    /// supervisor knowing about ratatui/input at all: the same
    /// `SupervisorAction` channel `run`/`simulation run` already wires up for
    /// `--watch` hot-reload, shared here rather than duplicated.
    restart_tx: Option<mpsc::Sender<SupervisorAction>>,
}

impl SessionController {
    /// Build the controller and start its renderer immediately - for the TUI
    /// this means entering the alternate screen right now, before the
    /// caller's preparation has even started, so there is exactly one
    /// interactive surface for the whole session (Product decision 1).
    pub fn new(output: OutputContext, mode: SessionMode, project_root: &Path) -> io::Result<Self> {
        Self::build(output, session_title(project_root, mode), true)
    }

    fn build(output: OutputContext, title: TitleInfo, activate_tui: bool) -> io::Result<Self> {
        install_panic_hook();
        let (events_tx, events_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        // Install every process-level terminal shutdown signal once and keep
        // the receivers for the controller lifetime. Tokio keeps the global
        // handler installed after a receiver is dropped, so recreating these
        // per phase would swallow signals in the setup/supervision gap.
        let interrupts = signal(SignalKind::interrupt())?;
        let terminates = signal(SignalKind::terminate())?;
        let hangups = signal(SignalKind::hangup())?;

        let tui = if activate_tui {
            if !output.interactive || !TerminalGuard::should_use_terminal(&io::stderr()) {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "interactive `run` and `simulation run` sessions require a terminal; run this command in a TTY",
                ));
            }
            let mut tui = Box::new(TuiDisplay::new(output.theme, title));
            tui.activate()?;
            Some(tui)
        } else {
            None
        };

        // Do this only after terminal activation succeeds. Otherwise a failed
        // constructor would leave a process-global diagnostics sender pointing
        // at a receiver that was immediately dropped.
        diagnostics::install(events_tx.clone());
        Ok(Self {
            output,
            token: CancellationToken::new(),
            state: SessionState::Preparing,
            events_tx,
            events_rx,
            tui,
            interrupts,
            terminates,
            hangups,
            restart_tx: None,
        })
    }

    #[cfg(test)]
    fn new_for_test(mode: SessionMode) -> io::Result<Self> {
        Self::build(
            OutputContext::new(
                false,
                phoxal_cli_ui::Theme::new(phoxal_cli_ui::ColorCapability::None),
            ),
            unknown_session_title(mode),
            false,
        )
    }

    /// The root cancellation signal: pass a clone into `SupervisorOptions`
    /// and every other cancel-safe wait (including stage waits) so
    /// one Ctrl-C reaches all of them.
    #[must_use]
    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    /// The event sender: clone it into preparation and the supervisor so
    /// both can emit [`SessionEvent`]s the TUI consumes.
    #[must_use]
    pub fn events(&self) -> mpsc::Sender<SessionEvent> {
        self.events_tx.clone()
    }

    /// Wire a TUI's `r restart` key to the supervisor's own action channel
    /// (the same one `--watch` hot-reload already uses). A no-op call is
    pub fn set_restart_channel(&mut self, tx: mpsc::Sender<SupervisorAction>) {
        self.restart_tx = Some(tx);
    }

    pub fn set_bus_endpoint(&mut self, endpoint: String) {
        if let Some(tui) = &mut self.tui {
            tui.set_bus_endpoint(endpoint);
        }
    }

    /// The current output context, e.g. for a caller deciding whether an
    /// interactive wait may run unbounded (see `crate::run`/`simulate`'s
    /// `interactive_wait_budget`).
    #[must_use]
    pub const fn output(&self) -> OutputContext {
        self.output
    }

    /// `true` once the TUI owns the
    /// terminal - the gate `run`/`simulation run` use to decide whether
    /// subscribing to the extra live-telemetry bus feeds (host/
    /// router/joypad) is worth the connections, since only the TUI can
    /// render them.
    #[must_use]
    pub const fn renders_tui(&self) -> bool {
        self.tui.is_some()
    }

    /// Drive one bracketed "Preparing" phase while `prepare` (a blocking
    /// closure - the caller's own `prepare_run`/`prepare_with_mode`) runs on
    /// a `spawn_blocking` worker. A single phase, not a pre-declared
    /// multi-phase list: it exists only while preparation is genuinely
    /// running (Product decision 3), and any `Ui::info`/`warn` line
    /// preparation itself emits (e.g. a per-crate `cargo build`) still
    /// reaches the renderer as its own `SessionEvent::Diagnostic` - see
    /// `crate::ui::Ui`'s session routing. Finer per-operation phases
    /// (download/validate/build shown independently, only when each
    /// genuinely runs) are deferred to a later wave; see the crate's
    /// follow-up plan.
    pub async fn drive_prepare_phase<T, F>(&mut self, prepare: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T> + Send + 'static,
    {
        let started = std::time::Instant::now();
        // Applied directly rather than round-tripped through `events_tx` -
        // the controller is the only reader of that channel and has not
        // started its own select loop yet, so an awaited send here (the norm
        // for a lifecycle event - finding B5) would deadlock against nothing
        // draining it; a direct `apply_event` has the exact same effect
        // (state reduction + renderer forwarding) without that risk.
        self.apply_event(SessionEvent::PhaseStarted {
            id: PhaseId::new("prepare"),
            label: "Preparing".to_string(),
        });
        let result = self
            .drive_cancelable(tokio::task::spawn_blocking(prepare))
            .await;
        let outcome = match &result {
            Ok(_) => PhaseOutcome::Succeeded,
            Err(error) => PhaseOutcome::Failed {
                error: format!("{error:#}"),
            },
        };
        self.apply_event(SessionEvent::PhaseFinished {
            id: PhaseId::new("prepare"),
            outcome,
            elapsed: started.elapsed(),
        });
        // Best-effort: show the phase's final outcome for at least one frame
        // before the caller's `?` can propagate an error and drop this
        // controller. Errors are ignored here (not the `Err` this method
        // returns) - a genuine terminal failure at this exact instant is
        // vanishingly rare and would only affect this one cosmetic frame.
        let _ = self.redraw(&BoardSnapshot::default(), &TelemetryBackend::default());
        result
    }

    /// Drive `setup` (an async unit of work that runs BETWEEN preparation and
    /// supervision - `simulation run`'s Webots preflight, lock acquisition,
    /// world/controller staging, and spawn-responder startup) through the
    /// SAME cancelable loop as preparation (finding A1's "intermediate setup"
    /// gap). Unlike [`Self::drive_prepare_phase`]'s `spawn_blocking` (not
    /// itself interruptible mid-closure), `setup` runs as a normal
    /// `tokio::spawn`ed task, so it stays cooperatively cancellable at its
    /// own `.await` points even before a Ctrl-C forces the whole process to
    /// exit.
    pub async fn drive_setup<T, Fut>(&mut self, setup: Fut) -> Result<T>
    where
        T: Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        self.drive_cancelable(tokio::spawn(setup)).await
    }

    /// The shared loop behind [`Self::drive_prepare_phase`] and
    /// [`Self::drive_setup`] (finding A1): polls TUI input, `ctrl_c()`,
    /// session events, and redraws in one `select!` until `task` completes.
    /// No board/telemetry exist yet at this point in the session, so redraws
    /// show an empty participant list; the renderer still shows every
    /// [`SessionEvent`] phase/diagnostic `task` emits. Returns `task`'s own
    /// result, or a terminal-input `Err`. Ctrl-C first cancels and joins the
    /// owned task (including registered child processes) before restoring the
    /// terminal and exiting; it never exits while preparation/setup work is
    /// detached.
    async fn drive_cancelable<T: Send + 'static>(
        &mut self,
        mut task: JoinHandle<Result<T>>,
    ) -> Result<T> {
        let empty_board = BoardSnapshot::default();
        let empty_telemetry = TelemetryBackend::default();
        // Draw immediately, before waiting on anything - otherwise a phase
        // event applied just before this call (e.g. `drive_prepare_phase`'s
        // "Preparing" row) would not actually appear until the first event/
        // tick, which could be a visible delay on a slow host.
        if let Err(error) = self.redraw(&empty_board, &empty_telemetry) {
            self.cancel_owned_task(&mut task).await;
            return Err(error);
        }
        let mut ticker = tokio::time::interval(Duration::from_millis(100));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // The initial frame above already represents the bootstrap tick.
        ticker.tick().await;
        loop {
            tokio::select! {
                biased;
                _ = recv_shutdown_signal(&mut self.interrupts, &mut self.terminates, &mut self.hangups) => {
                    self.cancel_owned_task(&mut task).await;
                    self.teardown();
                    std::process::exit(130);
                }
                Some(input) = poll_next_input(&mut self.tui) => {
                    match input {
                        Ok(event) => {
                            // Navigation and resize input still update the
                            // startup UI immediately. Quit is the only action
                            // with an external effect before supervision;
                            // restart/joypad actions have no target yet.
                            let action = handle_input(&mut self.tui, event, &empty_board);
                            if matches!(action, DisplayAction::Quit) {
                                self.cancel_owned_task(&mut task).await;
                                self.teardown();
                                std::process::exit(130);
                            }
                            if let Err(error) = self.redraw(&empty_board, &empty_telemetry) {
                                self.cancel_owned_task(&mut task).await;
                                return Err(error);
                            }
                        }
                        Err(error) => {
                            self.cancel_owned_task(&mut task).await;
                            return Err(anyhow!(error).context("terminal input reader failed"));
                        }
                    }
                }
                Some(event) = self.events_rx.recv() => {
                    self.apply_event(event);
                    if let Err(error) = self.redraw(&empty_board, &empty_telemetry) {
                        self.cancel_owned_task(&mut task).await;
                        return Err(error);
                    }
                }
                _ = ticker.tick() => {
                    if let Err(error) = self.redraw(&empty_board, &empty_telemetry) {
                        self.cancel_owned_task(&mut task).await;
                        return Err(error);
                    }
                }
                result = &mut task => {
                    return match result {
                        Ok(inner) => inner,
                        Err(join_error) => Err(anyhow!(join_error)),
                    };
                }
            }
        }
    }

    /// Cancel and join pre-supervision work before terminal teardown. A
    /// preparation worker may own a captured cargo/tar child, so killing every
    /// registered child first is essential: raw-mode Ctrl-C is a key event,
    /// not a process-group SIGINT. Joining the task prevents an error path
    /// from silently detaching setup work in the background.
    async fn cancel_owned_task<T>(&mut self, task: &mut JoinHandle<Result<T>>) {
        // A setup task can itself be awaiting a lifecycle send. Once this
        // controller is leaving pre-supervision, no one will drain events, so
        // close the receiver before joining to make that send fail promptly.
        self.events_rx.close();
        self.token.cancel();
        self.transition_to_stopping();
        diagnostics::kill_active_children();
        if tokio::time::timeout(OWNED_TASK_CANCEL_TIMEOUT, &mut *task)
            .await
            .is_err()
        {
            task.abort();
        }
    }

    /// Drive the controller for the rest of the session once board/telemetry
    /// exist: redraws on a ticker, forwards `SessionEvent`s, bridges TUI
    /// input (restart/joypad) back to the supervisor/telemetry, and waits for
    /// `supervise` (the spawned `supervise_until_shutdown` task) to finish or
    /// Ctrl-C to cancel it. Consumes `self` so the TUI and terminal are torn
    /// down exactly once, on every return path.
    ///
    /// `runtime_store` carries this session's launch-time participant
    /// metadata (finding A5 - artifact reference, declared contracts,
    /// ownership) resolved once by the caller from its `LaunchPlan` and
    /// contract-check outcome; installed into the TUI alongside the board's
    /// log sink, right before this loop starts.
    pub async fn drive_supervision(
        mut self,
        board: BoardBackend,
        telemetry: TelemetryBackend,
        runtime_store: phoxal_cli_core::session::stores::runtime::RuntimeStore,
        orderly_shutdown_timeout: Duration,
        mut supervise: JoinHandle<Result<SupervisorOutcome>>,
    ) -> Result<SupervisorOutcome> {
        if let Some(tui) = &mut self.tui {
            board.set_log_sink(tui.log_sender());
            tui.set_runtime_store(runtime_store);
        }
        let mut ticker = tokio::time::interval(Duration::from_millis(100));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut cancel_requested = false;
        let mut cancel_deadline = None;
        let end = loop {
            tokio::select! {
                biased;
                _ = recv_shutdown_signal(&mut self.interrupts, &mut self.terminates, &mut self.hangups) => {
                    if register_cancel_request(&mut cancel_requested) {
                        self.force_exit_supervision(&board, &mut supervise).await;
                    }
                    self.token.cancel();
                    cancel_deadline = Some(tokio::time::Instant::now() + orderly_shutdown_timeout);
                    self.transition_to_stopping();
                    if let Err(error) = self.redraw_live(&board, &telemetry) {
                        break SupervisionEnd::Failed(error);
                    }
                }
                Some(event) = self.events_rx.recv() => {
                    self.apply_event(event);
                    if let Err(error) = self.redraw_live(&board, &telemetry) {
                        break SupervisionEnd::Failed(error);
                    }
                }
                Some(input) = poll_next_input(&mut self.tui) => {
                    match input {
                        Ok(event) => {
                            let board_snapshot = board.snapshot();
                            let action = handle_input(&mut self.tui, event, &board_snapshot);
                            if action == DisplayAction::Quit
                                && register_cancel_request(&mut cancel_requested)
                            {
                                self.force_exit_supervision(&board, &mut supervise).await;
                            }
                            if action == DisplayAction::Quit && cancel_deadline.is_none() {
                                cancel_deadline = Some(tokio::time::Instant::now() + orderly_shutdown_timeout);
                            }
                            self.apply_display_action(action, &telemetry);
                            if let Err(error) = self.redraw_live(&board, &telemetry) {
                                break SupervisionEnd::Failed(error);
                            }
                        }
                        Err(error) => {
                            break SupervisionEnd::Failed(anyhow!(error).context("terminal input reader failed"));
                        }
                    }
                }
                _ = ticker.tick() => {
                    if let Err(error) = self.redraw_live(&board, &telemetry) {
                        break SupervisionEnd::Failed(error);
                    }
                }
                _ = wait_for_cancel_deadline(cancel_deadline), if cancel_deadline.is_some() => {
                    self.force_exit_supervision(&board, &mut supervise).await;
                }
                result = &mut supervise => {
                    break SupervisionEnd::Finished(match result {
                        Ok(inner) => inner,
                        Err(join_error) => Err(anyhow!(join_error)),
                    });
                }
            }
        };
        // Finding B1: a terminal draw/input failure must not just tear down
        // the TUI and leave the supervisor (and every child it owns)
        // detached in the background. `self` (and its terminal/diagnostics
        // routing) is restored via `Drop` once this function returns - see
        // the module docs - so there is no explicit `teardown()` call on this
        // path any more.
        match end {
            SupervisionEnd::Finished(result) => {
                // Supervision may finish while a watch/setup worker is inside
                // spawn_blocking and still owns a registered cargo process
                // group. Aborting the Tokio task cannot interrupt that
                // blocking wait, so stop registered children before Drop
                // uninstalls the session registry.
                diagnostics::kill_active_children();
                self.reflect_final_outcome(&board, &telemetry, &result);
                result
            }
            SupervisionEnd::Failed(error) => {
                finish_after_failure(&mut self.events_rx, &self.token, &board, supervise, error)
                    .await
            }
        }
    }

    async fn force_exit_supervision(
        &mut self,
        board: &BoardBackend,
        supervise: &mut JoinHandle<Result<SupervisorOutcome>>,
    ) -> ! {
        self.events_rx.close();
        self.token.cancel();
        diagnostics::kill_active_children();
        crate::supervisor::force_kill_supervised_process_groups(board);
        if tokio::time::timeout(FORCED_REAP_TIMEOUT, &mut *supervise)
            .await
            .is_err()
        {
            // Catch a participant that completed its spawn while the first
            // snapshot was being killed, then stop the no-longer-useful owner
            // task. At this point every board-visible process group has
            // received SIGKILL, so aborting cannot orphan a live child.
            crate::supervisor::force_kill_supervised_process_groups(board);
            supervise.abort();
            let _ = supervise.await;
        }
        self.teardown();
        std::process::exit(130);
    }

    /// Render the terminal session state once before the controller and its
    /// renderer are consumed. Participant failures remain visible on the
    /// board while supervision is running and do not choose this state: a
    /// user-requested cancellation is a clean `Stopped` session even when its
    /// final snapshot contains failed participants. Only an internal supervisor
    /// error becomes `Failed`.
    fn reflect_final_outcome(
        &mut self,
        board: &BoardBackend,
        telemetry: &TelemetryBackend,
        result: &Result<SupervisorOutcome>,
    ) {
        let next = match result {
            Ok(_) => self.state.clone().to_stopped(),
            Err(error) => self
                .state
                .clone()
                .to_failed(FailReason::Terminal(format!("{error:#}"))),
        };
        if let Ok(next) = next {
            self.state = next;
            self.emit_session_changed_locally();
            let _ = self.redraw_live(board, telemetry);
        }
    }

    fn apply_display_action(&mut self, action: DisplayAction, telemetry: &TelemetryBackend) {
        match action {
            DisplayAction::None => {}
            DisplayAction::Quit => {
                if !self.token.is_cancelled() {
                    self.token.cancel();
                    self.transition_to_stopping();
                }
            }
            DisplayAction::Restart(id) => {
                let failure = if let Some(tx) = &self.restart_tx {
                    let action = SupervisorAction::Restart { id: id.clone() };
                    match tx.try_send(action) {
                        Ok(()) => None,
                        Err(mpsc::error::TrySendError::Closed(_)) => Some(format!(
                            "restart request for {id} was not sent because supervision ended"
                        )),
                        Err(mpsc::error::TrySendError::Full(_)) => Some(format!(
                            "restart request for {id} was not sent because the supervisor is busy; try again"
                        )),
                    }
                } else {
                    Some(format!(
                        "restart request for {id} is unavailable in this session"
                    ))
                };
                if let Some(message) = failure {
                    self.apply_event(SessionEvent::Diagnostic {
                        source: DiagnosticSource::Cli,
                        level: DiagnosticLevel::Warn,
                        message,
                    });
                }
            }
            DisplayAction::JoypadSelect(id) => {
                telemetry.send_joypad_command(JoypadCommand::Select(id));
            }
            DisplayAction::JoypadSetEnabled(enabled) => {
                telemetry.send_joypad_command(JoypadCommand::SetEnabled(enabled));
            }
            DisplayAction::JoypadRescan => {
                telemetry.send_joypad_command(JoypadCommand::Rescan);
            }
        }
    }

    fn transition_to_stopping(&mut self) {
        if let Ok(next) = self.state.clone().to_stopping() {
            self.state = next;
            self.emit_session_changed_locally();
        }
    }

    /// Reflect a controller-internal state change (Ctrl-C -> Stopping) into
    /// the renderer the same way an externally emitted `SessionChanged` event
    /// would - without looping it back through the bounded channel, which
    /// exists for producers OUTSIDE the controller (preparation, the
    /// supervisor), not for the controller's own transitions.
    fn emit_session_changed_locally(&mut self) {
        let event = SessionEvent::SessionChanged {
            state: self.state.clone(),
        };
        self.forward_to_renderer(&event);
    }

    /// Apply one incoming event. `SessionChanged` is pre-validated by its
    /// producer. `StagedStartupComplete` is the one observation the
    /// controller reduces itself, through the state machine's transition to
    /// `Running`.
    fn apply_event(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::StagedStartupComplete => {
                self.apply_reduced_state(reduce_staged_startup_complete(self.state.clone()));
            }
            other => {
                self.state = reduce_state(self.state.clone(), &other);
                self.forward_to_renderer(&other);
            }
        }
    }

    /// Adopt `next` and forward a synthesized `SessionChanged` to the
    /// renderer only if it actually differs from the current state - an
    /// observation reduction rejected by `SessionState` returns the state
    /// unchanged and must not spam the renderer with a no-op transition.
    fn apply_reduced_state(&mut self, next: SessionState) {
        if next != self.state {
            self.state = next.clone();
            self.forward_to_renderer(&SessionEvent::SessionChanged { state: next });
        }
    }

    /// Fold one event into the TUI startup model so the next redraw shows it.
    fn forward_to_renderer(&mut self, event: &SessionEvent) {
        if let Some(tui) = &mut self.tui {
            tui.apply_session_event(event);
        }
    }

    /// Draw one frame. An `Err` is a genuine terminal I/O failure (the TUI's
    /// underlying tty went away) - the caller fails the session rather than
    /// silently leaving a stale screen up forever.
    fn redraw(&mut self, board: &BoardSnapshot, telemetry: &TelemetryBackend) -> Result<()> {
        match &mut self.tui {
            Some(tui) => tui
                .redraw(board, telemetry.snapshot())
                .context("failed to draw the interactive session frame"),
            None => Ok(()),
        }
    }

    fn redraw_live(&mut self, board: &BoardBackend, telemetry: &TelemetryBackend) -> Result<()> {
        self.redraw(&board.snapshot(), telemetry)
    }

    /// Restore the terminal (if a TUI is active) and stop routing tracing
    /// through this session's event channel, SYNCHRONOUSLY. Only needed on
    /// the post-cancellation `std::process::exit` path: `exit` skips every
    /// `Drop` impl, so it must call this explicitly first. Every OTHER exit
    /// path (normal return, `?`, an early `return`) relies on `Drop` instead
    /// (see below) - idempotent either way, since `Drop` runs on an
    /// already-empty TUI harmlessly.
    fn teardown(&mut self) {
        diagnostics::uninstall();
        // Dropping the old TUI value here (rather than waiting for
        // `Self`'s own end-of-scope drop) restores the terminal
        // SYNCHRONOUSLY, so it is safe to call `std::process::exit`
        // immediately afterward - `exit` never runs `Drop` impls.
        self.tui = None;
    }
}

fn register_cancel_request(cancel_requested: &mut bool) -> bool {
    std::mem::replace(cancel_requested, true)
}

async fn recv_shutdown_signal(
    interrupts: &mut tokio::signal::unix::Signal,
    terminates: &mut tokio::signal::unix::Signal,
    hangups: &mut tokio::signal::unix::Signal,
) {
    tokio::select! {
        _ = interrupts.recv() => {}
        _ = terminates.recv() => {}
        _ = hangups.recv() => {}
    }
}

async fn wait_for_cancel_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Finding B1/D3: every OTHER exit path (a normal return, an early `?`, a
/// panic unwind) restores the terminal and uninstalls diagnostics routing
/// through this `Drop` impl instead of a call to `teardown()` a future code
/// path could forget. Uninstalling diagnostics here is what closes the gap:
/// dropping `self.tui` (a struct field, dropped automatically after this
/// method body runs) already restored the terminal on its own for the `Tui`
/// variant (`TerminalGuard`'s own `Drop`), but nothing previously guaranteed
/// diagnostics routing was ALSO torn down on every path.
impl Drop for SessionController {
    fn drop(&mut self) {
        diagnostics::uninstall();
    }
}

/// How [`SessionController::drive_supervision`]'s select loop ended: either
/// the supervisor task itself resolved (`Finished`, already carrying its own
/// result), or a terminal draw/input failure broke the loop before the
/// supervisor did (`Failed`, still holding the ORIGINAL `supervise` handle
/// unresolved).
enum SupervisionEnd {
    Finished(Result<SupervisorOutcome>),
    Failed(anyhow::Error),
}

/// Shared tail for a `Failed` [`SupervisionEnd`] (finding B1): cancels the
/// root token (idempotent - a harmless no-op if Ctrl-C already cancelled it)
/// and AWAITS `supervise` before returning, so the supervisor's own orderly
/// teardown (`request_participant_stop`/`shutdown_all`) always completes and
/// nothing is left running detached in the background. The terminal failure
/// `error` is still what gets returned - it is the actual cause, not
/// whatever the supervisor itself eventually returns once cancelled.
async fn finish_after_failure(
    events_rx: &mut mpsc::Receiver<SessionEvent>,
    token: &CancellationToken,
    board: &BoardBackend,
    mut supervise: JoinHandle<Result<SupervisorOutcome>>,
    error: anyhow::Error,
) -> Result<SupervisorOutcome> {
    // Closing wakes an awaited lifecycle send with an immediate error. Without
    // this, a full channel could deadlock failure teardown after the
    // controller stopped draining it.
    events_rx.close();
    token.cancel();
    diagnostics::kill_active_children();
    crate::supervisor::force_kill_supervised_process_groups(board);
    if tokio::time::timeout(FORCED_REAP_TIMEOUT, &mut supervise)
        .await
        .is_err()
    {
        crate::supervisor::force_kill_supervised_process_groups(board);
        supervise.abort();
        let _ = supervise.await;
    }
    Err(error)
}

/// Poll the active TUI for its next input event, if any. A free
/// function taking only the TUI field (rather than a `SessionController`
/// method taking `&mut self`) so its future, used directly inside
/// `drive_supervision`'s `tokio::select!`, borrows only the `renderer` field -
/// disjoint from the `events_rx` field another branch borrows in the same
/// `select!` invocation. The test-only empty case never resolves.
/// `Some(Err(_))` is a genuine terminal read failure (see
/// `TuiDisplay::next_input`'s docs) - the caller fails the session rather
/// than silently going input-deaf.
async fn poll_next_input(tui: &mut Option<Box<TuiDisplay>>) -> Option<io::Result<Event>> {
    match tui {
        Some(tui) => match tui.next_input().await {
            Ok(Some(event)) => Some(Ok(event)),
            Ok(None) => std::future::pending().await,
            Err(error) => Some(Err(error)),
        },
        None => std::future::pending().await,
    }
}

fn handle_input(
    tui: &mut Option<Box<TuiDisplay>>,
    event: Event,
    board: &BoardSnapshot,
) -> DisplayAction {
    match tui {
        Some(tui) => tui.handle_input(event, board),
        None => DisplayAction::None,
    }
}

/// Pure reduction of the controller's tracked [`SessionState`] against one
/// incoming event. Free of any I/O so a test can drive it directly over a
/// fake event stream (see this module's tests) - the actual transition
/// legality is enforced by whoever CONSTRUCTS the `SessionChanged` event
/// (preparation/the supervisor calling `SessionState`'s own transition
/// methods), not re-validated here.
#[must_use]
fn reduce_state(current: SessionState, event: &SessionEvent) -> SessionState {
    match event {
        SessionEvent::SessionChanged { state } => state.clone(),
        _ => current,
    }
}

/// Reduce `SessionEvent::StagedStartupComplete` into the next `SessionState`
/// once every staged-startup stage has finished. It is a no-op for a session
/// that is not in a state `to_running` accepts (for example, already
/// `Stopping`).
#[must_use]
fn reduce_staged_startup_complete(current: SessionState) -> SessionState {
    current.clone().to_running().unwrap_or(current)
}

#[cfg(test)]
mod tests {
    use phoxal_cli_core::session::event::DiagnosticSource;

    use super::*;

    fn changed(state: SessionState) -> SessionEvent {
        SessionEvent::SessionChanged { state }
    }

    #[test]
    fn manifest_display_prefixes_only_paths_inside_the_project() {
        let project = Path::new("/workspace/robot");
        assert_eq!(
            display_manifest_path(Path::new("/workspace/robot/robot.yaml"), project),
            "./robot.yaml"
        );
        assert_eq!(
            display_manifest_path(Path::new("/workspace/robot.yaml"), project),
            "../robot.yaml"
        );
    }

    /// The controller's own reducer over a fake event stream must follow the
    /// exact chain a real session drives:
    /// `Preparing -> Starting -> Running -> Stopping -> Stopped`.
    /// Each state is built through `SessionState`'s own
    /// transition methods (never constructed directly), so this doubles as
    /// proof the chain is legal per the state machine's own rules.
    #[test]
    fn reduce_state_follows_the_full_lifecycle_over_a_fake_event_stream() {
        let preparing = SessionState::Preparing;
        let starting = preparing.clone().start().expect("start");
        let running = starting.clone().to_running().expect("running");
        let stopping = running.clone().to_stopping().expect("stopping");
        let stopped = stopping.clone().to_stopped().expect("stopped");

        let mut state = SessionState::Preparing;
        for next in [starting, running, stopping, stopped.clone()] {
            state = reduce_state(state, &changed(next));
        }
        assert_eq!(state, stopped);
        assert!(state.is_terminal());
    }

    /// Events other than `SessionChanged` must never mutate the tracked
    /// state - only the session's own authoritative transition decides it.
    #[test]
    fn reduce_state_ignores_non_session_events() {
        let state = SessionState::Preparing.start().unwrap();
        let untouched = reduce_state(
            state.clone(),
            &SessionEvent::Diagnostic {
                source: DiagnosticSource::Tracing,
                level: DiagnosticLevel::Warn,
                message: "noise".to_string(),
            },
        );
        assert_eq!(untouched, state);
    }

    /// A cancellation token observed mid-wait must resolve a `select!`
    /// immediately rather than after some long fixed timeout - the exact
    /// property `drive_preparation`/`drive_supervision` rely on to fix
    /// live-acceptance #4. Exercises the underlying primitive directly
    /// (racing a token against a would-be-60s sleep) since driving the whole
    /// controller would require a real terminal/tokio::signal in CI.
    #[tokio::test]
    async fn cancellation_token_resolves_a_select_promptly_not_after_a_long_wait() {
        let token = CancellationToken::new();
        let waiter = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            waiter.cancel();
        });

        let outcome = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::select! {
                () = token.cancelled() => true,
                () = tokio::time::sleep(Duration::from_secs(60)) => false,
            }
        })
        .await
        .expect("the select must resolve well within 2s, long before the 60s branch");
        assert!(
            outcome,
            "cancellation must win the race against a 60s wait, not time out"
        );
    }

    /// `StagedStartupComplete` is the only path to `Running`; it must reach
    /// `Running` from `Starting`, and must not resurrect a session already
    /// on to `Stopping`.
    #[test]
    fn reduce_staged_startup_complete_reaches_running_but_never_past_stopping() {
        let starting = SessionState::Preparing.start().unwrap();
        assert_eq!(
            reduce_staged_startup_complete(starting),
            SessionState::Running
        );

        let stopping = SessionState::Preparing
            .start()
            .unwrap()
            .to_stopping()
            .unwrap();
        assert_eq!(
            reduce_staged_startup_complete(stopping.clone()),
            stopping,
            "staged-startup completing after Ctrl-C must not resurrect Running"
        );
    }

    #[tokio::test]
    async fn failed_participant_outcome_still_finishes_as_stopped() {
        let _guard = super::super::diagnostics::DIAGNOSTICS_TEST_LOCK
            .lock()
            .await;
        let mut controller =
            SessionController::new_for_test(SessionMode::Run).expect("test controller");
        controller.state = SessionState::Preparing
            .start()
            .unwrap()
            .to_stopping()
            .unwrap();
        let board = BoardBackend::new();
        board.upsert(crate::supervisor::ParticipantStatus::new(
            "failed-binary",
            phoxal_cli_core::session::ParticipantKind::Service,
            crate::supervisor::ParticipantState::Failed,
        ));

        controller.reflect_final_outcome(
            &board,
            &TelemetryBackend::new(),
            &Ok(SupervisorOutcome {
                failed_participants: vec!["failed-binary".to_string()],
            }),
        );

        assert_eq!(controller.state, SessionState::Stopped);
    }

    /// Finding B1: the CRITICAL regression - a controller failure (a
    /// terminal draw/input error) must never just drop the supervisor's
    /// `JoinHandle` and leave it (and every child it owns) detached in the
    /// background. `finish_after_failure` is `drive_supervision`'s shared
    /// tail for exactly that case; this proves it actually awaits the
    /// supervisor to completion (via a flag the spawned task itself flips
    /// only once it finishes) rather than merely dropping the handle, AND
    /// that the root token is cancelled so the supervisor's own orderly
    /// teardown runs in the first place.
    #[tokio::test]
    async fn finish_after_failure_cancels_the_token_and_awaits_the_supervisor_task() {
        let token = CancellationToken::new();
        let (_events_tx, mut events_rx) = mpsc::channel(1);
        let completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let completed_in_task = completed.clone();
        let supervise = tokio::spawn(async move {
            // A short delay so a caller that merely dropped the handle
            // (instead of awaiting it) would very likely observe `completed`
            // still `false` - making a regression back to "drop and return"
            // an easy, reliable test failure rather than a rare flake.
            tokio::time::sleep(Duration::from_millis(20)).await;
            completed_in_task.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(SupervisorOutcome {
                failed_participants: Vec::new(),
            })
        });
        let board = BoardBackend::new();

        let result = finish_after_failure(
            &mut events_rx,
            &token,
            &board,
            supervise,
            anyhow!("terminal draw failed"),
        )
        .await;

        assert!(
            token.is_cancelled(),
            "the root token must be cancelled so the supervisor's own orderly teardown runs"
        );
        assert!(
            completed.load(std::sync::atomic::Ordering::SeqCst),
            "the supervisor task must be awaited to completion, never left detached"
        );
        assert_eq!(result.unwrap_err().to_string(), "terminal draw failed");
    }

    #[tokio::test]
    async fn failure_teardown_closes_a_full_event_channel_before_awaiting_supervisor() {
        let token = CancellationToken::new();
        let (events_tx, mut events_rx) = mpsc::channel(1);
        events_tx
            .try_send(SessionEvent::Diagnostic {
                source: DiagnosticSource::Cli,
                level: DiagnosticLevel::Info,
                message: "fill the channel".to_string(),
            })
            .expect("the first event fills the channel");
        let blocked_sender = events_tx.clone();
        let supervise = tokio::spawn(async move {
            // This is the exact supervisor shape: an awaited lifecycle send
            // while the bounded receiver is full.
            let _ = blocked_sender
                .send(SessionEvent::StagedStartupComplete)
                .await;
            Ok(SupervisorOutcome {
                failed_participants: Vec::new(),
            })
        });
        let board = BoardBackend::new();

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            finish_after_failure(
                &mut events_rx,
                &token,
                &board,
                supervise,
                anyhow!("terminal failed"),
            ),
        )
        .await
        .expect("closing the receiver must unblock the lifecycle sender");

        assert!(token.is_cancelled());
        assert_eq!(result.unwrap_err().to_string(), "terminal failed");
    }

    /// An orderly supervisor result still ends the session and must terminate
    /// captured setup workers registered outside the supervisor itself.
    #[cfg(unix)]
    #[tokio::test]
    async fn orderly_supervision_end_kills_registered_captured_process_groups() {
        use std::os::unix::process::CommandExt;
        use std::process::Command;
        use std::sync::{Arc, Mutex};

        let _guard = super::super::diagnostics::DIAGNOSTICS_TEST_LOCK
            .lock()
            .await;
        let mut controller =
            SessionController::new_for_test(SessionMode::Run).expect("test controller");
        controller.state = SessionState::Preparing.start().unwrap();

        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg("sleep 30").process_group(0);
        let child = Arc::new(Mutex::new(Some(
            command.spawn().expect("spawn captured child"),
        )));
        super::super::diagnostics::register_child(child.clone());

        let supervise = tokio::spawn(async {
            Ok(SupervisorOutcome {
                failed_participants: Vec::new(),
            })
        });
        controller
            .drive_supervision(
                BoardBackend::new(),
                TelemetryBackend::new(),
                phoxal_cli_core::session::stores::runtime::RuntimeStore::new(),
                Duration::from_secs(1),
                supervise,
            )
            .await
            .expect("supervision result");

        let mut child = child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let status = child
            .as_mut()
            .expect("captured child remains available for reap")
            .wait()
            .expect("reap captured child");
        child.take();
        assert!(
            !status.success(),
            "orderly end must stop the captured group"
        );
    }

    /// Finding A1: `drive_setup` and `drive_prepare_phase` share one
    /// cancelable loop; this proves the plumbing for the new `drive_setup`
    /// method (simulate's "intermediate setup" gap) actually runs an
    /// arbitrary future to completion and returns its value. The test-only
    /// controller never touches a real terminal.
    /// `SessionController::new` installs a diagnostics sender into the same
    /// process-global cell `session::diagnostics`'s own tests (and
    /// `progress`'s) touch - serialize through their shared lock.
    #[tokio::test]
    async fn drive_setup_returns_the_future_s_own_result() {
        let _guard = super::super::diagnostics::DIAGNOSTICS_TEST_LOCK
            .lock()
            .await;
        let mut controller =
            SessionController::new_for_test(SessionMode::Run).expect("test controller");
        let result = controller
            .drive_setup(async { Ok::<_, anyhow::Error>(42) })
            .await;
        assert_eq!(result.unwrap(), 42);
    }

    /// The same plumbing must also propagate the setup future's own error,
    /// not swallow or replace it.
    #[tokio::test]
    async fn drive_setup_propagates_the_future_s_error() {
        let _guard = super::super::diagnostics::DIAGNOSTICS_TEST_LOCK
            .lock()
            .await;
        let mut controller =
            SessionController::new_for_test(SessionMode::Run).expect("test controller");
        let result: Result<()> = controller
            .drive_setup(async { Err(anyhow!("setup failed")) })
            .await;
        assert_eq!(result.unwrap_err().to_string(), "setup failed");
    }

    #[test]
    fn repeated_cancel_requests_escalate_on_the_second_request() {
        let mut requested = false;
        assert!(!register_cancel_request(&mut requested));
        assert!(requested);
        assert!(register_cancel_request(&mut requested));
    }
}
