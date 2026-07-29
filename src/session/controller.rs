//! Disposable terminal projection for a project-local resident supervisor.
//!
//! Process lifecycle, preparation, restart, and shutdown authority stay in the
//! resident engine. This module owns only terminal state and forwards explicit
//! user commands to the resident protocol.

use std::io;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use crossterm::event::Event;
use phoxal_cli_core::session::event::{
    DiagnosticLevel, DiagnosticSource, PhaseId, PhaseOutcome, SessionEvent,
};
use phoxal_cli_core::session::state::{FailReason, SessionState};
use phoxal_cli_core::session::stores::telemetry::RobotScope;
use phoxal_cli_core::session::{BoardSnapshot, JoypadCommand, ProjectLifecycle, SessionMode};
use phoxal_cli_protocol::{CommandAction, CommandError};
use phoxal_cli_ui::tui::{
    DisplayAction, TerminalGuard, TuiDisplay, install_panic_hook, render::TitleInfo,
};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use crate::run::ClientProjection;
use crate::session::output::OutputContext;
use crate::telemetry::TelemetryBackend;

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
    telemetry_scope: RobotScope,
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
        let telemetry_scope = RobotScope {
            namespace: title.namespace.clone(),
            robot_id: title.robot.clone(),
        };
        let mut tui = Box::new(TuiDisplay::new(output.theme, title));
        tui.activate()?;
        let interrupts = signal(SignalKind::interrupt())?;
        let terminates = signal(SignalKind::terminate())?;
        let hangups = signal(SignalKind::hangup())?;
        let (diagnostic_tx, diagnostics) = mpsc::channel(256);
        crate::session::diagnostics::install(diagnostic_tx);
        Ok(Self {
            state: SessionState::Preparing,
            telemetry_scope,
            tui,
            diagnostics,
            interrupts,
            terminates,
            hangups,
        })
    }

    pub fn set_bus_endpoint(&mut self, endpoint: String) {
        self.tui.set_bus_endpoint(endpoint);
    }

    pub async fn drive_attachment(
        mut self,
        projection: ClientProjection,
        telemetry: TelemetryBackend,
        client: phoxal_cli_client::SupervisorClient,
        recovery_tx: tokio::sync::watch::Sender<u64>,
        shutdown_on_quit: bool,
    ) -> Result<AttachmentOutcome> {
        projection.set_log_sink(self.tui.log_sender());
        let store = client.snapshots();
        projection.replace_supervisor(&store.current());
        let mut snapshots = store.subscribe();
        let mut connection = store.connection();
        let current = store.current();
        let mut graph_generation = current.graph_generation;
        self.reflect_resident_lifecycle(current.lifecycle);
        let mut last_phase = None;
        self.reflect_resident_phase(current.startup.active_phase.as_deref(), &mut last_phase);
        let mut ticker = tokio::time::interval(Duration::from_millis(100));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        self.redraw_live(&projection, &telemetry)?;

        loop {
            tokio::select! {
                biased;
                _ = self.interrupts.recv() => {
                    if shutdown_on_quit {
                        self.request_resident_shutdown(&client).await?;
                    } else {
                        return Ok(AttachmentOutcome::Disconnected);
                    }
                }
                _ = self.terminates.recv() => {
                    if shutdown_on_quit {
                        self.request_resident_shutdown(&client).await?;
                    } else {
                        return Ok(AttachmentOutcome::Disconnected);
                    }
                }
                _ = self.hangups.recv() => return Ok(AttachmentOutcome::Disconnected),
                Some(event) = self.diagnostics.recv() => {
                    self.forward_to_renderer(&event);
                    self.redraw_live(&projection, &telemetry)?;
                }
                Ok(()) = snapshots.changed() => {
                    let snapshot = snapshots.borrow_and_update().clone();
                    if snapshot.graph_generation != graph_generation {
                        graph_generation = snapshot.graph_generation;
                        recovery_tx.send_replace(graph_generation);
                    }
                    let terminal = matches!(
                        snapshot.lifecycle,
                        ProjectLifecycle::Stopped | ProjectLifecycle::Failed
                    );
                    projection.replace_supervisor(&snapshot);
                    self.tui.set_simulation_info(snapshot.simulation.as_ref());
                    self.reflect_resident_lifecycle(snapshot.lifecycle);
                    self.reflect_resident_phase(
                        snapshot.startup.active_phase.as_deref(),
                        &mut last_phase,
                    );
                    self.redraw_live(&projection, &telemetry)?;
                    if terminal {
                        return if snapshot.lifecycle == ProjectLifecycle::Failed {
                            Err(anyhow!("resident supervisor failed"))
                        } else {
                            Ok(AttachmentOutcome::Terminal)
                        };
                    }
                }
                Ok(()) = connection.changed() => {
                    if *connection.borrow_and_update()
                        == phoxal_cli_client::ConnectionState::Crashed
                    {
                        return Err(anyhow!(
                            "resident supervisor disconnected before a terminal snapshot"
                        ));
                    }
                }
                Some(input) = poll_next_input(&mut self.tui) => {
                    let event = input.context("terminal input reader failed")?;
                    match handle_input(&mut self.tui, event, &projection.snapshot()) {
                        DisplayAction::None => {}
                        DisplayAction::Quit if !shutdown_on_quit => {
                            return Ok(AttachmentOutcome::Disconnected);
                        }
                        DisplayAction::Quit => self.request_resident_shutdown(&client).await?,
                        DisplayAction::Restart(id) => {
                            self.request_restart(&client, &id).await;
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
                    self.redraw_live(&projection, &telemetry)?;
                }
                _ = ticker.tick() => self.redraw_live(&projection, &telemetry)?,
            }
        }
    }

    async fn request_restart(&mut self, client: &phoxal_cli_client::SupervisorClient, id: &str) {
        let key = match id.parse() {
            Ok(key) => key,
            Err(error) => {
                self.report_command_warning(format!(
                    "restart request has invalid process key `{id}`: {error}"
                ));
                return;
            }
        };
        let Some(expected_producer) = client
            .snapshots()
            .current()
            .processes
            .get(&key)
            .and_then(|entry| entry.status.producer)
        else {
            self.report_command_warning(format!("process `{id}` has no restartable producer"));
            return;
        };
        match client
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
        client: &phoxal_cli_client::SupervisorClient,
    ) -> Result<()> {
        let reply = client
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
        projection: &ClientProjection,
        telemetry: &TelemetryBackend,
    ) -> Result<()> {
        self.tui
            .redraw(
                &projection.snapshot(),
                telemetry.snapshot(&self.telemetry_scope),
            )
            .context("failed to draw the interactive session frame")
    }
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
