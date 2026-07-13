//! [`SessionController`]: the one lifecycle owner for `run`/`simulation run`
//! (Target design part 1). It owns terminal acquisition/restoration, the
//! root [`CancellationToken`], the current [`SessionState`], the bounded
//! [`SessionEvent`] receiver, and picks the TUI or line renderer from an
//! [`OutputContext`] - replacing the old `Stepper` + display-activation
//! handoff + supervisor-owned `Display` design.
//!
//! # Cancellation (fixes live-acceptance #4)
//!
//! The controller starts the renderer FIRST, then the caller (`run`/
//! `simulation run`) drives preparation ([`SessionController::drive_preparation`])
//! and then supervision ([`SessionController::drive_supervision`]) through it.
//! Both methods race every wait against Ctrl-C in their own `select!`, so a
//! Ctrl-C during ANY phase - download, validate, build, a staged-startup
//! wait, the readiness barrier, or steady state - is observed immediately
//! rather than only once an outer `join!`/barrier happens to return.
//!
//! During preparation nothing is under supervision yet (no participant has
//! been spawned), so the first Ctrl-C both restores the terminal and exits
//! the process immediately - there is nothing else to tear down, and any
//! `cargo build`/download child sharing this process's foreground process
//! group already received the terminal's own SIGINT independent of this
//! handler. During supervision the first Ctrl-C instead cancels the root
//! token (letting the supervisor run its own orderly
//! `request_participant_stop`/`shutdown_all`) and waits for that to finish; a
//! second Ctrl-C forces an immediate exit if teardown is not prompt enough.
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
//!   alternate screen is the ONE surface from command start to shutdown. A
//!   dedicated ratatui startup-phase surface (the active-phase-row +
//!   diagnostics list ahead of the existing runtime navigator) is deferred to
//!   a later wave - see the crate's follow-up plan; this wave reuses the
//!   existing navigator unconditionally; a board that does not exist yet
//!   (during preparation) simply renders as an empty participant list rather
//!   than a scrollback- or corruption-causing renderer hand-off, which is the
//!   part this wave's fix targets.

use std::io;
use std::time::Duration;

use anyhow::{Result, anyhow};
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
use super::event::{DiagnosticLevel, DiagnosticSource, PhaseId, PhaseOutcome, SessionEvent};
use super::output::OutputContext;
use super::state::SessionState;

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
                let mut tui = Box::new(TuiDisplay::new(output.theme, title));
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
        let _ = self.events_tx.try_send(SessionEvent::PhaseStarted {
            id: PhaseId::new("prepare"),
            label: "Preparing".to_string(),
        });
        let result = self
            .drive_preparation(tokio::task::spawn_blocking(prepare))
            .await;
        let outcome = match &result {
            Ok(_) => PhaseOutcome::Succeeded,
            Err(error) => PhaseOutcome::Failed {
                error: format!("{error:#}"),
            },
        };
        let _ = self.events_tx.try_send(SessionEvent::PhaseFinished {
            id: PhaseId::new("prepare"),
            outcome,
            elapsed: started.elapsed(),
        });
        result
    }

    /// Drive the controller while `prepare` (a `spawn_blocking` task) runs -
    /// no board/telemetry exist yet, so redraws show an empty participant
    /// list; the renderer still shows every [`SessionEvent`] phase/diagnostic
    /// preparation emits. Returns `prepare`'s own result, or an `Err` if
    /// Ctrl-C cancelled the session first (see the module docs on why that
    /// exits the process immediately rather than waiting preparation out).
    pub async fn drive_preparation<T: Send + 'static>(
        &mut self,
        mut prepare: JoinHandle<Result<T>>,
    ) -> Result<T> {
        let empty_board = BoardSnapshot::default();
        let empty_telemetry = TelemetryBackend::default();
        loop {
            tokio::select! {
                biased;
                _ = tokio::signal::ctrl_c() => {
                    self.token.cancel();
                    self.transition_to_stopping();
                    self.teardown();
                    std::process::exit(130);
                }
                Some(event) = self.events_rx.recv() => {
                    self.apply_event(event);
                    self.redraw(&empty_board, &empty_telemetry);
                }
                result = &mut prepare => {
                    return match result {
                        Ok(inner) => inner,
                        Err(join_error) => Err(anyhow!(join_error)),
                    };
                }
            }
        }
    }

    /// Drive the controller for the rest of the session once board/telemetry
    /// exist: redraws on a ticker, forwards `SessionEvent`s, bridges TUI
    /// input (restart/joypad) back to the supervisor/telemetry, and waits for
    /// `supervise` (the spawned `supervise_until_shutdown` task) to finish or
    /// Ctrl-C to cancel it. Consumes `self` so the renderer (and, for the
    /// TUI, the terminal) is torn down exactly once, on every return path.
    pub async fn drive_supervision(
        mut self,
        board: BoardBackend,
        telemetry: TelemetryBackend,
        mut supervise: JoinHandle<Result<SupervisorOutcome>>,
    ) -> Result<SupervisorOutcome> {
        if let Renderer::Tui(tui) = &self.renderer {
            board.set_log_sink(tui.log_sender());
        }
        let mut ticker = tokio::time::interval(Duration::from_millis(500));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut cancel_requested = false;

        let outcome = loop {
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
                    self.redraw(&board.snapshot(), &telemetry);
                }
                Some(event) = self.events_rx.recv() => {
                    self.apply_event(event);
                }
                Some(input) = poll_next_input(&mut self.renderer) => {
                    let action = handle_input(&mut self.renderer, input, &board.snapshot(), &telemetry);
                    self.apply_display_action(action, &telemetry);
                }
                _ = ticker.tick() => {
                    self.redraw(&board.snapshot(), &telemetry);
                }
                result = &mut supervise => {
                    break match result {
                        Ok(inner) => inner,
                        Err(join_error) => Err(anyhow!(join_error)),
                    };
                }
            }
        };
        self.teardown();
        outcome
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
        if let Renderer::Line(line) = &mut self.renderer {
            line.handle_event(&SessionEvent::SessionChanged {
                state: self.state.clone(),
            });
        }
    }

    fn apply_event(&mut self, event: SessionEvent) {
        self.state = reduce_state(self.state.clone(), &event);
        if let Renderer::Line(line) = &mut self.renderer {
            line.handle_event(&event);
        }
        // The Tui renderer does not yet visualize phase/diagnostic events
        // directly (the dedicated startup surface is deferred - see the
        // module docs); it still benefits from this call because
        // `diagnostics::install` already keeps a `Diagnostic` event's raw
        // text from ever reaching stderr while the alternate screen owns it,
        // which is the actual corruption this fixes (#2).
    }

    fn redraw(&mut self, board: &BoardSnapshot, telemetry: &TelemetryBackend) {
        match &mut self.renderer {
            Renderer::Tui(tui) => tui.redraw(board, telemetry),
            Renderer::Line(line) => line.redraw_board(board),
            Renderer::None => {}
        }
    }

    /// Restore the terminal (if a TUI is active) and stop routing tracing
    /// through this session's event channel. Idempotent - safe to call more
    /// than once (a `std::process::exit` path calls this explicitly before
    /// exiting, since `exit` skips `Drop`; the normal return path then drops
    /// an already-`None` renderer harmlessly).
    fn teardown(&mut self) {
        diagnostics::uninstall();
        // Dropping the old `Renderer` value here (rather than waiting for
        // `Self`'s own end-of-scope drop) restores the terminal
        // SYNCHRONOUSLY, so it is safe to call `std::process::exit`
        // immediately afterward - `exit` never runs `Drop` impls.
        self.renderer = Renderer::None;
    }
}

/// Poll the active renderer for its next input event, if any. A free
/// function taking only `&mut Renderer` (rather than a `SessionController`
/// method taking `&mut self`) so its future, used directly inside
/// `drive_supervision`'s `tokio::select!`, borrows only the `renderer` field -
/// disjoint from the `events_rx` field another branch borrows in the same
/// `select!` invocation. `Line`/`None` never produce input, so this future
/// simply never resolves for them - safe to poll repeatedly every loop pass.
async fn poll_next_input(renderer: &mut Renderer) -> Option<Event> {
    match renderer {
        Renderer::Tui(tui) => tui.next_input().await,
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
            board_logger: LineLogger::new(mode_label),
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
                    format_diagnostic(self.theme, source_kind(source), *level, message)
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

/// A short, stable label for a [`DiagnosticSource`] - split out from
/// [`LineRenderer::handle_event`] so `format_diagnostic` stays unit-testable
/// without constructing a whole event.
fn source_kind(source: &DiagnosticSource) -> &str {
    match source {
        DiagnosticSource::Tracing | DiagnosticSource::Cli => "cli",
        DiagnosticSource::Dependency => "dependency",
        DiagnosticSource::Tool { name } => name.as_str(),
        DiagnosticSource::Supervisor => "supervisor",
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
    fn source_kind_labels_every_diagnostic_source() {
        assert_eq!(source_kind(&DiagnosticSource::Tracing), "cli");
        assert_eq!(source_kind(&DiagnosticSource::Dependency), "dependency");
        assert_eq!(source_kind(&DiagnosticSource::Supervisor), "supervisor");
        assert_eq!(
            source_kind(&DiagnosticSource::Tool {
                name: "router".to_string()
            }),
            "router"
        );
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
}
