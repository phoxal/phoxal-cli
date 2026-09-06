//! Backend-neutral local world lifecycle and robot connection workflow.

mod observation;
mod start;
mod terminal;

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use phoxal::model::identity::SpawnId;
use phoxal::session::WorldSessionClient;
use phoxal::version::FrameworkVersion;
use phoxal::world::api::session::connect::WorldSessionBootstrap;
use phoxal::world::api::session::control::WorldControl;
use phoxal::world::api::session::state::WorldSessionState;
use phoxal::world::api::session::{
    WorldLifecycle, WorldMemberCleanup, WorldMemberEndReason, WorldMemberPhase, WorldMotion,
};
use phoxal_cli_host::world::{
    DEFAULT_LOG_BYTE_LIMIT, DEFAULT_TERMINAL_SESSION_LIMIT, LocalWorldRegistration,
    TerminalOutcome, WorldEvidence, WorldMemberEvidence, WorldPaths, WorldRegistry,
    WorldTerminalSummary,
};

use crate::cli::context::AppContext;
use crate::cli::exit::ReportedExit;
use crate::cli::output::welcome::{Mode, StepId};

use super::summary::SessionSummary;
use observation::{
    StatusReport, ensure_state_matches_registration, lifecycle_text, load_list, load_logs,
    load_status, print_live_status, print_terminal_status, stop_world,
};
use phoxal_cli_host::world_process::LaunchedWorldHost;
use start::{rollback_host, start_world};
use terminal::open_world_tui;

#[cfg(test)]
use observation::format_live_status;
#[cfg(test)]
use terminal::{spawn_diagnostics_feed, spawn_state_feed};

const WORLD_UI_INGRESS_CAPACITY: usize = 64;
const ATTACHMENT_BUDGET: Duration = Duration::from_secs(5 * 60);
const STOP_BUDGET: Duration = Duration::from_secs(60);
const TERMINAL_POLL_INTERVAL: Duration = Duration::from_millis(100);
const STREAM_RECONNECT_DELAY: Duration = Duration::from_millis(100);

pub(super) struct StartedWorld {
    registration: LocalWorldRegistration,
    host: LaunchedWorldHost,
}

/// Inputs resolved before a `simulation run` creates its transaction-owned
/// world process.
struct ConnectionIntent {
    spawn: Option<SpawnId>,
    target: super::lifecycle::Target,
    detach: bool,
}

enum ConnectedSimulationEnding {
    World(Box<WorldTerminalSummary>),
    Member(WorldMemberEvidence),
}

pub(crate) async fn run_command(app: &AppContext, world: &Path, spawn: Option<&str>) -> Result<()> {
    let intent = connection_intent(app, spawn, false)?;
    let started = start_world(app, world).await?;
    connect_world(
        app,
        &started.registration.instance.to_string(),
        intent,
        Some(started),
    )
    .await
}

pub(crate) async fn start_command(app: &AppContext, world: &Path) -> Result<()> {
    let started = start_world(app, world).await?;
    let instance = started.registration.instance.to_string();
    started.host.detach();
    app.ui.info(format!(
        "world instance {instance} ready and paused; open with `phoxal simulation open {instance}` or stop with `phoxal simulation stop {instance}`"
    ));
    Ok(())
}

pub(crate) async fn open_command(app: &AppContext, instance: &str) -> Result<()> {
    let stores = Stores::discover()?;
    let registration = stores.registry.resolve(instance)?;
    open_world_tui(app, registration).await
}

pub(crate) async fn connect_command(
    app: &AppContext,
    instance: &str,
    spawn: Option<&str>,
    detach: bool,
) -> Result<()> {
    connect_world(app, instance, connection_intent(app, spawn, detach)?, None).await
}

pub(crate) async fn status_command(_app: &AppContext, instance: &str) -> Result<()> {
    let stores = Stores::discover()?;
    match load_status(&stores, instance).await? {
        StatusReport::Live(state) => print_live_status(&state),
        StatusReport::Terminal { summary, members } => {
            print_terminal_status(&summary, &members);
        }
    }
    Ok(())
}

pub(crate) async fn logs_command(_app: &AppContext, instance: &str) -> Result<()> {
    let stores = Stores::discover()?;
    let logs = load_logs(&stores, instance).await?;
    if logs.is_empty() {
        eprintln!("no retained world process logs");
    }
    for (name, bytes) in logs {
        println!("== {name} ==");
        print!("{}", String::from_utf8_lossy(&bytes));
        if !bytes.ends_with(b"\n") {
            println!();
        }
    }
    Ok(())
}

pub(crate) async fn list_command(_app: &AppContext, all: bool) -> Result<()> {
    let stores = Stores::discover()?;
    let report = load_list(&stores, all).await?;
    for registration in &report.live {
        println!(
            "{}  live      {}  {}  train {}",
            registration.instance,
            registration.world.id,
            registration.world.digest,
            registration.framework
        );
    }
    for summary in &report.terminal {
        println!(
            "{}  {:<9} {}  {}  train {}",
            summary.instance,
            summary.outcome.kind(),
            summary.provenance.world,
            summary.provenance.digest,
            summary.provenance.framework
        );
    }
    if report.live.is_empty() && !all {
        println!("no live world sessions");
    }
    Ok(())
}

pub(crate) async fn stop_command(_app: &AppContext, instance: &str) -> Result<()> {
    let stores = Stores::discover()?;
    let registration = stores.registry.resolve(instance)?;
    stop_world(registration, &stores).await?;
    println!("stopped world {instance}");
    Ok(())
}

pub(crate) async fn pause_command(_app: &AppContext, instance: &str) -> Result<()> {
    control_world(instance, WorldControl::Pause).await
}

pub(crate) async fn resume_command(_app: &AppContext, instance: &str) -> Result<()> {
    control_world(instance, WorldControl::Resume).await
}

async fn control_world(instance: &str, operation: WorldControl) -> Result<()> {
    let stores = Stores::discover()?;
    let registration = stores.registry.resolve(instance)?;
    let client = connect_verified(&registration).await?;
    let state = client.control(operation).await?;
    ensure_state_matches_registration(&state, &registration)?;
    print_live_status(&state);
    Ok(())
}

fn connection_intent(
    app: &AppContext,
    spawn: Option<&str>,
    detach: bool,
) -> Result<ConnectionIntent> {
    let spawn = spawn
        .map(SpawnId::new)
        .transpose()
        .context("invalid world spawn name")?;
    let target = super::lifecycle::Target::resolve(None, app.project.root())?;
    Ok(ConnectionIntent {
        spawn,
        target,
        detach,
    })
}

async fn connect_world(
    app: &AppContext,
    instance: &str,
    intent: ConnectionIntent,
    mut started: Option<StartedWorld>,
) -> Result<()> {
    let stores = match Stores::discover() {
        Ok(stores) => stores,
        Err(error) => return Err(rollback_started_world(started.take(), error).await),
    };
    let registration = match &started {
        Some(started) => started.registration.clone(),
        None => stores.registry.resolve(instance)?,
    };
    let client = match connect_verified(&registration).await {
        Ok(client) => client,
        Err(error) => return Err(rollback_started_world(started.take(), error).await),
    };
    let project = app.project.root().to_path_buf();
    let offline = app.offline;
    let locked = tokio::task::spawn_blocking(move || {
        phoxal_cli_project::source::train::resolve_locked_train(&project, offline)
    })
    .await
    .context("locked framework preflight worker failed")
    .and_then(|locked| locked);
    let locked = match locked {
        Ok(locked) => locked,
        Err(error) => return Err(rollback_started_world(started.take(), error).await),
    };
    if let Err(error) = ensure_compatible_train(registration.framework, locked.framework()) {
        return Err(rollback_started_world(started.take(), error).await);
    }

    // The compatibility decision is deliberately before project preparation,
    // package materialization, supervisor launch, or native scene mutation.
    connect_fresh_execution(app, registration, client, intent, started).await
}

fn ensure_compatible_train(world: FrameworkVersion, robot: FrameworkVersion) -> Result<()> {
    ensure!(
        world.is_compatible_with(robot),
        "world instance uses framework {world}, but this robot lockfile resolves {robot}; the world accepts {}, so select a robot patch on that line before any build or launch",
        world.compatibility_line()
    );
    Ok(())
}

async fn connect_fresh_execution(
    app: &AppContext,
    registration: LocalWorldRegistration,
    client: WorldSessionClient,
    intent: ConnectionIntent,
    mut started: Option<StartedWorld>,
) -> Result<()> {
    let ConnectionIntent {
        spawn,
        target,
        detach,
    } = intent;
    let startup = super::startup::Startup::begin(app, &target.project, Mode::Simulation);
    let prepared =
        match super::lifecycle::prepare_driver_free_execution(app, &target, &startup).await {
            Ok(prepared) => prepared,
            Err(error) => {
                let error = rollback_started_world(started.take(), error).await;
                return Err(startup.failed(error));
            }
        };
    let launched =
        match super::lifecycle::launch_driver_free_execution(&target, &startup, prepared).await {
            Ok(launched) => launched,
            Err(error) => {
                let error = rollback_started_world(started.take(), error).await;
                return Err(startup.failed(error));
            }
        };
    if let Err(error) = launched.ensure_drivers_absent() {
        let error = rollback_before_active(launched, started.take(), error).await;
        return Err(startup.failed(error));
    }

    let mut states = match client.state_subscription().await {
        Ok(states) => states,
        Err(error) => {
            let error = rollback_before_active(launched, started.take(), error.into()).await;
            return Err(startup.failed(error));
        }
    };
    let execution = launched.session.connected().execution;
    startup.step(
        StepId::Attachment,
        format!("attaching {execution} to {}", registration.instance),
    );
    let attached = match client
        .attach(execution, target.endpoint.clone(), spawn)
        .await
    {
        Ok(state) => state,
        Err(error) => {
            let error = rollback_before_active(launched, started.take(), error.into()).await;
            return Err(startup.failed(error));
        }
    };
    if !member_is_active(&attached, execution) {
        let active = tokio::select! {
            result = tokio::time::timeout(
                ATTACHMENT_BUDGET,
                states.wait_for_member_active(execution),
            ) => match result {
                Ok(result) => result.map(|_| ()).map_err(anyhow::Error::from),
                Err(_) => Err(anyhow::anyhow!(
                    "timed out after {}s waiting for world member {execution} to become active",
                    ATTACHMENT_BUDGET.as_secs()
                )),
            },
            () = startup.cancelled() => Err(anyhow::anyhow!(
                "world attachment was interrupted before member {execution} became active"
            )),
        };
        if let Err(error) = active {
            let error = rollback_before_active(launched, started.take(), error).await;
            return Err(startup.failed(error));
        }
    }

    startup.complete(
        StepId::Attachment,
        format!("member {execution} active in {}", registration.instance),
    );
    if let Some(started) = started.take() {
        started.host.detach();
    }
    startup.ready();
    if detach {
        launched.detach();
        report_detached_attachment(app, execution, &attached, registration.instance);
        report_world_commands(app, registration.instance);
        return Ok(());
    }
    let outcome =
        super::lifecycle::drive_launched_session(app, &target, launched, Mode::Simulation).await?;
    if outcome == phoxal_cli_ui::AttachmentOutcome::Detached {
        super::lifecycle::report_outcome(app, &target, &outcome)?;
        report_world_commands(app, registration.instance);
        return Ok(());
    }

    let stores = Stores::discover()?;
    let ending =
        await_connected_simulation_ending(&stores, &registration, &client, execution).await?;
    let world_remains_live = matches!(&ending, ConnectedSimulationEnding::Member(_));
    report_connected_simulation_ending(app, &target, &ending)?;
    if world_remains_live {
        report_world_commands(app, registration.instance);
    }
    Ok(())
}

fn report_detached_attachment(
    app: &AppContext,
    execution: phoxal::identity::ExecutionId,
    state: &WorldSessionState,
    instance: phoxal::model::world::WorldInstanceId,
) {
    match state.lifecycle {
        WorldLifecycle::Ready {
            motion: WorldMotion::Paused,
        } => app.ui.success(format!(
            "robot execution {execution} is active; world physics is paused. Resume with `phoxal simulation resume {instance}`"
        )),
        WorldLifecycle::Ready {
            motion: WorldMotion::Running,
        } => app.ui.success(format!(
            "robot execution {execution} is active; world physics is running"
        )),
        WorldLifecycle::Stopping | WorldLifecycle::Failed { .. } | WorldLifecycle::Starting => {
            app.ui.info(format!(
                "robot execution {execution} attached, but world lifecycle is {:?}",
                state.lifecycle
            ));
        }
    }
}

fn report_world_commands(app: &AppContext, instance: phoxal::model::world::WorldInstanceId) {
    app.ui.info(format!(
        "world physics is independent of robot execution. Inspect it with `phoxal simulation status {instance}`, control it with `phoxal simulation pause {instance}` or `phoxal simulation resume {instance}`, open the terminal view with `phoxal simulation open {instance}`, or stop it with `phoxal simulation stop {instance}`"
    ));
}

async fn await_connected_simulation_ending(
    stores: &Stores,
    registration: &LocalWorldRegistration,
    client: &WorldSessionClient,
    execution: phoxal::identity::ExecutionId,
) -> Result<ConnectedSimulationEnding> {
    let instance = registration.instance.to_string();
    tokio::time::timeout(STOP_BUDGET, async {
        loop {
            if let Some(summary) = stores.evidence.read_summary(&instance)? {
                return Ok(ConnectedSimulationEnding::World(Box::new(summary)));
            }
            if stores.registry.find(&instance)?.is_some() {
                if let Some(member) = stores
                    .evidence
                    .read_member_terminal(&instance, execution)?
                {
                    // World stop persists member evidence before its summary and registration
                    // removal. A held registration alone cannot prove this was member-only.
                    if let Ok(state) = client.current_state().await {
                        ensure_state_matches_registration(&state, registration)?;
                        if matches!(state.lifecycle, WorldLifecycle::Ready { .. })
                            && !state.members.iter().any(|member| member.execution == execution)
                        {
                            return Ok(ConnectedSimulationEnding::Member(member));
                        }
                    }
                }
            } else if let Some(summary) = stores.recover_terminal(&instance).await? {
                return Ok(ConnectedSimulationEnding::World(Box::new(summary)));
            }
            tokio::time::sleep(TERMINAL_POLL_INTERVAL).await;
        }
    })
    .await
    .with_context(|| {
        format!(
            "timed out after {}s waiting for typed terminal evidence for member {execution} in world {instance}",
            STOP_BUDGET.as_secs()
        )
    })?
}

fn report_connected_simulation_ending(
    app: &AppContext,
    target: &super::lifecycle::Target,
    ending: &ConnectedSimulationEnding,
) -> Result<()> {
    let (description, failed) = connected_simulation_ending_description(ending);
    SessionSummary::new(description, vec![target.paths().supervisor_log()])
        .print(&target.project, app.output.theme);
    if failed {
        return Err(ReportedExit(1).into());
    }
    Ok(())
}

fn connected_simulation_ending_description(ending: &ConnectedSimulationEnding) -> (String, bool) {
    match ending {
        ConnectedSimulationEnding::World(summary) => {
            let failed = matches!(&summary.outcome, TerminalOutcome::Failed { .. });
            let detail = summary
                .outcome
                .detail()
                .map_or_else(String::new, |detail| format!(": {detail}"));
            (
                format!(
                    "world terminal outcome {}/{:?}{detail}",
                    summary.outcome.kind(),
                    summary.outcome.reason()
                ),
                failed,
            )
        }
        ConnectedSimulationEnding::Member(member) => {
            let (cleanup, cleanup_failed) = match &member.terminal.cleanup {
                WorldMemberCleanup::Complete => ("complete".to_owned(), false),
                WorldMemberCleanup::Incomplete { detail } => {
                    (format!("incomplete: {detail}"), true)
                }
            };
            (
                format!(
                    "simulation member terminal outcome {:?}; cleanup {cleanup}",
                    member.terminal.reason
                ),
                cleanup_failed || member.terminal.reason != WorldMemberEndReason::Stopped,
            )
        }
    }
}

fn member_is_active(state: &WorldSessionState, execution: phoxal::identity::ExecutionId) -> bool {
    state
        .members
        .iter()
        .any(|member| member.execution == execution && member.phase == WorldMemberPhase::Active)
}

async fn rollback_before_active(
    launched: super::lifecycle::LaunchedSession,
    started: Option<StartedWorld>,
    error: anyhow::Error,
) -> anyhow::Error {
    let error = match super::lifecycle::rollback_launched_session(launched).await {
        Ok(()) => error,
        Err(cleanup) => error.context(format!("robot rollback also failed: {cleanup:#}")),
    };
    rollback_started_world(started, error).await
}

async fn rollback_started_world(
    started: Option<StartedWorld>,
    error: anyhow::Error,
) -> anyhow::Error {
    let Some(started) = started else {
        return error;
    };
    rollback_host(started.host, error).await
}

async fn connect_verified(registration: &LocalWorldRegistration) -> Result<WorldSessionClient> {
    let client = WorldSessionClient::connect(&registration.endpoint)
        .await
        .with_context(|| {
            format!(
                "failed to connect to live world instance {}",
                registration.instance
            )
        })?;
    ensure_bootstrap_matches_registration(client.bootstrap(), registration)?;
    Ok(client)
}

async fn current_verified(
    registration: &LocalWorldRegistration,
) -> Result<(WorldSessionClient, WorldSessionState)> {
    let client = connect_verified(registration).await?;
    let state = client
        .current_state()
        .await
        .context("failed to read current world state")?;
    ensure_state_matches_registration(&state, registration)?;
    Ok((client, state))
}

fn ensure_ready_and_paused(state: &WorldSessionState) -> Result<()> {
    ensure!(
        matches!(
            state.lifecycle,
            WorldLifecycle::Ready {
                motion: WorldMotion::Paused
            }
        ),
        "world host published readiness while its authoritative lifecycle was {}",
        lifecycle_text(state.lifecycle)
    );
    Ok(())
}

fn ensure_bootstrap_matches_registration(
    bootstrap: &WorldSessionBootstrap,
    registration: &LocalWorldRegistration,
) -> Result<()> {
    ensure!(
        bootstrap.instance == registration.instance,
        "world endpoint registered for {} bootstrapped as {}; refusing mismatched locator",
        registration.instance,
        bootstrap.instance
    );
    ensure!(
        bootstrap.framework == registration.framework,
        "world {} registered framework {} but bootstrapped as {}",
        registration.instance,
        registration.framework,
        bootstrap.framework
    );
    ensure!(
        bootstrap.world == registration.world.id,
        "world {} registered authored ID {} but bootstrapped as {}",
        registration.instance,
        registration.world.id,
        bootstrap.world
    );
    ensure!(
        bootstrap.digest == registration.world.digest,
        "world {} registered digest {} but bootstrapped as {}",
        registration.instance,
        registration.world.digest,
        bootstrap.digest
    );
    Ok(())
}

struct Stores {
    paths: WorldPaths,
    registry: WorldRegistry,
    evidence: WorldEvidence,
}

impl Stores {
    fn discover() -> Result<Self> {
        Ok(Self::at(WorldPaths::discover()?))
    }

    fn at(paths: WorldPaths) -> Self {
        Self {
            registry: WorldRegistry::new(
                paths.clone(),
                phoxal_cli_host::world::SystemProcessInspector,
            ),
            evidence: WorldEvidence::new(paths.clone()),
            paths,
        }
    }

    fn prune(&self) -> Result<()> {
        let live = self
            .registry
            .list()?
            .into_iter()
            .map(|registration| registration.instance.to_string())
            .collect::<BTreeSet<_>>();
        let report = self.evidence.prune(DEFAULT_TERMINAL_SESSION_LIMIT, &live)?;
        for path in report.incomplete {
            tracing::warn!(path = %path.display(), "incomplete world evidence was retained");
        }
        Ok(())
    }

    async fn recover_terminal(&self, instance: &str) -> Result<Option<WorldTerminalSummary>> {
        let paths = self.paths.clone();
        let instance = instance.to_owned();
        tokio::task::spawn_blocking(move || {
            let inspector = phoxal_cli_host::world::SystemProcessInspector;
            let registry = WorldRegistry::new(paths.clone(), inspector);
            let evidence = WorldEvidence::new(paths);
            registry.recover_host_loss(&evidence, &instance, &inspector)
        })
        .await
        .context("world host-loss recovery worker failed")?
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File, OpenOptions};
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use super::*;
    use phoxal::identity::{ExecutionId, ProducerId, RobotId};
    use phoxal::model::identity::WorldId;
    use phoxal::model::world::{WorldDigest, WorldInstanceId, WorldProgress, WorldProvenance};
    use phoxal::supervisor::api::simulation::SimulationEndReason;
    use phoxal::world::api::session::diagnostics::WorldSessionDiagnostics;
    use phoxal::world::{WorldSessionHandler, WorldSessionOperation, WorldSessionServer};
    use phoxal_cli_host::world::{
        ProcessIdentity, ProcessInspector, REGISTRATION_SCHEMA, RegisteredWorld,
        SystemProcessInspector, TERMINAL_SUMMARY_SCHEMA, TerminalCleanup, TerminalFailure,
        TerminalRetention,
    };
    use serde::Serialize;
    use tokio::sync::broadcast;

    const WORKFLOW_INSTANCE: &str = "3234567890abcdef1234567890abcdef";

    struct WorkflowWorld {
        bootstrap: WorldSessionBootstrap,
        state: Mutex<WorldSessionState>,
        states: Mutex<broadcast::Sender<WorldSessionState>>,
        diagnostics: Mutex<WorldSessionDiagnostics>,
        diagnostic_updates: Mutex<broadcast::Sender<WorldSessionDiagnostics>>,
        paths: WorldPaths,
    }

    impl WorkflowWorld {
        fn new(paths: WorldPaths) -> Self {
            let instance = WorldInstanceId::parse(WORKFLOW_INSTANCE).unwrap();
            let world = WorldId::new("warehouse").unwrap();
            let digest = WorldDigest::parse(&"c".repeat(64)).unwrap();
            let bootstrap = WorldSessionBootstrap {
                instance,
                framework: FrameworkVersion::CURRENT,
                world: world.clone(),
                digest,
            };
            let state = WorldSessionState {
                revision: 1,
                instance,
                provenance: WorldProvenance {
                    world,
                    digest,
                    random_seed: 17,
                    framework: FrameworkVersion::CURRENT,
                    adapter: "workflow-test".to_owned(),
                    adapter_version: "1".to_owned(),
                    simulator_version: "fake".to_owned(),
                    platform: "test".to_owned(),
                    time_step_ns: 10_000_000,
                },
                lifecycle: WorldLifecycle::Ready {
                    motion: WorldMotion::Paused,
                },
                progress: WorldProgress::at(3, 10_000_000).unwrap(),
                members: Vec::new(),
            };
            let (states, _) = broadcast::channel(8);
            let (diagnostic_updates, _) = broadcast::channel(8);
            Self {
                bootstrap,
                state: Mutex::new(state),
                states: Mutex::new(states),
                diagnostics: Mutex::new(WorldSessionDiagnostics {
                    revision: 2,
                    pacing: None,
                    last_transition_age_ns: Some(1),
                }),
                diagnostic_updates: Mutex::new(diagnostic_updates),
                paths,
            }
        }

        fn rotate_state_stream(&self) {
            let (replacement, _) = broadcast::channel(8);
            *self.states.lock().unwrap() = replacement;
        }

        fn rotate_diagnostics_stream(&self) {
            let (replacement, _) = broadcast::channel(8);
            *self.diagnostic_updates.lock().unwrap() = replacement;
        }

        fn stop(&self) -> Result<WorldSessionState, String> {
            let mut state = self.state.lock().unwrap();
            state.revision += 1;
            state.lifecycle = WorldLifecycle::Stopping;
            let stopped = state.clone();
            let _ = self.states.lock().unwrap().send(stopped.clone());
            let root = self.paths.evidence_path(WORKFLOW_INSTANCE);
            let summary = WorldTerminalSummary {
                schema: TERMINAL_SUMMARY_SCHEMA.to_owned(),
                instance: state.instance,
                provenance: state.provenance.clone(),
                outcome: TerminalOutcome::Stopped {
                    reason: SimulationEndReason::WorldStopped,
                },
                progress: state.progress,
                members: state.members.clone(),
                member_evidence: Vec::new(),
                failing: TerminalFailure {
                    process: None,
                    producer: None,
                },
                evidence: vec!["host.log".to_owned(), "webots.log".to_owned()],
                cleanup: TerminalCleanup {
                    complete: true,
                    detail: None,
                },
                retention: TerminalRetention {
                    log_byte_limit: 1_024,
                    truncated: Vec::new(),
                },
                ended_at_unix_ms: 123_456,
            };
            write_owner_json(&root.join("summary.json"), &summary)
                .map_err(|error| error.to_string())?;
            fs::remove_file(self.paths.registration_path(WORKFLOW_INSTANCE))
                .map_err(|error| error.to_string())?;
            fs::remove_file(
                self.paths
                    .registry()
                    .join(format!("{WORKFLOW_INSTANCE}.lease")),
            )
            .map_err(|error| error.to_string())?;
            Ok(stopped)
        }
    }

    impl WorldSessionHandler for WorkflowWorld {
        fn bootstrap(&self) -> WorldSessionBootstrap {
            self.bootstrap.clone()
        }

        fn state(&self) -> WorldSessionState {
            self.state.lock().unwrap().clone()
        }

        fn subscribe_state(&self) -> broadcast::Receiver<WorldSessionState> {
            self.states.lock().unwrap().subscribe()
        }

        fn diagnostics(&self) -> WorldSessionDiagnostics {
            *self.diagnostics.lock().unwrap()
        }

        fn subscribe_diagnostics(&self) -> broadcast::Receiver<WorldSessionDiagnostics> {
            self.diagnostic_updates.lock().unwrap().subscribe()
        }

        fn control(&self, request: WorldControl) -> WorldSessionOperation<'_, WorldSessionState> {
            Box::pin(async move {
                match request {
                    WorldControl::Stop => self.stop(),
                    WorldControl::Pause | WorldControl::Resume => Ok(self.state()),
                }
            })
        }

        fn attach(
            &self,
            _execution: ExecutionId,
            _supervisor_endpoint: String,
            _spawn: Option<SpawnId>,
        ) -> WorldSessionOperation<'_, WorldSessionState> {
            Box::pin(async move { Ok(self.state()) })
        }
    }

    #[cfg(unix)]
    fn write_owner_json(path: &Path, value: &impl Serialize) -> std::io::Result<File> {
        use std::os::unix::fs::OpenOptionsExt;

        let mut file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(&serde_json::to_vec(value)?)?;
        Ok(file)
    }

    #[cfg(unix)]
    fn write_owner_bytes(path: &Path, bytes: &[u8]) -> std::io::Result<File> {
        use std::os::unix::fs::OpenOptionsExt;

        let mut file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(bytes)?;
        Ok(file)
    }

    #[cfg(unix)]
    fn create_owner_directory(path: &Path) {
        use std::os::unix::fs::DirBuilderExt;

        fs::DirBuilder::new().mode(0o700).create(path).unwrap();
    }

    #[cfg(unix)]
    fn write_live_registration(
        paths: &WorldPaths,
        endpoint: &str,
    ) -> (LocalWorldRegistration, File) {
        use std::os::fd::AsRawFd;

        let inspector = SystemProcessInspector;
        let pid = std::process::id();
        let process = ProcessIdentity {
            pid,
            started_at_unix_s: inspector.started_at_unix_s(pid).unwrap(),
        };
        let instance = WorldInstanceId::parse(WORKFLOW_INSTANCE).unwrap();
        let registration = LocalWorldRegistration {
            schema: REGISTRATION_SCHEMA.to_owned(),
            instance,
            endpoint: endpoint.to_owned(),
            process,
            framework: FrameworkVersion::CURRENT,
            world: RegisteredWorld {
                id: WorldId::new("warehouse").unwrap(),
                digest: WorldDigest::parse(&"c".repeat(64)).unwrap(),
            },
            lease: format!("{WORKFLOW_INSTANCE}.lease"),
        };
        let lease = write_owner_bytes(
            &paths.registry().join(format!("{WORKFLOW_INSTANCE}.lease")),
            b"",
        )
        .unwrap();
        // SAFETY: `lease` owns a valid descriptor for the fixture's lifetime.
        assert_eq!(unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX) }, 0);
        write_owner_json(&paths.registration_path(WORKFLOW_INSTANCE), &registration).unwrap();
        (registration, lease)
    }

    #[cfg(unix)]
    async fn workflow_fixture() -> (
        tempfile::TempDir,
        Stores,
        Arc<WorkflowWorld>,
        WorldSessionServer,
        LocalWorldRegistration,
        File,
    ) {
        let temporary = tempfile::tempdir().unwrap();
        let paths = WorldPaths::create(
            temporary.path().join("registry"),
            temporary.path().join("evidence"),
        )
        .unwrap();
        let evidence = paths.evidence_path(WORKFLOW_INSTANCE);
        create_owner_directory(&evidence);
        write_owner_bytes(&evidence.join("host.log"), b"host retained\n").unwrap();
        write_owner_bytes(&evidence.join("webots.log"), b"webots retained\n").unwrap();
        let handler = Arc::new(WorkflowWorld::new(paths.clone()));
        let server = WorldSessionServer::bind(Arc::clone(&handler))
            .await
            .unwrap();
        let (registration, lease) = write_live_registration(&paths, server.endpoint());
        (
            temporary,
            Stores::at(paths),
            handler,
            server,
            registration,
            lease,
        )
    }

    fn registration() -> LocalWorldRegistration {
        let instance =
            WorldInstanceId::parse("1234567890abcdef1234567890abcdef").expect("world instance");
        LocalWorldRegistration {
            schema: REGISTRATION_SCHEMA.to_owned(),
            instance,
            endpoint: "tcp://127.0.0.1:12345".to_owned(),
            process: ProcessIdentity {
                pid: 42,
                started_at_unix_s: 100,
            },
            framework: FrameworkVersion::new(0, 68, 2),
            world: RegisteredWorld {
                id: WorldId::new("warehouse").expect("world id"),
                digest: WorldDigest::parse(&"a".repeat(64)).expect("world digest"),
            },
            lease: format!("{instance}.lease"),
        }
    }

    fn bootstrap(registration: &LocalWorldRegistration) -> WorldSessionBootstrap {
        WorldSessionBootstrap {
            instance: registration.instance,
            framework: registration.framework,
            world: registration.world.id.clone(),
            digest: registration.world.digest,
        }
    }

    fn state(lifecycle: WorldLifecycle) -> WorldSessionState {
        WorldSessionState {
            revision: 1,
            instance: WorldInstanceId::parse("1234567890abcdef1234567890abcdef").unwrap(),
            provenance: WorldProvenance {
                world: WorldId::new("warehouse").unwrap(),
                digest: WorldDigest::parse(&"a".repeat(64)).unwrap(),
                random_seed: 7,
                framework: FrameworkVersion::new(0, 68, 2),
                adapter: "webots".to_owned(),
                adapter_version: "R2025a".to_owned(),
                simulator_version: "R2025a".to_owned(),
                platform: "test".to_owned(),
                time_step_ns: 10_000_000,
            },
            lifecycle,
            progress: WorldProgress::at(0, 10_000_000).unwrap(),
            members: Vec::new(),
        }
    }

    #[test]
    fn live_status_is_a_complete_pure_projection_of_the_returned_state() {
        let rendered = format_live_status(&state(WorldLifecycle::Ready {
            motion: WorldMotion::Paused,
        }));
        for expected in [
            "instance:  1234567890abcdef1234567890abcdef",
            "world:     warehouse",
            "lifecycle: ready/paused",
            "step:      0",
            "members:   0",
        ] {
            assert!(rendered.contains(expected), "{rendered}");
        }
    }

    fn member_ending(
        reason: WorldMemberEndReason,
        cleanup: WorldMemberCleanup,
    ) -> ConnectedSimulationEnding {
        ConnectedSimulationEnding::Member(WorldMemberEvidence {
            schema: phoxal_cli_host::world::MEMBER_TERMINAL_SCHEMA.to_owned(),
            terminal: phoxal::world::api::session::WorldMemberTerminal {
                execution: ExecutionId::parse("1234567890abcdef1234567890abcdef").unwrap(),
                robot: RobotId::new("rover").unwrap(),
                controller: ProducerId::parse("2234567890abcdef1234567890abcdef").unwrap(),
                spawn: SpawnId::new("loading-bay").unwrap(),
                reason,
                last_progress: WorldProgress::at(4, 10_000_000).unwrap(),
                cleanup,
                evidence_paths: Vec::new(),
            },
        })
    }

    #[test]
    fn same_line_patches_pass_preflight_in_both_directions() {
        assert!(
            ensure_compatible_train(
                FrameworkVersion::new(0, 68, 2),
                FrameworkVersion::new(0, 68, 0)
            )
            .is_ok()
        );
        assert!(
            ensure_compatible_train(
                FrameworkVersion::new(1, 4, 0),
                FrameworkVersion::new(1, 9, 7)
            )
            .is_ok()
        );
    }

    #[test]
    fn adjacent_line_is_refused_with_both_versions_and_required_line() {
        let error = ensure_compatible_train(
            FrameworkVersion::new(0, 68, 2),
            FrameworkVersion::new(0, 69, 0),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("0.68.2"), "{error}");
        assert!(error.contains("0.69.0"), "{error}");
        assert!(error.contains("0.68.x"), "{error}");
        assert!(error.contains("before any build or launch"), "{error}");
    }

    #[test]
    fn live_endpoint_bootstrap_must_match_every_locator_identity() {
        let registration = registration();
        assert!(
            ensure_bootstrap_matches_registration(&bootstrap(&registration), &registration).is_ok()
        );

        let mut wrong_instance = bootstrap(&registration);
        wrong_instance.instance =
            WorldInstanceId::parse("2234567890abcdef1234567890abcdef").unwrap();
        let error = ensure_bootstrap_matches_registration(&wrong_instance, &registration)
            .unwrap_err()
            .to_string();
        assert!(error.contains("mismatched locator"), "{error}");

        let mut wrong_provenance = bootstrap(&registration);
        wrong_provenance.digest = WorldDigest::parse(&"b".repeat(64)).unwrap();
        let error = ensure_bootstrap_matches_registration(&wrong_provenance, &registration)
            .unwrap_err()
            .to_string();
        assert!(error.contains("registered digest"), "{error}");
    }

    #[test]
    fn launch_commit_requires_authoritative_ready_and_paused_state() {
        assert!(
            ensure_ready_and_paused(&state(WorldLifecycle::Ready {
                motion: WorldMotion::Paused,
            }))
            .is_ok()
        );
        for lifecycle in [
            WorldLifecycle::Starting,
            WorldLifecycle::Ready {
                motion: WorldMotion::Running,
            },
            WorldLifecycle::Stopping,
        ] {
            let error = ensure_ready_and_paused(&state(lifecycle))
                .unwrap_err()
                .to_string();
            assert!(error.contains("authoritative lifecycle"), "{error}");
        }
    }

    #[test]
    fn typed_member_outcomes_distinguish_clean_stop_from_world_failure() {
        let (stopped, failed) = connected_simulation_ending_description(&member_ending(
            WorldMemberEndReason::Stopped,
            WorldMemberCleanup::Complete,
        ));
        assert!(!failed, "{stopped}");
        assert!(stopped.contains("Stopped"), "{stopped}");

        let (fault, failed) = connected_simulation_ending_description(&member_ending(
            WorldMemberEndReason::ControllerFault,
            WorldMemberCleanup::Incomplete {
                detail: "native controller survived".to_owned(),
            },
        ));
        assert!(failed, "{fault}");
        assert!(fault.contains("ControllerFault"), "{fault}");
        assert!(fault.contains("native controller survived"), "{fault}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn world_stop_member_evidence_waits_for_world_terminal_summary() {
        let (_temporary, stores, handler, server, registration, _lease) = workflow_fixture().await;
        let client = connect_verified(&registration).await.unwrap();
        let ConnectedSimulationEnding::Member(member) =
            member_ending(WorldMemberEndReason::Stopped, WorldMemberCleanup::Complete)
        else {
            unreachable!();
        };
        let members = handler
            .paths
            .evidence_path(WORKFLOW_INSTANCE)
            .join("members");
        create_owner_directory(&members);
        write_owner_json(
            &members.join(format!("{}.json", member.terminal.execution)),
            &member,
        )
        .unwrap();
        handler.state.lock().unwrap().lifecycle = WorldLifecycle::Stopping;
        let ending = await_connected_simulation_ending(
            &stores,
            &registration,
            &client,
            member.terminal.execution,
        );
        tokio::pin!(ending);
        assert!(
            tokio::time::timeout(Duration::from_millis(250), &mut ending)
                .await
                .is_err(),
            "member evidence during world stop must not be reported as an independently live world"
        );
        handler.stop().unwrap();
        assert!(matches!(
            ending.await.unwrap(),
            ConnectedSimulationEnding::World(_)
        ));
        server.close().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn member_only_stop_resolves_while_world_is_ready() {
        let (_temporary, stores, handler, server, registration, _lease) = workflow_fixture().await;
        let client = connect_verified(&registration).await.unwrap();
        let ConnectedSimulationEnding::Member(member) =
            member_ending(WorldMemberEndReason::Stopped, WorldMemberCleanup::Complete)
        else {
            unreachable!();
        };
        let members = handler
            .paths
            .evidence_path(WORKFLOW_INSTANCE)
            .join("members");
        create_owner_directory(&members);
        write_owner_json(
            &members.join(format!("{}.json", member.terminal.execution)),
            &member,
        )
        .unwrap();
        let ending = tokio::time::timeout(
            Duration::from_secs(3),
            await_connected_simulation_ending(
                &stores,
                &registration,
                &client,
                member.terminal.execution,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(ending, ConnectedSimulationEnding::Member(_)));
        server.close().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_workflow_reads_live_and_terminal_state_logs_and_list_scope() {
        let (_temporary, stores, _handler, server, registration, _lease) = workflow_fixture().await;

        let StatusReport::Live(live) = load_status(&stores, WORKFLOW_INSTANCE).await.unwrap()
        else {
            panic!("a held registration must resolve as live");
        };
        assert_eq!(live.instance, registration.instance);
        assert_eq!(
            live.lifecycle,
            WorldLifecycle::Ready {
                motion: WorldMotion::Paused
            }
        );
        assert_eq!(
            load_logs(&stores, WORKFLOW_INSTANCE).await.unwrap(),
            vec![
                ("host.log".to_owned(), b"host retained\n".to_vec()),
                ("webots.log".to_owned(), b"webots retained\n".to_vec()),
            ]
        );

        let live_only = load_list(&stores, false).await.unwrap();
        assert_eq!(live_only.live.len(), 1);
        assert!(live_only.terminal.is_empty());
        let live_and_terminal = load_list(&stores, true).await.unwrap();
        assert_eq!(live_and_terminal.live.len(), 1);
        assert!(live_and_terminal.terminal.is_empty());

        let stopped = stop_world(registration, &stores).await.unwrap();
        assert_eq!(stopped.outcome.reason(), SimulationEndReason::WorldStopped);
        let StatusReport::Terminal { summary, members } =
            load_status(&stores, WORKFLOW_INSTANCE).await.unwrap()
        else {
            panic!("a stopped world must resolve from retained evidence");
        };
        assert_eq!(*summary, stopped);
        assert!(members.is_empty());
        assert_eq!(
            load_logs(&stores, WORKFLOW_INSTANCE).await.unwrap(),
            vec![
                ("host.log".to_owned(), b"host retained\n".to_vec()),
                ("webots.log".to_owned(), b"webots retained\n".to_vec()),
            ]
        );

        let live_only = load_list(&stores, false).await.unwrap();
        assert!(live_only.live.is_empty());
        assert!(live_only.terminal.is_empty());
        let all = load_list(&stores, true).await.unwrap();
        assert!(all.live.is_empty());
        assert_eq!(all.terminal, vec![stopped]);

        server.close().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn monitor_feeds_reconnect_state_and_diagnostics_after_stream_loss() {
        let (_temporary, _stores, handler, server, registration, _lease) = workflow_fixture().await;
        let client = connect_verified(&registration).await.unwrap();

        let states = client.state_subscription().await.unwrap();
        let (state_tx, mut state_rx) = tokio::sync::mpsc::channel(4);
        let mut state_tasks = tokio::task::JoinSet::new();
        spawn_state_feed(
            &mut state_tasks,
            client.clone(),
            states,
            registration,
            state_tx,
        );
        handler.rotate_state_stream();
        let state = tokio::time::timeout(Duration::from_secs(3), state_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let phoxal_cli_ui::WorldInput::State(state) = state else {
            panic!("state loss must reconnect with a fresh authoritative snapshot");
        };
        assert_eq!(state.instance.to_string(), WORKFLOW_INSTANCE);
        state_tasks.shutdown().await;

        let diagnostics = client.diagnostics_subscription().await.unwrap();
        let (diagnostics_tx, mut diagnostics_rx) = tokio::sync::mpsc::channel(4);
        let mut diagnostics_tasks = tokio::task::JoinSet::new();
        spawn_diagnostics_feed(&mut diagnostics_tasks, client, diagnostics, diagnostics_tx);
        handler.rotate_diagnostics_stream();
        let diagnostics = tokio::time::timeout(Duration::from_secs(3), diagnostics_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let phoxal_cli_ui::WorldInput::Diagnostics(diagnostics) = diagnostics else {
            panic!("diagnostics loss must reconnect with a fresh bounded snapshot");
        };
        assert_eq!(diagnostics.revision, 2);
        diagnostics_tasks.shutdown().await;

        server.close().await.unwrap();
    }
}
