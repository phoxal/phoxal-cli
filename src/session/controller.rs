//! [`SessionController`]: the one lifecycle owner for `run`/`simulation run`
//! (Target design part 1). It owns terminal acquisition/restoration, the
//! root [`CancellationToken`], the current [`SessionState`], the bounded
//! [`SessionEvent`] receiver, and picks the TUI or line renderer from an
//! [`OutputContext`] - replacing the old `Stepper` + display-activation
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
//! During preparation/setup nothing is under supervision yet (no participant
//! has been spawned), so the first Ctrl-C both restores the terminal and
//! exits the process immediately - there is nothing else to tear down, and
//! any `cargo build`/download child sharing this process's foreground
//! process group already received the terminal's own SIGINT independent of
//! this handler. During supervision the first Ctrl-C instead cancels the
//! root token (letting the supervisor run its own orderly
//! `request_participant_stop`/`shutdown_all`) and waits for that to finish; a
//! second Ctrl-C forces an immediate exit if teardown is not prompt enough.
//!
//! # Cleanup (finding B1/D3)
//!
//! [`SessionController`] restores the terminal and uninstalls diagnostics
//! routing on EVERY exit path, not just an explicit call some branches used
//! to skip: [`Drop`] uninstalls diagnostics routing unconditionally, and
//! dropping the `Renderer` field (either via `Drop` or the explicit
//! process-exit path's [`SessionController::teardown`]) restores the terminal
//! through the `Tui` renderer's own `TerminalGuard` chain. A terminal
//! draw/input failure inside [`SessionController::drive_supervision`] no
//! longer just tears down the renderer and returns: it cancels the root
//! token and AWAITS the supervisor's `JoinHandle` first (see
//! `finish_after_failure`), so a dropped `JoinHandle` never silently detaches
//! the supervisor (and every child it owns) in the background.
//!
//! # Renderer (Target design parts 4 and 6; this wave keeps part 6 minimal)
//!
//! - [`OutputMode::Json`] selects no renderer at all - stdout stays the
//!   command's JSON document, stderr stays empty.
//! - [`OutputMode::Plain`] (or `Rich` without a real TTY) selects
//!   [`LineRenderer`]: one append-only line per event, reusing
//!   [`crate::logger::LineLogger`]'s board-diff shape for participant state
//!   and adding phase/diagnostic/session lines from the event stream.
//! - [`OutputMode::Rich`] on a real TTY selects the existing
//!   [`crate::tui::TuiDisplay`], activated immediately (before preparation
//!   even starts) rather than gated behind a stepper hand-off, so the
//!   alternate screen is the ONE surface from command start to shutdown.
//!   Every [`SessionEvent`] this controller applies (phase/diagnostic/session
//!   transitions) is forwarded into the TUI via
//!   [`crate::tui::TuiDisplay::apply_session_event`], which drives a
//!   dedicated startup surface (welcome card + one active phase row + a
//!   bounded diagnostics ring) until the session first reaches
//!   `Running`/`Paused`, then collapses to the existing runtime navigator -
//!   see `crate::tui::startup`'s module docs for the pure model.

use std::future::Future;
use std::io;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use crossterm::event::Event;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use crate::display::DisplayAction;
use crate::identity::IdentitySummary;
use crate::logger::LineLogger;
use crate::output_mode::OutputMode;
use crate::supervisor::{BoardBackend, BoardSnapshot, SupervisorAction, SupervisorOutcome};
use crate::telemetry::{JoypadCommand, TelemetryBackend};
use crate::theme::Theme;
use crate::tui::{TerminalGuard, TitleInfo, TuiDisplay};

use super::diagnostics;
use super::event::{ClockPresence, DiagnosticLevel, PhaseId, PhaseOutcome, SessionEvent};
use super::output::OutputContext;
use super::state::{ClockObservation, SessionState, WaitReason};

/// Bound on the [`SessionEvent`] channel: the only source of startup and
/// runtime transitions the renderer sees (Target design part 2). Generous
/// enough that ordinary bursts (several stages finishing close together)
/// never drop an event; a slow/stuck consumer is a bug to fix, not a queue to
/// grow unboundedly for.
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Which renderer owns stderr/the terminal for this session (Target design
/// parts 4 and 6, minimal this wave - see the module docs).
enum Renderer {
    Tui(Box<TuiDisplay>),
    Line(LineRenderer),
    None,
}

/// The one lifecycle owner for a `run`/`simulation run` session. See the
/// module docs.
pub struct SessionController {
    output: OutputContext,
    token: CancellationToken,
    state: SessionState,
    events_tx: mpsc::Sender<SessionEvent>,
    events_rx: mpsc::Receiver<SessionEvent>,
    renderer: Renderer,
    /// Lets a TUI's `r restart` input reach the supervisor without the
    /// supervisor knowing about ratatui/input at all: the same
    /// `SupervisorAction` channel `run`/`simulation run` already wires up for
    /// `--watch` hot-reload, shared here rather than duplicated. `None` under
    /// the `Line`/no renderer (no interactive input exists to produce a
    /// restart in the first place).
    restart_tx: Option<mpsc::Sender<SupervisorAction>>,
}

impl SessionController {
    /// Build the controller and start its renderer immediately - for the TUI
    /// this means entering the alternate screen right now, before the
    /// caller's preparation has even started, so there is exactly one
    /// interactive surface for the whole session (Product decision 1).
    pub fn new(
        output: OutputContext,
        mode_label: &'static str,
        identity: Option<IdentitySummary>,
    ) -> io::Result<Self> {
        let (events_tx, events_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        diagnostics::install(events_tx.clone());

        let renderer = match output.mode {
            OutputMode::Json => Renderer::None,
            OutputMode::Rich if TerminalGuard::should_use_terminal(&io::stderr()) => {
                let title = TitleInfo {
                    robot: identity
                        .as_ref()
                        .map_or_else(|| "-".to_string(), |summary| summary.robot.clone()),
                    channel: identity
                        .as_ref()
                        .map_or_else(|| "-".to_string(), |summary| summary.channel.clone()),
                    mode: mode_label,
                };
                let mut tui = Box::new(TuiDisplay::new(output.theme, title, identity));
                tui.activate()?;
                Renderer::Tui(tui)
            }
            _ => Renderer::Line(LineRenderer::new(mode_label, output.theme)),
        };

        Ok(Self {
            output,
            token: CancellationToken::new(),
            state: SessionState::Preparing,
            events_tx,
            events_rx,
            renderer,
            restart_tx: None,
        })
    }

    /// The root cancellation signal: pass a clone into `SupervisorOptions`
    /// and every other cancel-safe wait (readiness barriers, stage waits) so
    /// one Ctrl-C reaches all of them.
    #[must_use]
    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    /// The event sender: clone it into preparation and the supervisor so
    /// both can emit [`SessionEvent`]s the renderer consumes.
    #[must_use]
    pub fn events(&self) -> mpsc::Sender<SessionEvent> {
        self.events_tx.clone()
    }

    /// Wire a TUI's `r restart` key to the supervisor's own action channel
    /// (the same one `--watch` hot-reload already uses). A no-op call is
    /// harmless for the `Line`/`None` renderers - they never produce a
    /// `DisplayAction::Restart` in the first place.
    pub fn set_restart_channel(&mut self, tx: mpsc::Sender<SupervisorAction>) {
        self.restart_tx = Some(tx);
    }

    /// The current output context, e.g. for a caller deciding whether an
    /// interactive wait may run unbounded (see `commands::run`/`simulate`'s
    /// `interactive_wait_budget`).
    #[must_use]
    pub const fn output(&self) -> OutputContext {
        self.output
    }

    /// `true` once the TUI (not the line renderer, not `None`) owns the
    /// terminal - the gate `run`/`simulation run` use to decide whether
    /// subscribing to the extra live-telemetry bus feeds (host/process/
    /// router/joypad) is worth the connections, since only the TUI can
    /// render them.
    #[must_use]
    pub const fn renders_tui(&self) -> bool {
        matches!(self.renderer, Renderer::Tui(_))
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
    /// result, or a terminal-input `Err`, or never returns at all if Ctrl-C
    /// fires (see the module docs on why that exits the process immediately
    /// rather than waiting `task` out - neither a `spawn_blocking` closure
    /// nor an arbitrary setup future is guaranteed to unwind promptly, and
    /// nothing is under supervision yet to tear down in the meantime).
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
        self.redraw(&empty_board, &empty_telemetry)?;
        loop {
            tokio::select! {
                biased;
                _ = tokio::signal::ctrl_c() => {
                    self.cancel_and_exit();
                }
                Some(input) = poll_next_input(&mut self.renderer) => {
                    match input {
                        Ok(event) => {
                            // The only input action that matters before
                            // supervision exists is Ctrl-C's raw-mode KEY
                            // form (`handle_input` already maps it to
                            // `DisplayAction::Quit`, same as the `q` key) -
                            // a restart/joypad key has nothing to act on yet,
                            // so it is simply ignored during this phase.
                            let action = handle_input(&mut self.renderer, event, &empty_board, &empty_telemetry);
                            if matches!(action, DisplayAction::Quit) {
                                self.cancel_and_exit();
                            }
                        }
                        Err(error) => {
                            return Err(anyhow!(error).context("terminal input reader failed"));
                        }
                    }
                }
                Some(event) = self.events_rx.recv() => {
                    self.apply_event(event);
                    self.redraw(&empty_board, &empty_telemetry)?;
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

    /// Cancel the root token, transition to `Stopping`, restore the terminal,
    /// and exit the process immediately - the shared tail for every Ctrl-C
    /// observed before supervision exists (see the module docs: nothing is
    /// under supervision yet, so there is no orderly child teardown to await
    /// first, and a `spawn_blocking`/arbitrary setup task is not guaranteed
    /// to unwind promptly on its own).
    fn cancel_and_exit(&mut self) -> ! {
        self.token.cancel();
        self.transition_to_stopping();
        self.teardown();
        std::process::exit(130);
    }

    /// Drive the controller for the rest of the session once board/telemetry
    /// exist: redraws on a ticker, forwards `SessionEvent`s, bridges TUI
    /// input (restart/joypad) back to the supervisor/telemetry, and waits for
    /// `supervise` (the spawned `supervise_until_shutdown` task) to finish or
    /// Ctrl-C to cancel it. Consumes `self` so the renderer (and, for the
    /// TUI, the terminal) is torn down exactly once, on every return path.
    ///
    /// `runtime_store` carries this session's launch-time participant
    /// metadata (finding A5 - artifact reference, declared contracts,
    /// ownership) resolved once by the caller from its `LaunchPlan` and
    /// contract-check outcome; installed into the TUI (if any) alongside the
    /// board's log sink, right before this loop starts. The `Line`/`None`
    /// renderers have no runtime-detail view to feed it to, so it is simply
    /// dropped for them.
    pub async fn drive_supervision(
        mut self,
        board: BoardBackend,
        telemetry: TelemetryBackend,
        runtime_store: crate::stores::runtime_store::RuntimeStore,
        mut supervise: JoinHandle<Result<SupervisorOutcome>>,
    ) -> Result<SupervisorOutcome> {
        if let Renderer::Tui(tui) = &mut self.renderer {
            board.set_log_sink(tui.log_sender());
            tui.set_runtime_store(runtime_store);
        }
        let mut ticker = tokio::time::interval(Duration::from_millis(500));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut cancel_requested = false;

        let end = loop {
            tokio::select! {
                biased;
                _ = tokio::signal::ctrl_c() => {
                    if cancel_requested {
                        self.teardown();
                        std::process::exit(130);
                    }
                    cancel_requested = true;
                    self.token.cancel();
                    self.transition_to_stopping();
                    if let Err(error) = self.redraw(&board.snapshot(), &telemetry) {
                        break SupervisionEnd::Failed(error);
                    }
                }
                Some(event) = self.events_rx.recv() => {
                    self.apply_event(event);
                }
                Some(input) = poll_next_input(&mut self.renderer) => {
                    match input {
                        Ok(event) => {
                            let action = handle_input(&mut self.renderer, event, &board.snapshot(), &telemetry);
                            self.apply_display_action(action, &telemetry);
                        }
                        Err(error) => {
                            break SupervisionEnd::Failed(anyhow!(error).context("terminal input reader failed"));
                        }
                    }
                }
                _ = ticker.tick() => {
                    if let Err(error) = self.redraw(&board.snapshot(), &telemetry) {
                        break SupervisionEnd::Failed(error);
                    }
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
        // the renderer and leave the supervisor (and every child it owns)
        // detached in the background. `self` (and its `Renderer`/diagnostics
        // routing) is restored via `Drop` once this function returns - see
        // the module docs - so there is no explicit `teardown()` call on this
        // path any more.
        match end {
            SupervisionEnd::Finished(result) => result,
            SupervisionEnd::Failed(error) => {
                finish_after_failure(&self.token, supervise, error).await
            }
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
                if let Some(tx) = &self.restart_tx {
                    let _ = tx.try_send(SupervisorAction::Restart { id });
                }
            }
            DisplayAction::JoypadConnect(id) => {
                telemetry.send_joypad_command(JoypadCommand::Connect(id));
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

    /// Apply one incoming event. Most variants are reduced by
    /// [`reduce_state`] (only `SessionChanged` affects `self.state`; it is
    /// PRE-validated by whoever constructed it - see that function's docs)
    /// and forwarded to the renderer as-is. `ClockObserved` and
    /// `StagedStartupComplete` are different: they carry an OBSERVATION, not
    /// a state replacement (finding B4), so the controller itself reduces
    /// them through `SessionState`'s own validated transition methods
    /// (rejecting the update entirely once the session has started
    /// `Stopping`/reached a terminal state) and, only if that produces an
    /// actual change, synthesizes a `SessionChanged` for the renderer -
    /// exactly like `transition_to_stopping`'s own internal transitions do.
    fn apply_event(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::ClockObserved(presence) => {
                self.apply_reduced_state(reduce_clock_observation(self.state.clone(), presence));
            }
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
    /// observation reduction rejected by `SessionState` (e.g. a clock sample
    /// arriving after `Stopping` has begun) returns the state unchanged, and
    /// must not spam the renderer with a no-op transition.
    fn apply_reduced_state(&mut self, next: SessionState) {
        if next != self.state {
            self.state = next.clone();
            self.forward_to_renderer(&SessionEvent::SessionChanged { state: next });
        }
    }

    /// Hand one event to whichever renderer owns the terminal - the `Line`
    /// renderer prints it as an append-only line, the TUI folds it into its
    /// startup model (`TuiDisplay::apply_session_event`) so the NEXT
    /// `redraw` shows it. `Renderer::None` (JSON mode) drops it: JSON stdout
    /// must stay byte-identical and stderr must stay empty.
    fn forward_to_renderer(&mut self, event: &SessionEvent) {
        match &mut self.renderer {
            Renderer::Line(line) => line.handle_event(event),
            Renderer::Tui(tui) => tui.apply_session_event(event),
            Renderer::None => {}
        }
    }

    /// Draw one frame. An `Err` is a genuine terminal I/O failure (the TUI's
    /// underlying tty went away) - the caller fails the session rather than
    /// silently leaving a stale screen up forever; `Line`/`None` never draw
    /// to a terminal and so never fail here.
    fn redraw(&mut self, board: &BoardSnapshot, telemetry: &TelemetryBackend) -> Result<()> {
        match &mut self.renderer {
            Renderer::Tui(tui) => tui
                .redraw(board, telemetry)
                .context("failed to draw the interactive session frame"),
            Renderer::Line(line) => {
                line.redraw_board(board);
                Ok(())
            }
            Renderer::None => Ok(()),
        }
    }

    /// Restore the terminal (if a TUI is active) and stop routing tracing
    /// through this session's event channel, SYNCHRONOUSLY. Only needed on
    /// the `std::process::exit` path (`cancel_and_exit`): `exit` skips every
    /// `Drop` impl, so it must call this explicitly first. Every OTHER exit
    /// path (normal return, `?`, an early `return`) relies on `Drop` instead
    /// (see below) - idempotent either way, since `Drop` runs on an
    /// already-`None` renderer harmlessly.
    fn teardown(&mut self) {
        diagnostics::uninstall();
        // Dropping the old `Renderer` value here (rather than waiting for
        // `Self`'s own end-of-scope drop) restores the terminal
        // SYNCHRONOUSLY, so it is safe to call `std::process::exit`
        // immediately afterward - `exit` never runs `Drop` impls.
        self.renderer = Renderer::None;
    }
}

/// Finding B1/D3: every OTHER exit path (a normal return, an early `?`, a
/// panic unwind) restores the terminal and uninstalls diagnostics routing
/// through this `Drop` impl instead of a call to `teardown()` a future code
/// path could forget. Uninstalling diagnostics here is what closes the gap:
/// dropping `self.renderer` (a struct field, dropped automatically after this
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
    token: &CancellationToken,
    supervise: JoinHandle<Result<SupervisorOutcome>>,
    error: anyhow::Error,
) -> Result<SupervisorOutcome> {
    token.cancel();
    let _ = supervise.await;
    Err(error)
}

/// Poll the active renderer for its next input event, if any. A free
/// function taking only `&mut Renderer` (rather than a `SessionController`
/// method taking `&mut self`) so its future, used directly inside
/// `drive_supervision`'s `tokio::select!`, borrows only the `renderer` field -
/// disjoint from the `events_rx` field another branch borrows in the same
/// `select!` invocation. `Line`/`None` never produce input, so this future
/// simply never resolves for them - safe to poll repeatedly every loop pass.
/// `Some(Err(_))` is a genuine terminal read failure (see
/// `TuiDisplay::next_input`'s docs) - the caller fails the session rather
/// than silently going input-deaf.
async fn poll_next_input(renderer: &mut Renderer) -> Option<io::Result<Event>> {
    match renderer {
        Renderer::Tui(tui) => match tui.next_input().await {
            Ok(Some(event)) => Some(Ok(event)),
            Ok(None) => std::future::pending().await,
            Err(error) => Some(Err(error)),
        },
        Renderer::Line(_) | Renderer::None => std::future::pending().await,
    }
}

fn handle_input(
    renderer: &mut Renderer,
    event: Event,
    board: &BoardSnapshot,
    telemetry: &TelemetryBackend,
) -> DisplayAction {
    match renderer {
        Renderer::Tui(tui) => tui.handle_input(event, board, telemetry),
        Renderer::Line(_) | Renderer::None => DisplayAction::None,
    }
}

/// Pure reduction of the controller's tracked [`SessionState`] against one
/// incoming event. Free of any I/O so a test can drive it directly over a
/// fake event stream (see this module's tests) - the actual transition
/// legality is enforced by whoever CONSTRUCTS the `SessionChanged` event
/// (preparation/the supervisor calling `SessionState`'s own `start`/
/// `to_waiting`/... methods), not re-validated here.
#[must_use]
fn reduce_state(current: SessionState, event: &SessionEvent) -> SessionState {
    match event {
        SessionEvent::SessionChanged { state } => state.clone(),
        _ => current,
    }
}

/// Reduce a raw clock OBSERVATION into the next `SessionState` (finding B4):
/// unlike `reduce_state`'s `SessionChanged` (already pre-validated by its
/// sender), this goes through `SessionState`'s own validated transition
/// methods itself, so a stale observation - notably one arriving after
/// `Stopping` has begun - is REJECTED (returns `current` unchanged) rather
/// than silently overwriting a state the clock watcher knows nothing about.
/// This is what makes the controller the sole authority reducing
/// observations into state, per the plan's target design.
#[must_use]
fn reduce_clock_observation(current: SessionState, presence: ClockPresence) -> SessionState {
    let next = match presence {
        ClockPresence::Absent => current.clone().to_waiting(WaitReason::ClockAbsent),
        ClockPresence::Paused => current
            .clone()
            .to_paused(ClockObservation { running: false }),
        ClockPresence::Running => current.clone().to_running(),
    };
    next.unwrap_or(current)
}

/// Reduce `SessionEvent::StagedStartupComplete` into the next `SessionState`
/// (finding B3): `run` has no other authority over `Running` (no simulation
/// clock), so once every staged-startup stage has finished, this is the only
/// signal that gets it there - through the same validated `to_running`
/// transition, so it is a no-op (returns `current` unchanged) for a session
/// that is not in a state `to_running` accepts (e.g. already `Stopping`).
#[must_use]
fn reduce_staged_startup_complete(current: SessionState) -> SessionState {
    current.clone().to_running().unwrap_or(current)
}

/// The append-only renderer for `OutputMode::Plain` (or `Rich` without a real
/// TTY) - Target design part 4/6. Reuses [`LineLogger`]'s board-diff shape
/// for participant lines and adds one line per phase/diagnostic/session
/// event; never redraws or emits cursor control.
struct LineRenderer {
    mode_label: &'static str,
    theme: Theme,
    board_logger: LineLogger,
    /// A `HashMap`, not a `BTreeMap`: [`PhaseId`] intentionally derives only
    /// `Eq + Hash` (see its own docs), not `Ord` - phase ids are dynamic,
    /// never a fixed sortable set.
    phase_labels: std::collections::HashMap<PhaseId, String>,
}

impl LineRenderer {
    fn new(mode_label: &'static str, theme: Theme) -> Self {
        Self {
            mode_label,
            theme,
            board_logger: LineLogger::new(mode_label, theme),
            phase_labels: std::collections::HashMap::new(),
        }
    }

    fn redraw_board(&mut self, board: &BoardSnapshot) {
        self.board_logger.redraw(board);
    }

    fn handle_event(&mut self, event: &SessionEvent) {
        match event {
            SessionEvent::PhaseStarted { id, label } => {
                self.phase_labels.insert(id.clone(), label.clone());
                eprintln!("{label}...");
            }
            SessionEvent::PhaseProgress { .. } => {
                // Append-only: no live in-place counter under Plain/non-TTY -
                // matches the design's "no cursor control" rule.
            }
            SessionEvent::PhaseFinished {
                id,
                outcome,
                elapsed,
            } => {
                let label = self
                    .phase_labels
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| id.to_string());
                self.print_phase_finished(&label, outcome, *elapsed);
            }
            SessionEvent::ParticipantChanged { .. } => {
                // `redraw_board` (board polling) is the source of truth for
                // participant lines, matching every other consumer of
                // `BoardSnapshot` - this avoids printing a line twice for the
                // same transition.
            }
            SessionEvent::Diagnostic {
                source,
                level,
                message,
            } => {
                eprintln!(
                    "{}",
                    format_diagnostic(self.theme, source.label(), *level, message)
                );
            }
            SessionEvent::Telemetry { .. } => {}
            SessionEvent::SessionChanged { state } => {
                eprintln!(
                    "{:<5} session [{}] {}",
                    "info",
                    self.mode_label,
                    state.label()
                );
            }
            // The controller's own `apply_event` intercepts both of these
            // before they ever reach a renderer: it reduces them into a
            // `SessionChanged` (via `reduce_clock_observation`/
            // `reduce_staged_startup_complete`) and forwards THAT instead -
            // see `SessionController::apply_reduced_state`. These arms exist
            // only for exhaustiveness.
            SessionEvent::ClockObserved(_) | SessionEvent::StagedStartupComplete => {}
        }
    }

    fn print_phase_finished(&self, label: &str, outcome: &PhaseOutcome, elapsed: Duration) {
        match outcome {
            PhaseOutcome::Succeeded => eprintln!(
                "{} {label} ({:.1}s)",
                self.theme.success("\u{2713}"),
                elapsed.as_secs_f32()
            ),
            PhaseOutcome::Skipped => {
                eprintln!("{} {label}", self.theme.muted("\u{b7}"));
            }
            PhaseOutcome::Failed { error } => {
                eprintln!("{} {label}", self.theme.error("\u{2717}"));
                eprintln!("  {error}");
            }
        }
    }
}

fn format_diagnostic(theme: Theme, source: &str, level: DiagnosticLevel, message: &str) -> String {
    let level_word = match level {
        DiagnosticLevel::Info => "info",
        DiagnosticLevel::Warn => "warn",
        DiagnosticLevel::Error => "error",
    };
    let role = match level {
        DiagnosticLevel::Info => crate::theme::Role::TextPrimary,
        DiagnosticLevel::Warn => crate::theme::Role::Warn,
        DiagnosticLevel::Error => crate::theme::Role::Error,
    };
    format!("{:<5} {}: {message}", level_word, theme.paint(role, source))
}

#[cfg(test)]
mod tests {
    use super::super::event::DiagnosticSource;
    use super::*;
    use crate::session::state::{ClockObservation, FailReason, WaitReason};
    use crate::theme::ColorCapability;

    fn changed(state: SessionState) -> SessionEvent {
        SessionEvent::SessionChanged { state }
    }

    /// The controller's own reducer over a fake event stream must follow the
    /// exact chain a real session drives:
    /// `Preparing -> Starting -> Waiting(ClockAbsent) -> Paused -> Running ->
    /// Stopping -> Stopped`. Each state is built through `SessionState`'s own
    /// transition methods (never constructed directly), so this doubles as
    /// proof the chain is legal per the state machine's own rules.
    #[test]
    fn reduce_state_follows_the_full_lifecycle_over_a_fake_event_stream() {
        let preparing = SessionState::Preparing;
        let starting = preparing.clone().start().expect("start");
        let waiting = starting
            .clone()
            .to_waiting(WaitReason::ClockAbsent)
            .expect("waiting");
        let paused = waiting
            .clone()
            .to_paused(ClockObservation { running: false })
            .expect("paused");
        let running = paused.clone().to_running().expect("running");
        let stopping = running.clone().to_stopping().expect("stopping");
        let stopped = stopping.clone().to_stopped().expect("stopped");

        let mut state = SessionState::Preparing;
        for next in [
            starting,
            waiting,
            paused,
            running,
            stopping,
            stopped.clone(),
        ] {
            state = reduce_state(state, &changed(next));
        }
        assert_eq!(state, stopped);
        assert!(state.is_terminal());
    }

    /// A paused clock is healthy (Product decision 5): the reducer must
    /// adopt `Paused`, never silently reinterpret it as `Failed`.
    #[test]
    fn reduce_state_adopts_paused_not_failed() {
        let state = SessionState::Preparing
            .start()
            .unwrap()
            .to_waiting(WaitReason::ClockAbsent)
            .unwrap();
        let paused = state
            .clone()
            .to_paused(ClockObservation { running: false })
            .expect("an observed paused clock sample must be accepted");
        let reduced = reduce_state(state, &changed(paused.clone()));
        assert_eq!(reduced, paused);
        assert_ne!(reduced, SessionState::Failed(FailReason::Timeout));
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

    #[test]
    fn format_diagnostic_includes_the_level_word_and_source() {
        let theme = Theme::new(ColorCapability::None);
        let text = format_diagnostic(theme, "cli", DiagnosticLevel::Warn, "connection retrying");
        assert!(text.contains("warn"), "{text}");
        assert!(text.contains("cli"), "{text}");
        assert!(text.contains("connection retrying"), "{text}");
    }

    #[test]
    fn line_renderer_finishes_a_phase_using_its_started_label() {
        let mut renderer = LineRenderer::new("run", Theme::new(ColorCapability::None));
        renderer.handle_event(&SessionEvent::PhaseStarted {
            id: PhaseId::new("download"),
            label: "Downloading artifacts".to_string(),
        });
        assert_eq!(
            renderer
                .phase_labels
                .get(&PhaseId::new("download"))
                .map(String::as_str),
            Some("Downloading artifacts")
        );
        // Finishing must not panic even for an id whose Started line was
        // never recorded (defensive - falls back to the bare id).
        renderer.handle_event(&SessionEvent::PhaseFinished {
            id: PhaseId::new("unseen"),
            outcome: PhaseOutcome::Succeeded,
            elapsed: Duration::from_millis(5),
        });
    }

    /// Finding B4: an OBSERVED clock presence must follow the SAME validated
    /// chain `reduce_state`'s tests already exercise for `SessionChanged` -
    /// `Starting -> Waiting(ClockAbsent) -> Paused -> Running` - proving the
    /// controller reduces observations itself rather than trusting them
    /// blindly.
    #[test]
    fn reduce_clock_observation_follows_the_validated_chain() {
        let starting = SessionState::Preparing.start().unwrap();
        let waiting = reduce_clock_observation(starting, ClockPresence::Absent);
        assert_eq!(waiting, SessionState::Waiting(WaitReason::ClockAbsent));

        let paused = reduce_clock_observation(waiting, ClockPresence::Paused);
        assert_eq!(paused, SessionState::Paused);

        let running = reduce_clock_observation(paused, ClockPresence::Running);
        assert_eq!(running, SessionState::Running);
    }

    /// The exact regression finding B4 flags: a clock observation arriving
    /// after the session has started `Stopping` must be REJECTED, not
    /// silently overwrite it with `Running`/`Paused`. This is what makes the
    /// controller (not the clock watcher) the sole state authority.
    #[test]
    fn reduce_clock_observation_rejects_updates_once_stopping_has_begun() {
        let stopping = SessionState::Preparing
            .start()
            .unwrap()
            .to_running()
            .unwrap()
            .to_stopping()
            .unwrap();

        let after_running = reduce_clock_observation(stopping.clone(), ClockPresence::Running);
        assert_eq!(
            after_running, stopping,
            "a queued Running observation must not overwrite Stopping"
        );

        let after_paused = reduce_clock_observation(stopping.clone(), ClockPresence::Paused);
        assert_eq!(
            after_paused, stopping,
            "a queued Paused observation must not overwrite Stopping"
        );

        let after_absent = reduce_clock_observation(stopping.clone(), ClockPresence::Absent);
        assert_eq!(
            after_absent, stopping,
            "a queued Absent observation must not overwrite Stopping"
        );
    }

    /// Finding B3: `StagedStartupComplete` is `run`'s only path to `Running`
    /// (no simulation clock ever tells it) - it must reach `Running` from
    /// `Starting`, and it must NOT resurrect a session that has already moved
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
                clean_shutdown: true,
                failed_participants: Vec::new(),
            })
        });

        let result = finish_after_failure(&token, supervise, anyhow!("terminal draw failed")).await;

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

    /// Finding A1: `drive_setup` and `drive_prepare_phase` share one
    /// cancelable loop; this proves the plumbing for the new `drive_setup`
    /// method (simulate's "intermediate setup" gap) actually runs an
    /// arbitrary future to completion and returns its value. `OutputMode::Plain`
    /// never touches a real terminal, so this needs no TUI/terminal fixture.
    /// `SessionController::new` installs a diagnostics sender into the same
    /// process-global cell `session::diagnostics`'s own tests (and
    /// `progress`'s) touch - serialize through their shared lock.
    #[tokio::test]
    async fn drive_setup_returns_the_future_s_own_result() {
        let _guard = super::super::diagnostics::DIAGNOSTICS_TEST_LOCK
            .lock()
            .await;
        let mut controller = SessionController::new(
            OutputContext::new(OutputMode::Plain, Theme::new(ColorCapability::None), false),
            "test",
            None,
        )
        .expect("Plain mode never touches a real terminal");
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
        let mut controller = SessionController::new(
            OutputContext::new(OutputMode::Plain, Theme::new(ColorCapability::None), false),
            "test",
            None,
        )
        .expect("Plain mode never touches a real terminal");
        let result: Result<()> = controller
            .drive_setup(async { Err(anyhow!("setup failed")) })
            .await;
        assert_eq!(result.unwrap_err().to_string(), "setup failed");
    }
}
