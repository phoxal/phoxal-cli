//! Compact, backend-neutral world-session monitor.
//!
//! Transport stays in the application crate. This module receives complete
//! framework-owned projections through a bounded mailbox and emits only the
//! three explicit world controls through a guaranteed, unbounded channel.

#![deny(clippy::print_stdout)]

use std::io::{self, Stderr};
use std::time::Duration;

use anyhow::{Context, Result};
use phoxal::world::api::session::control::WorldControl;
use phoxal::world::api::session::diagnostics::WorldSessionDiagnostics;
use phoxal::world::api::session::state::WorldSessionState;
use phoxal::world::api::session::{WorldLifecycle, WorldMemberPhase, WorldMotion};
use tokio::sync::mpsc;
use tuirealm::ratatui::Terminal;
use tuirealm::ratatui::backend::CrosstermBackend;
use tuirealm::ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use tuirealm::ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use tuirealm::ratatui::text::{Line, Span};
use tuirealm::ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, Wrap,
};

use crate::terminal::{TerminalGuard, install_panic_hook};
use crate::{Role, Theme};

/// One complete observation from the application-owned session client.
#[derive(Clone, Debug)]
pub enum WorldInput {
    State(WorldSessionState),
    Diagnostics(WorldSessionDiagnostics),
    DiagnosticsUnavailable {
        reason: String,
    },
    ControlFailed {
        request: WorldControl,
        reason: String,
    },
    Disconnected {
        reason: Option<String>,
    },
}

/// Why the monitor returned to the command application.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorldOutcome {
    /// The operator left the world and all of its members untouched.
    Detached,
    /// The operator's confirmed world stop reached a terminal disconnect.
    Stopped,
    /// The host or session ended independently of this monitor.
    Ended { reason: Option<String> },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorldUiOptions {
    pub title: &'static str,
    pub theme: Theme,
}

/// Run the compact world monitor on stderr.
///
/// The bounded input channel may coalesce upstream observations. Controls may
/// never be dropped, so the application supplies an unbounded sender and is
/// responsible for routing every accepted operation to the typed client.
pub async fn run(
    ingress: mpsc::Receiver<WorldInput>,
    controls: mpsc::UnboundedSender<WorldControl>,
    options: WorldUiOptions,
    initial_state: WorldSessionState,
    initial_diagnostics: Option<WorldSessionDiagnostics>,
) -> Result<WorldOutcome> {
    install_panic_hook();
    if !TerminalGuard::should_use_terminal(&io::stderr()) {
        anyhow::bail!("interactive world sessions require a terminal; run this command in a TTY");
    }
    tokio::task::spawn_blocking(move || {
        run_blocking(
            ingress,
            controls,
            options,
            initial_state,
            initial_diagnostics,
        )
    })
    .await
    .context("world UI worker panicked")?
}

#[derive(Debug)]
struct WorldModel {
    state: WorldSessionState,
    diagnostics: Option<WorldSessionDiagnostics>,
    diagnostics_problem: Option<String>,
    pending: Option<WorldControl>,
    confirming_stop: bool,
    stop_requested: bool,
    stop_acknowledged: bool,
    diagnostic: Option<String>,
    exit: Option<WorldOutcome>,
    redraw: bool,
    clear: bool,
}

impl WorldModel {
    fn new(state: WorldSessionState, diagnostics: Option<WorldSessionDiagnostics>) -> Self {
        Self {
            state,
            diagnostics,
            diagnostics_problem: None,
            pending: None,
            confirming_stop: false,
            stop_requested: false,
            stop_acknowledged: false,
            diagnostic: None,
            exit: None,
            redraw: true,
            clear: true,
        }
    }

    fn apply(&mut self, input: WorldInput) {
        match input {
            WorldInput::State(state) if state.revision > self.state.revision => {
                self.reconcile_pending(&state);
                if let WorldLifecycle::Failed { reason } = state.lifecycle {
                    self.exit = Some(WorldOutcome::Ended {
                        reason: Some(format!("{reason:?}")),
                    });
                }
                if state.lifecycle == WorldLifecycle::Stopping {
                    self.stop_acknowledged = true;
                }
                self.state = state;
                self.redraw = true;
            }
            WorldInput::State(_) => {}
            WorldInput::Diagnostics(diagnostics)
                if self
                    .diagnostics
                    .is_none_or(|current| diagnostics.revision > current.revision) =>
            {
                self.diagnostics = Some(diagnostics);
                self.diagnostics_problem = None;
                self.redraw = true;
            }
            WorldInput::Diagnostics(_) => {}
            WorldInput::DiagnosticsUnavailable { reason } => {
                self.diagnostics = None;
                self.diagnostics_problem = Some(reason);
                self.redraw = true;
            }
            WorldInput::ControlFailed { request, reason } => {
                if self.pending == Some(request) {
                    self.pending = None;
                }
                if request == WorldControl::Stop {
                    self.stop_requested = false;
                }
                self.diagnostic = Some(format!("control failed: {reason}; retry"));
                self.redraw = true;
            }
            WorldInput::Disconnected { reason } => {
                self.exit = Some(if self.stop_acknowledged {
                    WorldOutcome::Stopped
                } else {
                    WorldOutcome::Ended { reason }
                });
            }
        }
    }

    fn reconcile_pending(&mut self, state: &WorldSessionState) {
        let completed = matches!(
            (self.pending, state.lifecycle),
            (
                Some(WorldControl::Pause),
                WorldLifecycle::Ready {
                    motion: WorldMotion::Paused,
                },
            ) | (
                Some(WorldControl::Resume),
                WorldLifecycle::Ready {
                    motion: WorldMotion::Running,
                },
            ) | (Some(WorldControl::Stop), WorldLifecycle::Stopping)
        );
        if completed {
            self.pending = None;
        }
    }

    fn key(&mut self, key: KeyEvent) -> Option<WorldControl> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.exit = Some(WorldOutcome::Detached);
            return None;
        }
        if key.code == KeyCode::Char('q') {
            self.exit = Some(WorldOutcome::Detached);
            return None;
        }
        if self.confirming_stop {
            match key.code {
                KeyCode::Esc => self.confirming_stop = false,
                KeyCode::Enter if self.pending.is_none() => {
                    self.confirming_stop = false;
                    self.stop_requested = true;
                    self.pending = Some(WorldControl::Stop);
                    self.redraw = true;
                    return Some(WorldControl::Stop);
                }
                _ => {}
            }
            self.redraw = true;
            return None;
        }
        match key.code {
            KeyCode::Char('S') => {
                self.confirming_stop = true;
                self.redraw = true;
                None
            }
            KeyCode::Char('p') if self.pending.is_none() => {
                let request = match self.state.lifecycle {
                    WorldLifecycle::Ready {
                        motion: WorldMotion::Running,
                    } => WorldControl::Pause,
                    WorldLifecycle::Ready {
                        motion: WorldMotion::Paused,
                    } => WorldControl::Resume,
                    _ => {
                        self.diagnostic =
                            Some("pause is available only while the world is ready".to_owned());
                        self.redraw = true;
                        return None;
                    }
                };
                self.pending = Some(request);
                self.diagnostic = None;
                self.redraw = true;
                Some(request)
            }
            _ => None,
        }
    }
}

fn run_blocking(
    mut ingress: mpsc::Receiver<WorldInput>,
    controls: mpsc::UnboundedSender<WorldControl>,
    options: WorldUiOptions,
    initial_state: WorldSessionState,
    initial_diagnostics: Option<WorldSessionDiagnostics>,
) -> Result<WorldOutcome> {
    let title = crate::format::sanitize_terminal_text(options.title);
    let _guard = TerminalGuard::enter(&title)?;
    let backend = CrosstermBackend::new(io::stderr());
    let mut terminal: Terminal<CrosstermBackend<Stderr>> = Terminal::new(backend)?;
    force_full_repaint(&mut terminal)?;
    TerminalGuard::set_title(&title)?;

    let mut model = WorldModel::new(initial_state, initial_diagnostics);
    let mut ingress_open = true;
    loop {
        if ingress_open {
            loop {
                match ingress.try_recv() {
                    Ok(input) => model.apply(input),
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        ingress_open = false;
                        model.apply(WorldInput::Disconnected {
                            reason: Some("world observation streams closed".to_owned()),
                        });
                        break;
                    }
                }
            }
        }
        if let Some(outcome) = model.exit.clone() {
            return Ok(outcome);
        }
        if model.clear {
            force_full_repaint(&mut terminal)?;
            model.clear = false;
        }
        if model.redraw {
            terminal.draw(|frame| render(frame, &model, options.theme))?;
            model.redraw = false;
        }
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) => {
                if let Some(request) = model.key(key) {
                    controls
                        .send(request)
                        .map_err(|_| anyhow::anyhow!("world control router stopped"))?;
                }
            }
            Event::Resize(_, _) | Event::FocusGained => {
                model.clear = true;
                model.redraw = true;
            }
            _ => {}
        }
    }
}

fn force_full_repaint<B>(terminal: &mut Terminal<B>) -> Result<()>
where
    B: tuirealm::ratatui::backend::Backend,
    B::Error: Send + Sync + 'static,
{
    let area = terminal.size()?.into();
    terminal.resize(area)?;
    Ok(())
}

fn render(frame: &mut tuirealm::ratatui::Frame, model: &WorldModel, theme: Theme) {
    let [header, identity, metrics, members, footer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(4),
        Constraint::Length(7),
        Constraint::Min(4),
        Constraint::Length(2),
    ])
    .areas(frame.area());
    render_header(frame, header, model, theme);
    render_identity(frame, identity, model, theme);
    render_metrics(frame, metrics, model, theme);
    render_members(frame, members, model, theme);
    render_footer(frame, footer, model, theme);
    if model.confirming_stop {
        render_stop_confirmation(frame, model, theme);
    }
}

fn render_header(
    frame: &mut tuirealm::ratatui::Frame,
    area: Rect,
    model: &WorldModel,
    theme: Theme,
) {
    let (lifecycle, role) = lifecycle_label(model.state.lifecycle);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " PHOXAL SIMULATION ",
                crate::theme::role::selected(theme, Role::Accent),
            ),
            Span::raw(crate::format::sanitize_terminal_text(
                model.state.provenance.world.as_str(),
            )),
            Span::raw("  "),
            Span::styled(lifecycle, crate::theme::role::fg(theme, role)),
        ]))
        .block(Block::default().borders(Borders::BOTTOM)),
        area,
    );
}

fn lifecycle_label(lifecycle: WorldLifecycle) -> (&'static str, Role) {
    match lifecycle {
        WorldLifecycle::Starting => ("STARTING", Role::Steel),
        WorldLifecycle::Ready {
            motion: WorldMotion::Running,
        } => ("LIVE / RUNNING", Role::Success),
        WorldLifecycle::Ready {
            motion: WorldMotion::Paused,
        } => ("LIVE / PAUSED", Role::Warn),
        WorldLifecycle::Stopping => ("STOPPING", Role::Warn),
        WorldLifecycle::Failed { .. } => ("FAILED", Role::Error),
    }
}

fn render_identity(
    frame: &mut tuirealm::ratatui::Frame,
    area: Rect,
    model: &WorldModel,
    theme: Theme,
) {
    let provenance = &model.state.provenance;
    let text = vec![
        Line::from(model.state.instance.to_string()).alignment(Alignment::Center),
        Line::from(vec![
            Span::styled("train ", crate::theme::role::muted(theme)),
            Span::raw(provenance.framework.to_string()),
            Span::styled("  compatible ", crate::theme::role::muted(theme)),
            Span::raw(provenance.framework.compatibility_line().to_string()),
            Span::styled("  adapter ", crate::theme::role::muted(theme)),
            Span::raw(crate::format::sanitize_terminal_text(&provenance.adapter)),
            Span::raw(" "),
            Span::raw(crate::format::sanitize_terminal_text(
                &provenance.adapter_version,
            )),
        ])
        .alignment(Alignment::Center),
    ];
    frame.render_widget(Paragraph::new(text), area);
}

fn render_metrics(
    frame: &mut tuirealm::ratatui::Frame,
    area: Rect,
    model: &WorldModel,
    theme: Theme,
) {
    let progress = model.state.progress;
    let running = matches!(
        model.state.lifecycle,
        WorldLifecycle::Ready {
            motion: WorldMotion::Running
        }
    );
    let pacing = model
        .diagnostics
        .filter(|_| running)
        .and_then(|diagnostics| diagnostics.pacing)
        .filter(|pacing| pacing.is_valid())
        .map_or_else(
            || "unavailable".to_owned(),
            |pacing| observed_factor(pacing.world_elapsed_ns, pacing.host_elapsed_ns),
        );
    let mut lines = vec![
        Line::from(format!(
            "step {}  world elapsed {}  quantum {}",
            progress.completed_step(),
            format_duration(progress.elapsed_ns()),
            format_duration(model.state.provenance.time_step_ns)
        )),
        Line::from(format!("observed factor {pacing}")),
    ];
    if matches!(
        model.state.lifecycle,
        WorldLifecycle::Ready {
            motion: WorldMotion::Paused
        }
    ) {
        lines.push(Line::from(Span::styled(
            "Physics observations are paused. Robot services continue on monotonic execution time.",
            crate::theme::role::fg(theme, Role::Warn),
        )));
    }
    if let Some(diagnostic) = &model.diagnostic {
        lines.push(Line::from(Span::styled(
            format!("! {}", crate::format::sanitize_terminal_text(diagnostic)),
            crate::theme::role::fg(theme, Role::Error),
        )));
    }
    if let Some(problem) = &model.diagnostics_problem {
        lines.push(Line::from(Span::styled(
            format!(
                "diagnostics unavailable: {}",
                crate::format::sanitize_terminal_text(problem)
            ),
            crate::theme::role::muted(theme),
        )));
    }
    if let Some(pending) = model.pending {
        lines.push(Line::from(Span::styled(
            format!("{:?} requested; waiting for authoritative state", pending),
            crate::theme::role::muted(theme),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().title(" World ").borders(Borders::ALL)),
        area,
    );
}

fn render_members(
    frame: &mut tuirealm::ratatui::Frame,
    area: Rect,
    model: &WorldModel,
    theme: Theme,
) {
    let header = Row::new(["ROBOT", "STATUS", "EXECUTION"])
        .style(crate::theme::role::fg(theme, Role::Steel));
    let rows = model.state.members.iter().map(|member| {
        Row::new([
            Cell::from(crate::format::sanitize_terminal_text(member.robot.as_str())),
            Cell::from(match member.phase {
                WorldMemberPhase::Preparing => "preparing",
                WorldMemberPhase::Active => "active",
                WorldMemberPhase::Removing => "removing",
            }),
            Cell::from(member.execution.to_string()),
        ])
    });
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Percentage(25),
                Constraint::Length(12),
                Constraint::Min(32),
            ],
        )
        .header(header)
        .column_spacing(2)
        .block(
            Block::default()
                .title(format!(" Robots ({}) ", model.state.members.len()))
                .borders(Borders::ALL),
        ),
        area,
    );
}

fn render_footer(
    frame: &mut tuirealm::ratatui::Frame,
    area: Rect,
    model: &WorldModel,
    theme: Theme,
) {
    let pause = match model.state.lifecycle {
        WorldLifecycle::Ready {
            motion: WorldMotion::Paused,
        } => "resume",
        _ => "pause",
    };
    frame.render_widget(
        Paragraph::new(format!(" [p] {pause}  [S] stop world  [q] detach"))
            .style(crate::theme::role::muted(theme))
            .block(Block::default().borders(Borders::TOP)),
        area,
    );
}

fn render_stop_confirmation(
    frame: &mut tuirealm::ratatui::Frame,
    model: &WorldModel,
    theme: Theme,
) {
    let [horizontal] = Layout::horizontal([Constraint::Percentage(68)])
        .flex(Flex::Center)
        .areas(frame.area());
    let [popup] = Layout::vertical([Constraint::Length(9)])
        .flex(Flex::Center)
        .areas(horizontal);
    let body = stop_confirmation_body(model.state.members.len());
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(body).wrap(Wrap { trim: true }).block(
            Block::default()
                .title(" Stop world? ")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(crate::theme::role::fg(theme, Role::Error)),
        ),
        popup,
    );
}

fn stop_confirmation_body(affected: usize) -> String {
    format!(
        "Stop this world and all {affected} current robot execution(s)?\n\nEnter confirms. Esc cancels. q only detaches this terminal."
    )
}

fn observed_factor(world_elapsed_ns: u64, host_elapsed_ns: u64) -> String {
    if world_elapsed_ns == 0 || host_elapsed_ns == 0 {
        return "unavailable".to_owned();
    }
    let hundredths = u128::from(world_elapsed_ns)
        .saturating_mul(100)
        .checked_div(u128::from(host_elapsed_ns))
        .unwrap_or_default();
    format!("{}.{:02}x", hundredths / 100, hundredths % 100)
}

fn format_duration(nanoseconds: u64) -> String {
    if nanoseconds >= 1_000_000_000 {
        let seconds = nanoseconds / 1_000_000_000;
        let milliseconds = nanoseconds % 1_000_000_000 / 1_000_000;
        format!("{seconds}.{milliseconds:03}s")
    } else if nanoseconds >= 1_000_000 {
        format!("{}ms", nanoseconds / 1_000_000)
    } else if nanoseconds >= 1_000 {
        format!("{}us", nanoseconds / 1_000)
    } else {
        format!("{nanoseconds}ns")
    }
}

#[cfg(test)]
mod tests {
    use phoxal::model::identity::WorldId;
    use phoxal::model::world::{WorldDigest, WorldInstanceId, WorldProgress, WorldProvenance};
    use phoxal::version::FrameworkVersion;
    use phoxal::world::api::session::diagnostics::ObservedWorldPacing;
    use tuirealm::ratatui::Terminal;
    use tuirealm::ratatui::backend::TestBackend;

    use super::*;
    use crate::ColorCapability;

    fn state(motion: WorldMotion) -> WorldSessionState {
        WorldSessionState {
            revision: 1,
            instance: WorldInstanceId::parse("1234567890abcdef1234567890abcdef")
                .expect("world instance"),
            provenance: WorldProvenance {
                world: WorldId::new("warehouse").expect("world id"),
                digest: WorldDigest::parse(&"a".repeat(64)).expect("world digest"),
                random_seed: 7,
                framework: FrameworkVersion::new(0, 68, 2),
                adapter: "webots".to_owned(),
                adapter_version: "R2025a".to_owned(),
                simulator_version: "R2025a".to_owned(),
                platform: "test".to_owned(),
                time_step_ns: 10_000_000,
            },
            lifecycle: WorldLifecycle::Ready { motion },
            progress: WorldProgress::at(12, 10_000_000).expect("progress"),
            members: Vec::new(),
        }
    }

    fn diagnostics() -> WorldSessionDiagnostics {
        WorldSessionDiagnostics {
            revision: 1,
            pacing: None,
            last_transition_age_ns: None,
        }
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn p_requests_the_explicit_operation_from_authoritative_motion() {
        let mut running = WorldModel::new(state(WorldMotion::Running), Some(diagnostics()));
        assert_eq!(
            running.key(key(KeyCode::Char('p'), KeyModifiers::NONE)),
            Some(WorldControl::Pause)
        );
        assert_eq!(
            running.key(key(KeyCode::Char('p'), KeyModifiers::NONE)),
            None,
            "a repeated key cannot toggle local state while pause is pending"
        );

        let mut paused = WorldModel::new(state(WorldMotion::Paused), Some(diagnostics()));
        assert_eq!(
            paused.key(key(KeyCode::Char('p'), KeyModifiers::NONE)),
            Some(WorldControl::Resume)
        );
    }

    #[test]
    fn uppercase_s_requires_enter_and_q_always_only_detaches() {
        let mut model = WorldModel::new(state(WorldMotion::Running), Some(diagnostics()));
        assert_eq!(
            model.key(key(KeyCode::Char('S'), KeyModifiers::SHIFT)),
            None
        );
        assert!(model.confirming_stop);
        assert!(!model.stop_requested);
        assert_eq!(model.key(key(KeyCode::Char('q'), KeyModifiers::NONE)), None);
        assert_eq!(model.exit, Some(WorldOutcome::Detached));

        let mut confirmed = WorldModel::new(state(WorldMotion::Running), Some(diagnostics()));
        confirmed.key(key(KeyCode::Char('S'), KeyModifiers::SHIFT));
        assert_eq!(
            confirmed.key(key(KeyCode::Enter, KeyModifiers::NONE)),
            Some(WorldControl::Stop)
        );
        assert!(confirmed.stop_requested);
    }

    #[test]
    fn monitor_renders_only_compact_world_and_member_evidence() {
        let model = WorldModel::new(state(WorldMotion::Paused), Some(diagnostics()));
        let mut terminal = Terminal::new(TestBackend::new(110, 24)).expect("test terminal");
        terminal
            .draw(|frame| {
                render(frame, &model, Theme::new(ColorCapability::None));
            })
            .expect("render world monitor");
        let contents = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        for expected in [
            "PHOXAL SIMULATION",
            "warehouse",
            "LIVE / PAUSED",
            "1234567890abcdef1234567890abcdef",
            "compatible 0.68.x",
            "step 12",
            "Physics observations are paused",
            "ROBOT",
            "STATUS",
            "EXECUTION",
            "[p] resume",
            "[S] stop world",
            "[q] detach",
        ] {
            assert!(
                contents.contains(expected),
                "missing {expected}: {contents}"
            );
        }
        assert!(!contents.contains("logs"));
        assert!(!contents.contains("events"));
    }

    #[test]
    fn observed_factor_uses_integer_durations() {
        assert_eq!(observed_factor(5_000, 2_000), "2.50x");
        assert_eq!(observed_factor(1_000, 4_000), "0.25x");
        assert_eq!(observed_factor(0, 2_000), "unavailable");
    }

    #[test]
    fn diagnostics_loss_never_ends_the_authoritative_session() {
        let mut model = WorldModel::new(state(WorldMotion::Running), Some(diagnostics()));
        model.apply(WorldInput::DiagnosticsUnavailable {
            reason: "stream gap recovery failed".to_owned(),
        });

        assert!(model.exit.is_none());
        assert!(model.diagnostics.is_none());
        assert_eq!(
            model.diagnostics_problem.as_deref(),
            Some("stream gap recovery failed")
        );
    }

    #[test]
    fn paused_world_suppresses_stale_running_pacing() {
        let mut evidence = diagnostics();
        evidence.pacing = Some(ObservedWorldPacing {
            world_elapsed_ns: 5_000,
            host_elapsed_ns: 2_000,
            completed_transitions: 1,
        });
        let model = WorldModel::new(state(WorldMotion::Paused), Some(evidence));
        let mut terminal = Terminal::new(TestBackend::new(110, 24)).expect("test terminal");
        terminal
            .draw(|frame| render(frame, &model, Theme::new(ColorCapability::None)))
            .expect("render paused world monitor");
        let contents = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(
            contents.contains("observed factor unavailable"),
            "{contents}"
        );
        assert!(!contents.contains("2.50x"), "{contents}");
    }

    #[test]
    fn stop_confirmation_counts_every_current_member() {
        let body = stop_confirmation_body(3);
        assert!(body.contains("all 3 current robot execution(s)"), "{body}");
    }
}
