//! Composition root for the shared `run`/`attach` terminal application.

use std::path::Path;

use anyhow::{Context, Result};
use phoxal_cli_client::{Attachment, AttachmentPorts, SupervisorCommands, SupervisorFeed};
use phoxal_cli_protocol::CommandAction;
use phoxal_cli_ui::{AttachmentOutcome, Effect, SessionInput, UiOptions};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

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
) -> Result<AttachmentOutcome> {
    let initial = feed.current();
    let attachment =
        phoxal_cli_client::attach_with_supervisor(target.runtime_target(), feed, commands, initial)
            .await?;
    drive(app, &target.project, attachment).await
}

async fn drive(
    app: &AppContext,
    project: &Path,
    attachment: Attachment,
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
    let (effect_tx, mut effect_rx) = mpsc::channel(EFFECT_CAPACITY);
    let (diagnostic_tx, mut diagnostic_rx) = mpsc::channel(DIAGNOSTIC_CAPACITY);
    crate::cli::output::diagnostics::install(diagnostic_tx);
    let _diagnostics = DiagnosticGuard;
    let options = UiOptions {
        title: terminal_title(project),
        theme: app.output.theme,
    };
    let ui = phoxal_cli_ui::run(ingress_rx, effect_tx, options);
    tokio::pin!(ui);
    let mut terminates = signal(SignalKind::terminate())?;
    let mut hangups = signal(SignalKind::hangup())?;

    let outcome = loop {
        tokio::select! {
            result = &mut ui => break result?,
            _ = terminates.recv() => {
                send_input(&ingress_tx, SessionInput::Terminate).await?;
            }
            _ = hangups.recv() => {
                send_input(&ingress_tx, SessionInput::Terminate).await?;
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
            effect = effect_rx.recv() => {
                let Some(effect) = effect else {
                    anyhow::bail!("attachment UI closed its effect channel unexpectedly");
                };
                if let Some(input) = route_effect(
                    effect,
                    &supervisor_commands,
                    &input_commands,
                    &logs,
                    &bus,
                    &runtimes,
                ).await {
                    send_input(&ingress_tx, input).await?;
                }
            }
        }
    };
    runtime.shutdown().await;
    Ok(outcome)
}

async fn send_input(sender: &mpsc::Sender<SessionInput>, input: SessionInput) -> Result<()> {
    sender
        .send(input)
        .await
        .context("attachment UI stopped while routing input")
}

async fn route_effect(
    effect: Effect,
    supervisor: &phoxal_cli_client::SupervisorCommands,
    input: &phoxal_cli_client::InputCommands,
    logs: &phoxal_cli_client::LogReader,
    bus: &phoxal_cli_client::BusReader,
    runtimes: &phoxal_cli_client::RuntimeReader,
) -> Option<SessionInput> {
    let result = match effect {
        Effect::Restart {
            process,
            expected_producer,
        } => supervisor
            .command_with_reconnect(CommandAction::Restart {
                process,
                expected_producer,
            })
            .await
            .and_then(accepted_reply)
            .map(|()| None),
        Effect::StopProject => supervisor
            .command_with_reconnect(CommandAction::Shutdown)
            .await
            .and_then(accepted_reply)
            .map(|()| None),
        Effect::InputSelect(device) => input.select(device.0).await.map(|()| None),
        Effect::InputEnable(enabled) => input.set_enabled(enabled).await.map(|()| None),
        Effect::InputRescan => input.rescan().await.map(|()| None),
        Effect::ReadLogs(query) => Ok(Some(SessionInput::Logs(logs.read(query).await))),
        Effect::ReadBus(query) => Ok(Some(SessionInput::Bus(bus.read(query).await))),
        Effect::ReadRuntimes(query) => Ok(Some(SessionInput::Runtimes(runtimes.read(query).await))),
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
