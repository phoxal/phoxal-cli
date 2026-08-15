//! The shared attach-and-drive-the-TUI path.
//!
//! `run`, `attach`, and `simulation webots run` all end here: one attachment,
//! one terminal application, one set of effects routed back to the supervisor
//! API. The commands differ in how they *get* an execution to attach to, never
//! in how they attach.

use std::path::Path;
use std::time::Duration;

use crate::cli::output::diagnostics::RuntimeEvent;
use anyhow::{Context, Result};
use phoxal_cli_ui::{AttachmentOutcome, Effect, EffectSenders, SessionInput, UiOptions};
use phoxal_client::supervisor::execution::{CommandOutcome, CommandRejection};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::JoinSet;

use crate::attach::{Session, SessionPorts};
use crate::cli::context::AppContext;

const UI_INGRESS_CAPACITY: usize = 256;
const EFFECT_CAPACITY: usize = 64;
const DIAGNOSTIC_CAPACITY: usize = 256;
const SESSION_SHUTDOWN_BUDGET: Duration = Duration::from_secs(5);

/// Whether leaving this session may leave the execution running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Detachable {
    /// A real execution: `q` detaches and the supervisor keeps running.
    Yes,
    /// A simulation session: this client owns Webots, so leaving would strand
    /// a simulator with no operator. `q` ends the whole session.
    No,
}

impl Detachable {
    const fn allows_detach(self) -> bool {
        matches!(self, Self::Yes)
    }
}

/// Terminal evidence from a supervisor process owned by the calling command.
///
/// Simulation owns the supervisor child it launched. This side channel lets
/// that owner end the attachment even if the final bus observation is lost;
/// ordinary attach and run sessions still rely solely on supervisor evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OwnedSupervisorExit {
    Stopped,
    Failed { reason: String },
}

/// Drive one attachment through the terminal application until it ends.
pub(crate) async fn drive(
    app: &AppContext,
    project: &Path,
    session: Session,
    detachable: Detachable,
) -> Result<AttachmentOutcome> {
    drive_inner(app, project, session, detachable, None).await
}

/// Drive an attachment whose caller also owns the launched supervisor child.
pub(crate) async fn drive_with_owned_supervisor_exit(
    app: &AppContext,
    project: &Path,
    session: Session,
    detachable: Detachable,
    owned_supervisor_exit: oneshot::Receiver<OwnedSupervisorExit>,
) -> Result<AttachmentOutcome> {
    drive_inner(
        app,
        project,
        session,
        detachable,
        Some(owned_supervisor_exit),
    )
    .await
}

async fn drive_inner(
    app: &AppContext,
    project: &Path,
    session: Session,
    detachable: Detachable,
    mut owned_supervisor_exit: Option<oneshot::Receiver<OwnedSupervisorExit>>,
) -> Result<AttachmentOutcome> {
    let Session { ports, .. } = &session;
    let SessionPorts {
        events: _,
        supervisor,
        input_commands,
        logs,
        runtimes,
    } = ports;
    let effect_ports = EffectPorts {
        supervisor: supervisor.clone(),
        input: input_commands.clone(),
        logs: logs.clone(),
        runtimes: runtimes.clone(),
    };
    let mut session = session;

    let (ingress_tx, ingress_rx) = mpsc::channel(UI_INGRESS_CAPACITY);
    let (command_tx, mut command_rx) = mpsc::channel(EFFECT_CAPACITY);
    let (guaranteed_tx, mut guaranteed_rx) = mpsc::unbounded_channel();
    let (diagnostic_tx, mut diagnostic_rx) = mpsc::channel(DIAGNOSTIC_CAPACITY);
    crate::cli::output::diagnostics::install(diagnostic_tx);
    let _diagnostics = DiagnosticGuard;

    let mut initial_inputs = Vec::new();
    while let Ok(event) = session.ports.events.try_recv() {
        initial_inputs.push(SessionInput::Client(event));
    }
    let options = UiOptions {
        title: terminal_title(project),
        theme: app.output.theme,
        detachable: detachable.allows_detach(),
    };
    let ui = phoxal_cli_ui::run(
        ingress_rx,
        EffectSenders {
            guaranteed: guaranteed_tx,
            commands: command_tx,
        },
        options,
        initial_inputs,
    );
    tokio::pin!(ui);
    let mut interrupts = signal(SignalKind::interrupt())?;
    let mut terminates = signal(SignalKind::terminate())?;
    let mut hangups = signal(SignalKind::hangup())?;
    let effect_slots = std::sync::Arc::new(Semaphore::new(EFFECT_CAPACITY));
    let mut effect_tasks = JoinSet::new();
    let mut confirmed_stop_tasks = JoinSet::new();
    let mut events_open = true;
    let mut guaranteed_open = true;
    let mut commands_open = true;
    let mut diagnostics_open = true;

    let outcome: Result<AttachmentOutcome> = loop {
        tokio::select! {
            result = &mut ui => break result,
            // The terminal is in raw mode, so an interactive Ctrl+C arrives as
            // a key event. This arm catches a SIGINT delivered some other way
            // and gives it the same meaning: the UI decides, and the first one
            // never stops anything.
            _ = interrupts.recv() => {
                if send_input(&ingress_tx, SessionInput::Interrupt).await.is_err() {
                    break (&mut ui).await;
                }
            }
            // External termination is not a stop request. The supervisor is
            // durable and outside this process group, so the client leaves and
            // the execution continues - except in a simulation session, where
            // the UI turns leaving into a stop because this client owns Webots.
            _ = terminates.recv() => {
                if send_input(&ingress_tx, SessionInput::Terminate).await.is_err() {
                    break (&mut ui).await;
                }
            }
            _ = hangups.recv() => {
                if send_input(&ingress_tx, SessionInput::Terminate).await.is_err() {
                    break (&mut ui).await;
                }
            }
            diagnostic = diagnostic_rx.recv(), if diagnostics_open => {
                if let Some(diagnostic) = diagnostic {
                    if send_input(
                        &ingress_tx,
                        SessionInput::Diagnostic(diagnostic_message(diagnostic)),
                    )
                    .await
                    .is_err()
                    {
                        break (&mut ui).await;
                    }
                } else {
                    diagnostics_open = false;
                }
            }
            event = session.ports.events.recv(), if events_open => {
                if let Some(event) = event {
                    if send_input(&ingress_tx, SessionInput::Client(event)).await.is_err() {
                        break (&mut ui).await;
                    }
                } else {
                    events_open = false;
                    let disconnected = phoxal_cli_observation::AttachmentEvent::ConnectionChanged(
                        phoxal_cli_observation::ConnectionObservation::Lost {
                            reason: "attachment event stream ended before a terminal observation"
                                .into(),
                        },
                    );
                    if send_input(&ingress_tx, SessionInput::Client(disconnected)).await.is_err() {
                        break (&mut ui).await;
                    }
                }
            }
            owned_exit = receive_owned_supervisor_exit(&mut owned_supervisor_exit) => {
                let input = match owned_exit {
                    Ok(OwnedSupervisorExit::Stopped) => SessionInput::OwnedSupervisorStopped,
                    Ok(OwnedSupervisorExit::Failed { reason }) => {
                        tracing::error!(%reason, "locally owned supervisor exited unsuccessfully");
                        SessionInput::OwnedSupervisorFailed
                    }
                    Err(error) => {
                        tracing::error!(%error, "locally owned supervisor exit observer ended");
                        SessionInput::OwnedSupervisorFailed
                    }
                };
                owned_supervisor_exit = None;
                if send_input(&ingress_tx, input).await.is_err() {
                    break (&mut ui).await;
                }
            }
            effect = guaranteed_rx.recv(), if guaranteed_open => {
                if let Some(effect) = effect {
                    spawn_effect(
                        &mut effect_tasks,
                        &mut confirmed_stop_tasks,
                        &effect_slots,
                        effect,
                        &effect_ports,
                        &ingress_tx,
                    );
                } else {
                    guaranteed_open = false;
                }
            }
            effect = command_rx.recv(), if commands_open => {
                if let Some(effect) = effect {
                    spawn_effect(
                        &mut effect_tasks,
                        &mut confirmed_stop_tasks,
                        &effect_slots,
                        effect,
                        &effect_ports,
                        &ingress_tx,
                    );
                } else {
                    commands_open = false;
                }
            }
            completed = effect_tasks.join_next(), if !effect_tasks.is_empty() => {
                if let Some(Err(error)) = completed {
                    let _ = ingress_tx.try_send(SessionInput::Diagnostic(format!(
                        "attachment effect task failed: {error}"
                    )));
                }
            }
            completed = confirmed_stop_tasks.join_next(), if !confirmed_stop_tasks.is_empty() => {
                if let Some(Err(error)) = completed {
                    let _ = ingress_tx.try_send(SessionInput::StopProjectFailed(format!(
                        "confirmed stop task failed: {error}"
                    )));
                }
            }
        }
    };
    session.ports.events.close();
    effect_tasks.abort_all();
    while effect_tasks.join_next().await.is_some() {}
    // A confirmed stop is a lifecycle command, not disposable UI work. Once
    // emitted, let it finish before closing the connection it is using.
    while confirmed_stop_tasks.join_next().await.is_some() {}
    drop(effect_ports);
    if tokio::time::timeout(SESSION_SHUTDOWN_BUDGET, session.shutdown())
        .await
        .is_err()
    {
        tracing::warn!(
            timeout_seconds = SESSION_SHUTDOWN_BUDGET.as_secs(),
            "attachment session shutdown timed out"
        );
    }
    outcome
}

async fn receive_owned_supervisor_exit(
    receiver: &mut Option<oneshot::Receiver<OwnedSupervisorExit>>,
) -> std::result::Result<OwnedSupervisorExit, oneshot::error::RecvError> {
    match receiver {
        Some(receiver) => receiver.await,
        None => std::future::pending().await,
    }
}

#[derive(Clone)]
struct EffectPorts {
    supervisor: crate::attach::SupervisorCommands,
    input: crate::attach::ports::input::InputCommands,
    logs: crate::attach::ports::logs::LogReader,
    runtimes: crate::attach::ports::runtimes::RuntimeReader,
}

fn spawn_effect(
    tasks: &mut JoinSet<()>,
    confirmed_stop_tasks: &mut JoinSet<()>,
    slots: &std::sync::Arc<Semaphore>,
    effect: Effect,
    ports: &EffectPorts,
    ingress: &mpsc::Sender<SessionInput>,
) {
    // A stop must never be dropped for want of a slot: it is the one effect an
    // operator explicitly confirmed.
    if matches!(effect, Effect::StopProject) {
        let ports = ports.clone();
        let ingress = ingress.clone();
        confirmed_stop_tasks.spawn(async move {
            if let Some(input) = route_effect(effect, &ports).await {
                let _ = ingress.send(input).await;
            }
        });
        return;
    }
    // Reads are local store queries, not commands, so they are not rationed
    // against the command budget.
    if matches!(effect, Effect::ReadLogs(_) | Effect::ReadRuntimes(_)) {
        let ports = ports.clone();
        let ingress = ingress.clone();
        tasks.spawn(async move {
            if let Some(input) = route_effect(effect, &ports).await {
                let _ = ingress.send(input).await;
            }
        });
        return;
    }
    let Ok(permit) = std::sync::Arc::clone(slots).try_acquire_owned() else {
        let _ = ingress.try_send(SessionInput::Diagnostic(
            "attachment command queue is full; retry".to_string(),
        ));
        return;
    };
    let ports = ports.clone();
    let ingress = ingress.clone();
    tasks.spawn(async move {
        let _permit = permit;
        if let Some(input) = route_effect(effect, &ports).await {
            let _ = ingress.send(input).await;
        }
    });
}

async fn send_input(sender: &mpsc::Sender<SessionInput>, input: SessionInput) -> Result<()> {
    sender
        .send(input)
        .await
        .context("attachment UI stopped while routing input")
}

async fn route_effect(effect: Effect, ports: &EffectPorts) -> Option<SessionInput> {
    if matches!(effect, Effect::StopProject) {
        return Some(stop_project_completion(ports.supervisor.stop().await));
    }
    let result = match effect {
        Effect::Restart {
            process,
            expected_producer,
        } => ports
            .supervisor
            .restart(process, expected_producer)
            .await
            .and_then(accepted)
            .map(|()| None),
        Effect::StopProject => unreachable!("stop effects are routed above"),
        Effect::InputSelect(device) => ports.input.select(device.0).await.map(|()| None),
        Effect::InputEnable(enabled) => ports.input.set_enabled(enabled).await.map(|()| None),
        Effect::InputRescan => ports.input.rescan().await.map(|()| None),
        Effect::ReadLogs(query) => Ok(Some(SessionInput::Logs(ports.logs.read(query).await))),
        Effect::ReadRuntimes(query) => Ok(Some(SessionInput::Runtimes(
            ports.runtimes.read(query).await,
        ))),
    };
    match result {
        Ok(input) => input,
        Err(error) => Some(SessionInput::Diagnostic(format!(
            "attachment command failed: {error:#}"
        ))),
    }
}

fn stop_project_completion(result: Result<CommandOutcome>) -> SessionInput {
    match result {
        Ok(CommandOutcome::Accepted { .. }) => SessionInput::StopProjectAccepted,
        Ok(CommandOutcome::Rejected { reason }) => {
            SessionInput::StopProjectRejected(rejection(reason).to_string())
        }
        Err(error) => SessionInput::StopProjectFailed(format!("{error:#}")),
    }
}

/// A rejection is a typed outcome the operator must see, not a transport
/// error: the supervisor answered, and it said no and why.
fn accepted(outcome: CommandOutcome) -> Result<()> {
    match outcome {
        CommandOutcome::Accepted { .. } => Ok(()),
        CommandOutcome::Rejected { reason } => Err(anyhow::anyhow!(
            "the supervisor rejected the command: {}",
            rejection(reason)
        )),
    }
}

fn rejection(reason: CommandRejection) -> &'static str {
    match reason {
        CommandRejection::UnknownParticipant => {
            "no process in the current snapshot has that key; the graph moved on"
        }
        CommandRejection::ProducerFenced => {
            "the target's producer is not the one you saw - it has already been replaced, or the \
             fresh incarnation has not opened its session yet"
        }
        CommandRejection::RevisionStale => {
            "the snapshot the command fenced on is no longer current; the execution moved on"
        }
        CommandRejection::Busy => "the supervisor is already applying another lifecycle command",
        CommandRejection::UnsupportedHostAction => {
            "the supervisor does not manage the host this execution runs on"
        }
        CommandRejection::ControlClosed => "the supervisor lifecycle loop has already ended",
        CommandRejection::Unknown => "the supervisor returned an unknown rejection",
    }
}

fn terminal_title(project: &Path) -> String {
    let name = project
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("project");
    format!("phoxal - {name}")
}

fn diagnostic_message(event: RuntimeEvent) -> String {
    match event {
        RuntimeEvent::PhaseStarted { id, label } => format!("{id}: {label}"),
        RuntimeEvent::PhaseFinished {
            id,
            outcome,
            elapsed,
        } => format!("{id}: {outcome:?} in {elapsed:?}"),
        RuntimeEvent::Diagnostic {
            source,
            level,
            message,
        } => format!("{source:?} {level:?}: {message}"),
    }
}

struct DiagnosticGuard;

impl Drop for DiagnosticGuard {
    fn drop(&mut self) {
        crate::cli::output::diagnostics::uninstall();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The operator sees the supervisor's own reason, not "command failed".
    #[test]
    fn every_rejection_reason_renders_something_actionable() {
        for reason in [
            CommandRejection::UnknownParticipant,
            CommandRejection::ProducerFenced,
            CommandRejection::RevisionStale,
            CommandRejection::Busy,
            CommandRejection::UnsupportedHostAction,
            CommandRejection::ControlClosed,
            CommandRejection::Unknown,
        ] {
            let error = accepted(CommandOutcome::Rejected { reason })
                .expect_err("a rejection is not silently accepted");
            let rendered = format!("{error:#}");
            assert!(rendered.contains("rejected"), "{rendered}");
            assert!(rendered.len() > "the supervisor rejected the command: ".len());
        }
        assert!(accepted(CommandOutcome::Accepted { at_revision: 1 }).is_ok());
    }

    #[test]
    fn stop_completion_preserves_typed_acceptance_rejection_and_failure() {
        assert_eq!(
            stop_project_completion(Ok(CommandOutcome::Accepted { at_revision: 7 })),
            SessionInput::StopProjectAccepted
        );
        assert_eq!(
            stop_project_completion(Ok(CommandOutcome::Rejected {
                reason: CommandRejection::Busy,
            })),
            SessionInput::StopProjectRejected(
                "the supervisor is already applying another lifecycle command".to_string()
            )
        );
        assert_eq!(
            stop_project_completion(Err(anyhow::anyhow!("transport closed"))),
            SessionInput::StopProjectFailed("transport closed".to_string())
        );
    }
}
