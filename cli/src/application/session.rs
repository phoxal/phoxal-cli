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
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::JoinSet;

use super::launcher::OwnedSession;
use crate::attach::{Attachment, SessionPorts};
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
/// Every local session owns the supervisor it launched. This side channel lets
/// that owner end the attachment with its own account of why, rather than with
/// the transport's report of the identity token it lost as a consequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OwnedSupervisorExit {
    Stopped,
    Failed { reason: String },
}

/// Everything a driven session may own beyond the attachment itself.
#[derive(Default)]
pub(crate) struct SessionOwnership {
    /// The session this client launched, and can therefore stop. Absent for an
    /// attachment to an execution somebody else started.
    pub(crate) owned: Option<OwnedSession>,
    /// The exit of the supervisor this client launched, when it has one.
    pub(crate) supervisor_exit: Option<oneshot::Receiver<OwnedSupervisorExit>>,
}

/// Drive one attachment through the terminal application until it ends.
pub(crate) async fn drive(
    app: &AppContext,
    project: &Path,
    session: Attachment,
    detachable: Detachable,
    ownership: SessionOwnership,
) -> Result<AttachmentOutcome> {
    let SessionOwnership {
        owned,
        supervisor_exit,
    } = ownership;
    let mut owned_supervisor_exit = supervisor_exit;
    let Attachment { ports, .. } = &session;
    let SessionPorts {
        events: _,
        input_commands,
        logs,
        runtimes,
    } = ports;
    let effect_ports = EffectPorts {
        owned,
        input: input_commands.clone(),
        logs: logs.clone(),
        runtimes: runtimes.clone(),
    };
    let stoppable = effect_ports.owned.is_some();
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
        stoppable,
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
                        SessionInput::OwnedSupervisorFailed(reason)
                    }
                    Err(error) => {
                        tracing::error!(%error, "locally owned supervisor exit observer ended");
                        SessionInput::OwnedSupervisorFailed(format!(
                            "the supervisor exit observer ended: {error}"
                        ))
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
                    let _ = ingress_tx.try_send(SessionInput::StopSessionFailed(format!(
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

#[allow(clippy::future_not_send)]
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
    /// Present exactly when this client launched the session it is attached
    /// to. Stopping is ending those processes, not sending a command: the
    /// supervisor starts nothing and has no stop to accept.
    owned: Option<OwnedSession>,
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
    if matches!(effect, Effect::StopSession) {
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
    if matches!(effect, Effect::StopSession) {
        return Some(stop_session_completion(ports.owned.as_ref()).await);
    }
    let result = match effect {
        Effect::StopSession => unreachable!("stop effects are routed above"),
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

/// End the session this client launched, and report what happened.
///
/// A client with nothing of its own cannot reach here - the UI refuses the key
/// before emitting the effect - but the absence is reported rather than
/// assumed away, because an effect that silently did nothing would look to an
/// operator exactly like a stop that worked.
async fn stop_session_completion(owned: Option<&OwnedSession>) -> SessionInput {
    let Some(owned) = owned else {
        return SessionInput::StopSessionFailed(
            "this client launched nothing here, so there is nothing for it to stop".to_string(),
        );
    };
    match owned.stop().await {
        Ok(()) => SessionInput::SessionStopped,
        Err(error) => SessionInput::StopSessionFailed(format!("{error:#}")),
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

    /// A stop is the operator's own confirmed act, so an absent owner is
    /// reported rather than treated as a stop that worked. This can only
    /// happen through the effect router - the dashboard refuses the key first -
    /// and even then it says something true.
    #[tokio::test]
    async fn stopping_with_nothing_owned_reports_it_instead_of_claiming_success() {
        let input = stop_session_completion(None).await;
        assert!(
            matches!(
                &input,
                SessionInput::StopSessionFailed(reason) if reason.contains("launched nothing")
            ),
            "{input:?}"
        );
    }
}
