//! Disposable terminal projection for a project-local resident supervisor.
//!
//! Process lifecycle, preparation, restart, and shutdown authority stay in the
//! resident engine. This module owns only terminal state and forwards explicit
//! user commands to the resident protocol.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use crate::session::output::OutputContext;
use anyhow::{Context, Result, anyhow};
use crossterm::event::Event;
use phoxal_cli_core::session::RobotScope;
use phoxal_cli_core::session::event::{
    DiagnosticLevel, DiagnosticSource, PhaseId, PhaseOutcome, SessionEvent,
};
use phoxal_cli_core::session::state::{FailReason, SessionState};
use phoxal_cli_core::session::{
    BoardSnapshot, ParticipantStatus, ProcessScope, ProjectLifecycle, RoutedLogLine,
    RoutedLogUpdate, RouterMetricsSample, SessionMode, Timestamped, TopicMetric,
};
use phoxal_cli_observation::{
    AttachmentEvent, BusQuery, LogAnchor, LogFilters, LogQuery, ObservationQuery, ProcessTable,
    QueryToken, RuntimeQuery, WindowDirection,
};
use phoxal_cli_protocol::{CommandAction, CommandError};
use phoxal_cli_ui::tui::{
    DisplayAction, TerminalGuard, TuiDisplay, install_panic_hook, render::TitleInfo,
};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

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
    title.train = "Cargo.lock".to_string();
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
        train: "unknown".to_string(),
        manifest: "n/a".to_string(),
        mode,
        bus_endpoint: phoxal_cli_supervisor::default_connect_endpoint(),
        simulation_profile: None,
        simulation_world: None,
        started_at: std::time::SystemTime::now(),
        started_instant: std::time::Instant::now(),
    }
}

pub struct SessionController {
    state: SessionState,
    tui: Box<TuiDisplay>,
    diagnostics: mpsc::Receiver<SessionEvent>,
    interrupts: tokio::signal::unix::Signal,
    terminates: tokio::signal::unix::Signal,
    hangups: tokio::signal::unix::Signal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttachmentOutcome {
    Terminal,
    Disconnected,
}

impl SessionController {
    pub fn new_attachment(
        output: OutputContext,
        mode: SessionMode,
        project_root: &Path,
        snapshot: &phoxal_cli_protocol::SupervisorSnapshotV0,
    ) -> io::Result<Self> {
        install_panic_hook();
        let mut title = session_title(project_root, mode);
        title.train = snapshot.framework_train.clone();
        if let Some(simulation) = &snapshot.simulation {
            title.simulation_profile = Some(simulation.profile.clone());
            title.simulation_world = Some(simulation.world.clone());
        }
        if !output.interactive || !TerminalGuard::should_use_terminal(&io::stderr()) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "interactive resident sessions require a terminal; run this command in a TTY",
            ));
        }
        let mut tui = Box::new(TuiDisplay::new(output.theme, title));
        tui.activate()?;
        let interrupts = signal(SignalKind::interrupt())?;
        let terminates = signal(SignalKind::terminate())?;
        let hangups = signal(SignalKind::hangup())?;
        let (diagnostic_tx, diagnostics) = mpsc::channel(256);
        crate::session::diagnostics::install(diagnostic_tx);
        Ok(Self {
            state: SessionState::Preparing,
            tui,
            diagnostics,
            interrupts,
            terminates,
            hangups,
        })
    }

    pub async fn drive_attachment(
        mut self,
        mut ports: phoxal_cli_client::AttachmentPorts,
        shutdown_on_quit: bool,
    ) -> Result<AttachmentOutcome> {
        let mut board = BoardSnapshot::default();
        let mut telemetry = phoxal_cli_core::session::TelemetrySnapshot::default();
        let mut processes = ProcessTable::default();
        let mut current_epoch = None;
        let mut query_token = 0_u64;
        let mut last_phase = None;
        self.redraw_live(&board, &telemetry)?;

        loop {
            tokio::select! {
                biased;
                _ = self.interrupts.recv() => {
                    if shutdown_on_quit {
                        self.request_resident_shutdown(&ports.supervisor_commands).await?;
                    } else {
                        return Ok(AttachmentOutcome::Disconnected);
                    }
                }
                _ = self.terminates.recv() => {
                    if shutdown_on_quit {
                        self.request_resident_shutdown(&ports.supervisor_commands).await?;
                    } else {
                        return Ok(AttachmentOutcome::Disconnected);
                    }
                }
                _ = self.hangups.recv() => return Ok(AttachmentOutcome::Disconnected),
                Some(event) = self.diagnostics.recv() => {
                    self.forward_to_renderer(&event);
                    self.redraw_live(&board, &telemetry)?;
                }
                event = ports.events.recv() => {
                    let Some(event) = event else {
                        return Err(anyhow!(
                            "resident supervisor disconnected before a terminal observation"
                        ));
                    };
                    match event {
                        AttachmentEvent::EpochChanged(epoch) => {
                            current_epoch = Some(epoch);
                            telemetry = phoxal_cli_core::session::TelemetrySnapshot::default();
                        }
                        AttachmentEvent::SupervisorChanged(supervisor) => {
                            self.tui.set_bus_endpoint(supervisor.router.clone());
                            self.tui.set_simulation_info(supervisor.simulation.as_ref());
                            self.reflect_resident_lifecycle(supervisor.lifecycle);
                            self.reflect_resident_phase(
                                supervisor.startup.active_phase.as_deref(),
                                &mut last_phase,
                            );
                            if matches!(
                                supervisor.lifecycle,
                                ProjectLifecycle::Stopped | ProjectLifecycle::Failed
                            ) {
                                return if supervisor.lifecycle == ProjectLifecycle::Failed {
                                    Err(anyhow!("resident supervisor failed"))
                                } else {
                                    Ok(AttachmentOutcome::Terminal)
                                };
                            }
                        }
                        AttachmentEvent::ProcessesChanged(next) => {
                            processes = (*next).clone();
                            board = board_from_processes(&processes);
                        }
                        AttachmentEvent::LogsChanged(changed) => {
                            let Some(epoch) = current_epoch else {
                                continue;
                            };
                            query_token = query_token.wrapping_add(1);
                            let window = ports.logs.read(ObservationQuery {
                                epoch,
                                observed_revision: changed.revision,
                                token: QueryToken(query_token),
                                body: LogQuery {
                                    filters: LogFilters::default(),
                                    anchor: LogAnchor::Latest,
                                    direction: WindowDirection::Forward,
                                    limit: 2_000,
                                },
                            }).await;
                            if window.epoch == epoch
                                && window.revision >= changed.revision
                                && window.token == QueryToken(query_token)
                            {
                                install_log_window(&self.tui, &window.rows);
                            }
                        }
                        AttachmentEvent::DeviceChanged(observation) => {
                            let now = Instant::now();
                            telemetry.clock = observation
                                .clock
                                .map(|clock| Timestamped::new(clock, now));
                            let scope = telemetry.scope.clone().or_else(|| {
                                observation.robots.keys().next().map(|robot| RobotScope {
                                    namespace: robot.namespace.clone(),
                                    robot_id: robot.robot_id.clone(),
                                })
                            });
                            telemetry.device = scope.as_ref().and_then(|scope| {
                                observation
                                    .robots
                                    .iter()
                                    .find(|(robot, _)| {
                                        robot.namespace == scope.namespace
                                            && robot.robot_id == scope.robot_id
                                    })
                                    .map(|(_, sample)| Timestamped::new(sample.clone(), now))
                            });
                            telemetry.scope = scope;
                        }
                        AttachmentEvent::InputChanged(observation) => {
                            let now = Instant::now();
                            telemetry.joypad =
                                Some(Timestamped::new(observation.joypads.clone(), now));
                            telemetry.motion =
                                observation.motion.map(|motion| Timestamped::new(motion, now));
                        }
                        AttachmentEvent::BusChanged(changed) => {
                            let Some(epoch) = current_epoch else {
                                continue;
                            };
                            query_token = query_token.wrapping_add(1);
                            let token = QueryToken(query_token);
                            let window = ports.bus.read(ObservationQuery {
                                epoch,
                                observed_revision: changed.revision,
                                token,
                                body: BusQuery {
                                    topic: None,
                                    direction: WindowDirection::Backward,
                                    limit: 16_384,
                                },
                            }).await;
                            if window.epoch == epoch
                                && window.revision >= changed.revision
                                && window.token == token
                            {
                                install_bus_window(&mut telemetry, &window.rows);
                            }
                        }
                        AttachmentEvent::RuntimesChanged(changed) => {
                            let Some(epoch) = current_epoch else {
                                continue;
                            };
                            query_token = query_token.wrapping_add(1);
                            let token = QueryToken(query_token);
                            let window = ports.runtimes.read(ObservationQuery {
                                epoch,
                                observed_revision: changed.revision,
                                token,
                                body: RuntimeQuery {
                                    participant: None,
                                    direction: WindowDirection::Backward,
                                    limit: 4_096,
                                },
                            }).await;
                            if window.epoch == epoch
                                && window.revision >= changed.revision
                                && window.token == token
                            {
                                install_runtime_window(&mut telemetry, &window.rows);
                            }
                        }
                        AttachmentEvent::SourceHealthChanged(_)
                        | AttachmentEvent::FreshnessChanged(_) => {}
                    }
                    self.redraw_live(&board, &telemetry)?;
                }
                Some(input) = poll_next_input(&mut self.tui) => {
                    let event = input.context("terminal input reader failed")?;
                    match handle_input(&mut self.tui, event, &board) {
                        DisplayAction::None => {}
                        DisplayAction::Quit if !shutdown_on_quit => {
                            return Ok(AttachmentOutcome::Disconnected);
                        }
                        DisplayAction::Quit => self.request_resident_shutdown(&ports.supervisor_commands).await?,
                        DisplayAction::Restart(id) => {
                            self.request_restart(&processes, &ports.supervisor_commands, &id).await;
                        }
                        DisplayAction::JoypadSelect(id) => {
                            if let Err(error) = ports.input_commands.select(id).await {
                                self.report_command_warning(format!("input select failed: {error:#}"));
                            }
                        }
                        DisplayAction::JoypadSetEnabled(enabled) => {
                            if let Err(error) = ports.input_commands.set_enabled(enabled).await {
                                self.report_command_warning(format!("input enable failed: {error:#}"));
                            }
                        }
                        DisplayAction::JoypadRescan => {
                            if let Err(error) = ports.input_commands.rescan().await {
                                self.report_command_warning(format!("input rescan failed: {error:#}"));
                            }
                        }
                    }
                    self.redraw_live(&board, &telemetry)?;
                }
            }
        }
    }

    async fn request_restart(
        &mut self,
        processes: &ProcessTable,
        commands: &phoxal_cli_client::SupervisorCommands,
        id: &str,
    ) {
        let key = match id.parse() {
            Ok(key) => key,
            Err(error) => {
                self.report_command_warning(format!(
                    "restart request has invalid process key `{id}`: {error}"
                ));
                return;
            }
        };
        let Some(expected_producer) = processes
            .get(&key)
            .and_then(|entry| entry.entry.status.producer)
        else {
            self.report_command_warning(format!("process `{id}` has no restartable producer"));
            return;
        };
        match commands
            .command_with_reconnect(CommandAction::Restart {
                process: key,
                expected_producer,
            })
            .await
        {
            Ok(reply) if !reply.accepted => {
                self.report_command_rejection("restart", reply.error);
            }
            Err(error) => self.report_command_warning(format!(
                "supervisor restart command failed after reconnect: {error:#}"
            )),
            Ok(_) => {}
        }
    }

    async fn request_resident_shutdown(
        &mut self,
        commands: &phoxal_cli_client::SupervisorCommands,
    ) -> Result<()> {
        let reply = commands
            .command_with_reconnect(CommandAction::Shutdown)
            .await
            .context("resident shutdown command failed after reconnect")?;
        if reply.accepted {
            self.transition_to_stopping();
        } else {
            self.report_command_rejection("shutdown", reply.error);
        }
        Ok(())
    }

    fn reflect_resident_lifecycle(&mut self, lifecycle: ProjectLifecycle) {
        self.state = match lifecycle {
            ProjectLifecycle::Starting => SessionState::Starting,
            ProjectLifecycle::Ready | ProjectLifecycle::Degraded => SessionState::Running,
            ProjectLifecycle::Stopping => SessionState::Stopping,
            ProjectLifecycle::Stopped => SessionState::Stopped,
            ProjectLifecycle::Failed => SessionState::Failed(FailReason::Terminal(
                "resident supervisor failed".to_string(),
            )),
        };
        self.forward_to_renderer(&SessionEvent::SessionChanged {
            state: self.state.clone(),
        });
    }

    fn reflect_resident_phase(&mut self, active: Option<&str>, previous: &mut Option<String>) {
        if previous.as_deref() == active {
            return;
        }
        if let Some(finished) = previous.take() {
            self.forward_to_renderer(&SessionEvent::PhaseFinished {
                id: PhaseId::new(finished),
                outcome: PhaseOutcome::Succeeded,
                elapsed: Duration::ZERO,
            });
        }
        if let Some(active) = active {
            self.forward_to_renderer(&SessionEvent::PhaseStarted {
                id: PhaseId::new(active),
                label: active.to_string(),
            });
            *previous = Some(active.to_string());
        }
    }

    fn transition_to_stopping(&mut self) {
        if let Ok(next) = self.state.clone().to_stopping() {
            self.state = next;
            self.forward_to_renderer(&SessionEvent::SessionChanged {
                state: self.state.clone(),
            });
        }
    }

    fn report_command_rejection(&mut self, operation: &str, error: Option<CommandError>) {
        self.report_command_warning(format!(
            "supervisor rejected {operation}: {}",
            error.map_or_else(
                || "unspecified rejection".to_string(),
                |error| format!("{error:?}")
            )
        ));
    }

    fn report_command_warning(&mut self, message: String) {
        self.forward_to_renderer(&SessionEvent::Diagnostic {
            source: DiagnosticSource::Cli,
            level: DiagnosticLevel::Warn,
            message,
        });
    }

    fn forward_to_renderer(&mut self, event: &SessionEvent) {
        self.tui.apply_session_event(event);
    }

    fn redraw_live(
        &mut self,
        board: &BoardSnapshot,
        telemetry: &phoxal_cli_core::session::TelemetrySnapshot,
    ) -> Result<()> {
        self.tui
            .redraw(board, telemetry.clone())
            .context("failed to draw the interactive session frame")
    }
}

fn board_from_processes(processes: &ProcessTable) -> BoardSnapshot {
    let participants = processes
        .values()
        .map(|process| {
            let id = process.key.to_string();
            let mut status = ParticipantStatus::new(&id, process.kind, process.state);
            status.pid = process.entry.status.pid;
            status.restart_count = process.entry.status.restart_count_in_generation;
            status.note = process
                .entry
                .status
                .last_failure
                .as_ref()
                .map(|failure| failure.detail.as_str().to_string());
            status.present = process.present;
            status.runtime_started_at = Some(process.started_at);
            status.runtime_ended_at = process.ended_at;
            status.runtime_first_ready_at = process.first_ready_at;
            status.runtime_artifact_ref = Some(process.entry.descriptor.artifact.clone());
            status.runtime_user_service = process.user_service;
            if let ProcessScope::Robot(robot) = &process.key.scope {
                status.scope = Some(RobotScope {
                    namespace: robot.namespace.clone(),
                    robot_id: robot.robot_id.clone(),
                });
            }
            (id, status)
        })
        .collect();
    BoardSnapshot { participants }
}

fn install_log_window(tui: &TuiDisplay, rows: &[phoxal_cli_observation::LogRow]) {
    let lines = rows
        .iter()
        .map(|row| RoutedLogLine {
            participant: row.participant.clone(),
            source: row.source,
            severity: row.severity,
            text: row.text.clone(),
            event_time: row.event_time,
            scope: row.scope.clone(),
        })
        .collect();
    let sender = tui.log_sender();
    let _ = sender.try_send(RoutedLogUpdate::ReplaceAll(lines));
}

fn install_bus_window(
    telemetry: &mut phoxal_cli_core::session::TelemetrySnapshot,
    rows: &[phoxal_cli_observation::BusRow],
) {
    let scope = telemetry
        .scope
        .clone()
        .or_else(|| rows.first().map(|row| row.scope.clone()));
    let Some(scope) = scope else {
        telemetry.router = None;
        telemetry.router_throughput_history.clear();
        return;
    };
    let latest_at = rows
        .iter()
        .filter(|row| row.scope == scope)
        .map(|row| row.observed_at)
        .max();
    let Some(latest_at) = latest_at else {
        telemetry.router = None;
        telemetry.router_throughput_history.clear();
        return;
    };
    let latest = rows
        .iter()
        .filter(|row| row.scope == scope && row.observed_at == latest_at)
        .collect::<Vec<_>>();
    let topics = latest
        .iter()
        .copied()
        .filter(|row| !row.topic.is_empty())
        .map(|row| TopicMetric {
            topic: row.topic.clone(),
            from_participant: row.participant.clone(),
            ingress_rate_hz: row.rate_hz,
            count: row.count,
            aggregate_overflow: row.aggregate_overflow,
        })
        .collect::<Vec<_>>();
    let current = latest[0];
    let mut throughput = std::collections::BTreeMap::new();
    for row in rows.iter().filter(|row| row.scope == scope) {
        throughput.insert(row.observed_at, row.throughput_msg_s);
    }
    let mut history = throughput
        .into_iter()
        .map(|(received_at, value)| Timestamped::new(value, received_at))
        .collect::<Vec<_>>();
    if history.len() > 60 {
        history.drain(..history.len() - 60);
    }
    telemetry.scope = Some(scope.clone());
    telemetry.router = Some(Timestamped::new(
        RouterMetricsSample {
            topics: Arc::new(topics),
            topics_truncated: current.topics_truncated,
            throughput_msg_s: current.throughput_msg_s,
            window_ns: current.window_ns,
        },
        latest_at,
    ));
    telemetry.router_throughput_history = history;
}

fn install_runtime_window(
    telemetry: &mut phoxal_cli_core::session::TelemetrySnapshot,
    rows: &[phoxal_cli_observation::RuntimeRow],
) {
    let scope = telemetry
        .scope
        .clone()
        .or_else(|| rows.first().map(|row| row.scope.clone()));
    let Some(scope) = scope else {
        telemetry.runtimes.clear();
        return;
    };
    let now = Instant::now();
    let mut latest = std::collections::BTreeMap::new();
    for row in rows.iter().filter(|row| row.scope == scope) {
        latest
            .entry(row.sample.participant_id.clone())
            .or_insert_with(|| Timestamped::new(row.sample.clone(), now));
    }
    telemetry.scope = Some(scope.clone());
    telemetry.runtime_status.capacity_evictions = rows
        .iter()
        .filter(|row| row.scope == scope)
        .map(|row| row.capacity_evictions)
        .max()
        .unwrap_or_default();
    telemetry.runtimes = latest;
}

impl Drop for SessionController {
    fn drop(&mut self) {
        crate::session::diagnostics::uninstall();
    }
}

async fn poll_next_input(tui: &mut TuiDisplay) -> Option<io::Result<Event>> {
    match tui.next_input().await {
        Ok(Some(event)) => Some(Ok(event)),
        Ok(None) => std::future::pending().await,
        Err(error) => Some(Err(error)),
    }
}

fn handle_input(tui: &mut TuiDisplay, event: Event, board: &BoardSnapshot) -> DisplayAction {
    tui.handle_input(event, board)
}
