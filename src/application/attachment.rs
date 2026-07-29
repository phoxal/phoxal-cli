//! Composition root for the shared `run`/`attach` terminal application.

use std::path::Path;

use anyhow::{Context, Result};
use phoxal_cli_client::{Attachment, AttachmentPorts, SupervisorCommands, SupervisorFeed};
use phoxal_cli_protocol::CommandAction;
use phoxal_cli_ui::{AttachmentOutcome, Effect, EffectSenders, SessionInput, UiOptions};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;

use crate::AppContext;
use crate::commands::resident::ProjectTarget;

const UI_INGRESS_CAPACITY: usize = 256;
const EFFECT_CAPACITY: usize = 64;
const DIAGNOSTIC_CAPACITY: usize = 256;

pub(crate) async fn run(
    app: &AppContext,
    target: &ProjectTarget,
    feed: SupervisorFeed,
    commands: SupervisorCommands,
    shutdown_on_terminate: bool,
) -> Result<AttachmentOutcome> {
    let initial = feed.current();
    let attachment =
        phoxal_cli_client::attach_with_supervisor(target.runtime_target(), feed, commands, initial)
            .await?;
    drive(app, &target.project, attachment, shutdown_on_terminate).await
}

/// Drive the shared attachment UI.
///
/// SIGINT mirrors the UI's detach shortcut. For run-owned residents, the first
/// SIGTERM or SIGHUP requests orderly shutdown and a second restores the
/// terminal by detaching; attach-only sessions detach on the first signal.
async fn drive(
    app: &AppContext,
    project: &Path,
    attachment: Attachment,
    shutdown_on_terminate: bool,
) -> Result<AttachmentOutcome> {
    let Attachment { runtime, ports } = attachment;
    let AttachmentPorts {
        mut events,
        supervisor_commands,
        input_commands,
        logs,
        bus,
        runtimes,
    } = ports;
    let (ingress_tx, ingress_rx) = mpsc::channel(UI_INGRESS_CAPACITY);
    let (command_tx, mut command_rx) = mpsc::channel(EFFECT_CAPACITY);
    let (guaranteed_tx, mut guaranteed_rx) = mpsc::unbounded_channel();
    let (diagnostic_tx, mut diagnostic_rx) = mpsc::channel(DIAGNOSTIC_CAPACITY);
    crate::cli::output::diagnostics::install(diagnostic_tx);
    let _diagnostics = DiagnosticGuard;
    let options = UiOptions {
        title: terminal_title(project),
        theme: app.output.theme,
    };
    let ui = phoxal_cli_ui::run(
        ingress_rx,
        EffectSenders {
            guaranteed: guaranteed_tx,
            commands: command_tx,
        },
        options,
    );
    tokio::pin!(ui);
    let mut interrupts = signal(SignalKind::interrupt())?;
    let mut terminates = signal(SignalKind::terminate())?;
    let mut hangups = signal(SignalKind::hangup())?;
    let effect_ports = EffectPorts {
        supervisor: supervisor_commands,
        input: input_commands,
        logs,
        bus,
        runtimes,
    };
    let effect_slots = std::sync::Arc::new(Semaphore::new(EFFECT_CAPACITY));
    let mut effect_tasks = JoinSet::new();
    let mut shutdown_requested = false;

    let outcome = loop {
        tokio::select! {
            result = &mut ui => break result?,
            _ = interrupts.recv() => {
                send_input(&ingress_tx, SessionInput::Terminate).await?;
            }
            _ = terminates.recv() => {
                if shutdown_on_terminate {
                    if shutdown_requested {
                        send_input(&ingress_tx, SessionInput::Terminate).await?;
                    } else {
                        request_resident_shutdown(
                            &mut shutdown_requested,
                            &mut effect_tasks,
                            &effect_slots,
                            &effect_ports,
                            &ingress_tx,
                        );
                    }
                } else {
                    send_input(&ingress_tx, SessionInput::Terminate).await?;
                }
            }
            _ = hangups.recv() => {
                if shutdown_on_terminate {
                    if shutdown_requested {
                        send_input(&ingress_tx, SessionInput::Terminate).await?;
                    } else {
                        request_resident_shutdown(
                            &mut shutdown_requested,
                            &mut effect_tasks,
                            &effect_slots,
                            &effect_ports,
                            &ingress_tx,
                        );
                    }
                } else {
                    send_input(&ingress_tx, SessionInput::Terminate).await?;
                }
            }
            diagnostic = diagnostic_rx.recv() => {
                if let Some(diagnostic) = diagnostic {
                    send_input(
                        &ingress_tx,
                        SessionInput::Diagnostic(diagnostic_message(diagnostic)),
                    ).await?;
                }
            }
            event = events.recv() => {
                let Some(event) = event else {
                    anyhow::bail!(
                        "resident supervisor disconnected before a terminal observation"
                    );
                };
                send_input(&ingress_tx, SessionInput::Client(event)).await?;
            }
            effect = guaranteed_rx.recv() => {
                let Some(effect) = effect else {
                    anyhow::bail!("attachment UI closed its guaranteed effect channel unexpectedly");
                };
                spawn_effect(
                    &mut effect_tasks,
                    &effect_slots,
                    effect,
                    &effect_ports,
                    &ingress_tx,
                );
            }
            effect = command_rx.recv() => {
                let Some(effect) = effect else {
                    anyhow::bail!("attachment UI closed its command effect channel unexpectedly");
                };
                spawn_effect(
                    &mut effect_tasks,
                    &effect_slots,
                    effect,
                    &effect_ports,
                    &ingress_tx,
                );
            }
            completed = effect_tasks.join_next(), if !effect_tasks.is_empty() => {
                if let Some(Err(error)) = completed {
                    let _ = ingress_tx.try_send(SessionInput::Diagnostic(format!(
                        "attachment effect task failed: {error}"
                    )));
                }
            }
        }
    };
    effect_tasks.abort_all();
    while effect_tasks.join_next().await.is_some() {}
    runtime.shutdown().await;
    Ok(outcome)
}

#[derive(Clone)]
struct EffectPorts {
    supervisor: phoxal_cli_client::SupervisorCommands,
    input: phoxal_cli_client::InputCommands,
    logs: phoxal_cli_client::LogReader,
    bus: phoxal_cli_client::BusReader,
    runtimes: phoxal_cli_client::RuntimeReader,
}

fn request_resident_shutdown(
    shutdown_requested: &mut bool,
    tasks: &mut JoinSet<()>,
    slots: &std::sync::Arc<Semaphore>,
    ports: &EffectPorts,
    ingress: &mpsc::Sender<SessionInput>,
) {
    if !*shutdown_requested {
        *shutdown_requested = true;
        let _ = ingress.try_send(SessionInput::Diagnostic(
            "external termination requested; stopping the resident".to_string(),
        ));
        spawn_effect(tasks, slots, Effect::StopProject, ports, ingress);
    }
}

fn spawn_effect(
    tasks: &mut JoinSet<()>,
    slots: &std::sync::Arc<Semaphore>,
    effect: Effect,
    ports: &EffectPorts,
    ingress: &mpsc::Sender<SessionInput>,
) {
    if matches!(effect, Effect::StopProject) {
        let slots = std::sync::Arc::clone(slots);
        let ports = ports.clone();
        let ingress = ingress.clone();
        tasks.spawn(async move {
            let Ok(_permit) = slots.acquire_owned().await else {
                return;
            };
            if let Some(input) = route_effect(effect, &ports).await {
                let _ = ingress.send(input).await;
            }
        });
        return;
    }
    if matches!(
        effect,
        Effect::ReadLogs(_) | Effect::ReadBus(_) | Effect::ReadRuntimes(_)
    ) {
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
    let result = match effect {
        Effect::Restart {
            process,
            expected_producer,
        } => ports
            .supervisor
            .command_with_reconnect(CommandAction::Restart {
                process,
                expected_producer,
            })
            .await
            .and_then(accepted_reply)
            .map(|()| None),
        Effect::StopProject => ports
            .supervisor
            .command_with_reconnect(CommandAction::Shutdown)
            .await
            .and_then(accepted_reply)
            .map(|()| None),
        Effect::InputSelect(device) => ports.input.select(device.0).await.map(|()| None),
        Effect::InputEnable(enabled) => ports.input.set_enabled(enabled).await.map(|()| None),
        Effect::InputRescan => ports.input.rescan().await.map(|()| None),
        Effect::ReadLogs(query) => Ok(Some(SessionInput::Logs(ports.logs.read(query).await))),
        Effect::ReadBus(query) => Ok(Some(SessionInput::Bus(ports.bus.read(query).await))),
        Effect::ReadRuntimes(query) => Ok(Some(SessionInput::Runtimes(
            ports.runtimes.read(query).await,
        ))),
        Effect::Detach => Ok(None),
    };
    match result {
        Ok(input) => input,
        Err(error) => Some(SessionInput::Diagnostic(format!(
            "attachment command failed: {error:#}"
        ))),
    }
}

fn accepted_reply(reply: phoxal_cli_protocol::CommandReply) -> Result<()> {
    anyhow::ensure!(
        reply.accepted,
        "resident rejected command: {:?}",
        reply.error
    );
    Ok(())
}

fn terminal_title(project: &Path) -> String {
    let name = project
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("project");
    format!("phoxal - {name}")
}

fn diagnostic_message(event: phoxal_cli_core::session::event::SessionEvent) -> String {
    use phoxal_cli_core::session::event::SessionEvent;
    match event {
        SessionEvent::Diagnostic { message, .. } => message,
        other => format!("{other:?}"),
    }
}

struct DiagnosticGuard;

impl Drop for DiagnosticGuard {
    fn drop(&mut self) {
        crate::cli::output::diagnostics::uninstall();
    }
}
