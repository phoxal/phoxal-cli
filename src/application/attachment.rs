//! Composition root for the shared `run`/`attach` terminal application.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use phoxal_cli_client::{Attachment, AttachmentPorts, SupervisorCommands, SupervisorFeed};
use phoxal_cli_core::identity::ExecutionId;
use phoxal_cli_core::runtime::ProjectLifecycle;
use phoxal_cli_protocol::CommandAction;
use phoxal_cli_supervisor::resident::supervisor_socket_path;
use phoxal_cli_supervisor::{ProjectLock, ProjectLockStatus, ProjectOperation};
use phoxal_cli_ui::{AttachmentOutcome, Effect, EffectSenders, SessionInput, UiOptions};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;

use crate::cli::AppContext;

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
    phoxal_cli_client::validate_requested_entry(&target.runtime_target(), &initial.entry)?;
    match initial.lifecycle {
        ProjectLifecycle::Failed => {
            return Ok(AttachmentOutcome::ResidentFailed {
                reason: initial.failure.clone(),
            });
        }
        ProjectLifecycle::Stopped => return Ok(AttachmentOutcome::ResidentStopped),
        _ => {}
    }
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
    let mut initial_inputs = Vec::new();
    while let Ok(event) = events.try_recv() {
        initial_inputs.push(SessionInput::Client(event));
    }
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
        initial_inputs,
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
    let mut events_open = true;
    let mut guaranteed_open = true;
    let mut commands_open = true;
    let mut diagnostics_open = true;

    let outcome: Result<AttachmentOutcome> = loop {
        tokio::select! {
            result = &mut ui => break result,
            _ = interrupts.recv() => {
                // A closed ingress channel means the UI worker has already
                // returned - its receiver lives inside `run_blocking`
                // (session.rs), dropped only when that function returns.
                // The send error carries nothing beyond "the UI is
                // exiting", so award the outcome to whatever the UI future
                // actually resolved to instead of the send error. Awaiting
                // it here is also what guarantees the worker's
                // `TerminalGuard` (declared before the receiver's owning
                // `Application`, so dropped after it) has already restored
                // the terminal before this function returns, so anything
                // printed afterwards lands on the restored screen instead
                // of racing into the alternate screen.
                if send_input(&ingress_tx, SessionInput::Terminate)
                    .await
                    .is_err()
                {
                    break (&mut ui).await;
                }
            }
            _ = terminates.recv() => {
                if shutdown_on_terminate {
                    if shutdown_requested {
                        if send_input(&ingress_tx, SessionInput::Terminate)
                            .await
                            .is_err()
                        {
                            break (&mut ui).await;
                        }
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
                    if send_input(&ingress_tx, SessionInput::Terminate)
                        .await
                        .is_err()
                    {
                        break (&mut ui).await;
                    }
                }
            }
            _ = hangups.recv() => {
                if shutdown_on_terminate {
                    if shutdown_requested {
                        if send_input(&ingress_tx, SessionInput::Terminate)
                            .await
                            .is_err()
                        {
                            break (&mut ui).await;
                        }
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
                    if send_input(&ingress_tx, SessionInput::Terminate)
                        .await
                        .is_err()
                    {
                        break (&mut ui).await;
                    }
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
            event = events.recv(), if events_open => {
                if let Some(event) = event {
                    if send_input(&ingress_tx, SessionInput::Client(event))
                        .await
                        .is_err()
                    {
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
                    if send_input(&ingress_tx, SessionInput::Client(disconnected))
                        .await
                        .is_err()
                    {
                        break (&mut ui).await;
                    }
                }
            }
            effect = guaranteed_rx.recv(), if guaranteed_open => {
                if let Some(effect) = effect {
                    spawn_effect(
                        &mut effect_tasks,
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
        }
    };
    events.close();
    effect_tasks.abort_all();
    while effect_tasks.join_next().await.is_some() {}
    runtime.shutdown().await;
    outcome
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

/// The message for a `ResidentFailed` outcome: the supervisor-reported reason
/// when one reached the client, otherwise a pointer to the resident's own log
/// (the client never saw a terminal snapshot carrying a reason, e.g. the
/// connection was lost first).
pub(crate) fn resident_failure_message(project: &Path, reason: Option<&str>) -> String {
    match reason {
        Some(reason) => reason.to_string(),
        None => {
            let log =
                phoxal_cli_core::runtime::paths::RuntimePaths::for_root(project).supervisor_log();
            format!(
                "resident supervisor failed; see {} for the exact error",
                log.display()
            )
        }
    }
}

fn terminal_title(project: &Path) -> String {
    let name = project
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("project");
    format!("phoxal - {name}")
}

fn diagnostic_message(event: phoxal_cli_observation::RuntimeEvent) -> String {
    use phoxal_cli_observation::RuntimeEvent;
    match event {
        RuntimeEvent::Diagnostic { message, .. } => message,
        other => format!("{other:?}"),
    }
}

pub(crate) async fn attach_command(app: &AppContext, target: Option<&Path>) -> Result<()> {
    let target = resolve_target(target, app.project.root())?;
    let (feed, commands) = connect_running(&target).await?;
    match run(app, &target, feed, commands, false).await? {
        AttachmentOutcome::ResidentFailed { reason } => {
            bail!(resident_failure_message(&target.project, reason.as_deref()))
        }
        AttachmentOutcome::Detached | AttachmentOutcome::ResidentStopped => Ok(()),
    }
}

pub(crate) async fn stop_command(
    app: &AppContext,
    target: Option<&Path>,
    force: bool,
) -> Result<()> {
    let target = resolve_target(target, app.project.root())?;
    if force {
        let outcome = phoxal_cli_supervisor::resident::force_stop(&target.runtime_target()).await?;
        app.ui.info(format!("resident stopped ({outcome})"));
        return Ok(());
    }
    let (feed, commands) = connect_running(&target).await?;
    validate_entry(&target, &feed.current().entry)?;
    let reply = commands.command(CommandAction::Shutdown).await?;
    anyhow::ensure!(
        reply.accepted,
        "supervisor rejected shutdown: {:?}",
        reply.error
    );
    let mut snapshots = feed.subscribe();
    loop {
        let snapshot = snapshots.borrow_and_update().clone();
        if matches!(
            snapshot.lifecycle,
            ProjectLifecycle::Stopped | ProjectLifecycle::Failed
        ) {
            return if snapshot.lifecycle == ProjectLifecycle::Stopped {
                Ok(())
            } else {
                Err(anyhow::anyhow!(resident_failure_message(
                    &target.project,
                    snapshot.failure.as_deref()
                )))
            };
        }
        // The feed only drops its sender (what `changed()` reports here)
        // after giving up for good, possibly before a terminal snapshot
        // ever arrived. `snapshot` is the last one this loop actually
        // observed, so its reason (if the resident got that far) still
        // wins over the generic supervisor.log pointer.
        snapshots.changed().await.map_err(|_| {
            anyhow::anyhow!(resident_failure_message(
                &target.project,
                snapshot.failure.as_deref()
            ))
        })?;
    }
}

#[derive(Debug)]
pub(crate) struct ProjectTarget {
    pub(crate) project: PathBuf,
    runtime: phoxal_cli_core::runtime::RuntimeTarget,
}

impl ProjectTarget {
    pub(crate) fn runtime_target(&self) -> phoxal_cli_core::runtime::RuntimeTarget {
        self.runtime.clone()
    }
}

pub(crate) fn resolve_target(explicit: Option<&Path>, fallback: &Path) -> Result<ProjectTarget> {
    let runtime = phoxal_cli_project::resolve_target(explicit, fallback)?;
    let project = runtime.logical_root.clone();
    let _ = supervisor_socket_path(&project)?;
    Ok(ProjectTarget { project, runtime })
}

async fn connect_running(target: &ProjectTarget) -> Result<(SupervisorFeed, SupervisorCommands)> {
    loop {
        match ProjectLock::inspect(&target.project)? {
            ProjectLockStatus::Free => {
                bail!("project is not running: {}", target.project.display())
            }
            ProjectLockStatus::Held(identity) if identity.operation != ProjectOperation::Run => {
                bail!(
                    "project is held by `{}` (pid {}), not a resident run",
                    identity.operation,
                    identity.pid
                )
            }
            ProjectLockStatus::Held(_) => {}
        }
        let path = supervisor_socket_path(&target.project)?;
        match SupervisorFeed::connect(&path).await {
            Ok(feed) => {
                let commands = SupervisorCommands::connect(&path).await?;
                return Ok((feed, commands));
            }
            Err(error) if phoxal_cli_client::is_connection_unavailable(&error) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => bail!("cancelled while waiting for the resident supervisor to finish startup"),
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                }
            }
            Err(error) => return Err(error),
        }
    }
}

fn validate_entry(target: &ProjectTarget, running_entry: &str) -> Result<()> {
    phoxal_cli_client::validate_requested_entry(&target.runtime_target(), running_entry)
}

#[derive(Debug, Clone)]
pub(crate) struct BusTargetRequest {
    pub(crate) connect: String,
    pub(crate) namespace: Option<String>,
    pub(crate) robot_id: Option<String>,
    pub(crate) execution: Option<ExecutionId>,
}

fn finite_bus_target(
    app: &AppContext,
    target: &BusTargetRequest,
) -> Result<phoxal_cli_client::finite::BusTarget> {
    let execution = match target.execution {
        Some(execution) => execution,
        None => phoxal_cli_supervisor::active_execution(app.project.root())?.context(
            "no phoxal run is active in this project; start one, or name the run with --execution",
        )?,
    };
    Ok(phoxal_cli_client::finite::BusTarget {
        project_root: app.project.root().to_path_buf(),
        connect: target.connect.clone(),
        namespace: target.namespace.clone(),
        robot_id: target.robot_id.clone(),
        execution,
    })
}

pub(crate) async fn logs_command(
    app: &AppContext,
    target: &BusTargetRequest,
    participant: Option<String>,
    follow: bool,
) -> Result<()> {
    let target = finite_bus_target(app, target)?;
    phoxal_cli_client::finite::stream_logs(&target, participant, follow, render_log_event).await
}

fn render_log_event(event: phoxal_cli_client::finite::LogEvent) {
    use phoxal_cli_client::finite::LogEvent;
    match event {
        LogEvent::Record {
            participant_id,
            level,
            mut message,
            dropped,
            truncated,
        } => {
            if dropped > 0 {
                message.push_str(&format!(" (producer dropped {dropped})"));
            }
            if truncated > 0 {
                message.push_str(&format!(" (truncated {truncated})"));
            }
            println!("[{participant_id}] {level}: {message}");
        }
        LogEvent::Empty => eprintln!("no retained log events"),
        LogEvent::NoMatch { participant } => {
            eprintln!("no retained log events matched participant {participant}")
        }
        LogEvent::ToolLoss { delta, cumulative } => {
            eprintln!("tool-log lost {delta} event(s) before retention ({cumulative} cumulative)")
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum StatusQuery {
    Safety,
    Motion,
    Localization,
}

pub(crate) async fn status_command(
    app: &AppContext,
    target: &BusTargetRequest,
    query: StatusQuery,
) -> Result<()> {
    let target = finite_bus_target(app, target)?;
    let query = match query {
        StatusQuery::Safety => phoxal_cli_client::finite::StatusQuery::Safety,
        StatusQuery::Motion => phoxal_cli_client::finite::StatusQuery::Motion,
        StatusQuery::Localization => phoxal_cli_client::finite::StatusQuery::Localization,
    };
    let snapshot = phoxal_cli_client::finite::query_status(&target, query).await?;
    render_status(snapshot);
    Ok(())
}

fn render_status(snapshot: phoxal_cli_client::finite::StatusSnapshot) {
    use phoxal_cli_client::finite::StatusSnapshot;
    match snapshot {
        StatusSnapshot::Safety {
            clear,
            sequence,
            expires_at,
            constraints,
        } => {
            println!(
                "safety: {} (sequence {sequence}, expires at {expires_at})",
                if clear { "clear" } else { "constrained" },
            );
            for constraint in constraints {
                println!(
                    "  {} from {} stop={} linear_limit={:?} observed={:?}",
                    constraint.reason,
                    constraint.participant_id,
                    constraint.stop,
                    constraint.max_linear_speed_mps,
                    constraint.observed_value
                );
            }
        }
        StatusSnapshot::Motion {
            selected_source,
            linear_x_mps,
            angular_z_radps,
            zero_reason,
            component_estop_blocked,
            safety_constraints,
        } => {
            println!(
                "motion: source={selected_source} target=({linear_x_mps:.3} m/s, {angular_z_radps:.3} rad/s) zero={zero_reason}"
            );
            println!(
                "  component_estop_blocked={component_estop_blocked} safety_constraints={safety_constraints}"
            );
        }
        StatusSnapshot::Localization {
            x_m,
            y_m,
            yaw_rad,
            confidence,
        } => println!(
            "localization: ({x_m:.3}, {y_m:.3}) yaw={yaw_rad:.3} confidence={confidence:.3}"
        ),
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
    use std::fs;

    #[test]
    fn resolve_target_reaches_a_layout_root_socket() -> Result<()> {
        let layout = tempfile::tempdir()?;
        fs::write(layout.path().join("robot.yaml"), "schema: robot/v0\n")?;
        fs::create_dir_all(layout.path().join("bin"))?;

        let target = resolve_target(Some(layout.path()), layout.path())?;
        assert_eq!(target.project, layout.path().canonicalize()?);
        assert!(target.runtime_target().requested_entry.is_none());
        assert_eq!(
            supervisor_socket_path(&target.project)?,
            target.project.join(".phoxal/run/supervisor.sock")
        );
        Ok(())
    }

    #[tokio::test]
    async fn permanent_handshake_failure_is_returned_instead_of_retried() -> Result<()> {
        let project = tempfile::tempdir()?;
        fs::write(
            project.path().join("robot.yaml"),
            "schema: robot/v0\nname: test\n",
        )?;
        let identity = phoxal_cli_supervisor::ProjectLockIdentity::resolve(
            project.path(),
            ProjectOperation::Run,
        )
        .in_execution(ExecutionId::mint());
        let _lock = ProjectLock::acquire(identity)?;
        let path = supervisor_socket_path(project.path())?;
        fs::create_dir_all(path.parent().expect("socket has parent"))?;
        let listener = tokio::net::UnixListener::bind(&path)?;
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
        });
        let target = resolve_target(Some(project.path()), Path::new("/unused"))?;

        let result = tokio::time::timeout(Duration::from_secs(1), connect_running(&target))
            .await
            .expect("permanent handshake failure must not retry");
        let error = match result {
            Ok(_) => panic!("early-close peer unexpectedly connected"),
            Err(error) => error,
        };
        peer.await.unwrap();
        assert!(
            format!("{error:#}")
                .contains("resident rejected or disconnected during the supervisor handshake")
        );
        Ok(())
    }
}
