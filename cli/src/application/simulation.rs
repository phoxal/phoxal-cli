//! Backend-neutral local world lifecycle and robot connection workflow.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use phoxal::model::identity::SpawnId;
use phoxal::session::WorldSessionClient;
use phoxal::version::FrameworkVersion;
use phoxal::world::api::session::connect::WorldSessionBootstrap;
use phoxal::world::api::session::control::WorldSessionControlRequest;
use phoxal::world::api::session::state::WorldSessionState;
use phoxal::world::api::session::{
    WorldLifecycle, WorldMemberCleanup, WorldMemberEndReason, WorldMemberPhase, WorldMotion,
};
use phoxal_cli_host::world::{
    DEFAULT_LOG_BYTE_LIMIT, DEFAULT_TERMINAL_SESSION_LIMIT, LocalWorldRegistration,
    TerminalMemberEvidence, TerminalOutcome, TerminalWorldSummary, WorldEvidence, WorldPaths,
    WorldRegistry,
};

use crate::cli::context::AppContext;
use crate::cli::exit::ReportedExit;
use crate::cli::output::welcome::{Mode, StepId};

use super::summary::SessionSummary;
use phoxal_cli_host::world_process::LaunchedWorldHost;

const WORLD_UI_INGRESS_CAPACITY: usize = 64;
const ATTACHMENT_BUDGET: Duration = Duration::from_secs(5 * 60);
const STOP_BUDGET: Duration = Duration::from_secs(60);
const TERMINAL_POLL_INTERVAL: Duration = Duration::from_millis(100);
const STREAM_RECONNECT_DELAY: Duration = Duration::from_millis(100);

struct StartedWorld {
    registration: LocalWorldRegistration,
    host: LaunchedWorldHost,
}

/// Inputs resolved before a `simulation run` creates its transaction-owned
/// world process.
struct ConnectionIntent {
    spawn: Option<SpawnId>,
    target: super::lifecycle::Target,
}

enum ConnectedSimulationEnding {
    World(Box<TerminalWorldSummary>),
    Member(TerminalMemberEvidence),
}

pub(crate) async fn run_command(app: &AppContext, world: &Path, spawn: Option<&str>) -> Result<()> {
    let intent = connection_intent(app, spawn)?;
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
) -> Result<()> {
    connect_world(app, instance, connection_intent(app, spawn)?, None).await
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

enum StatusReport {
    Live(Box<WorldSessionState>),
    Terminal {
        summary: Box<TerminalWorldSummary>,
        members: Vec<TerminalMemberEvidence>,
    },
}

async fn load_status(stores: &Stores, instance: &str) -> Result<StatusReport> {
    if let Some(registration) = stores.registry.find(instance)? {
        let (_, state) = current_verified(&registration).await?;
        return Ok(StatusReport::Live(Box::new(state)));
    }
    let summary = stores.recover_terminal(instance).await?.with_context(|| {
        format!(
            "no live or retained terminal world session `{instance}` was found; `phoxal simulation list --all` shows discoverable sessions"
        )
    })?;
    let members = stores.evidence.read_member_evidence(&summary)?;
    Ok(StatusReport::Terminal {
        summary: Box::new(summary),
        members,
    })
}

async fn load_logs(stores: &Stores, instance: &str) -> Result<Vec<(String, Vec<u8>)>> {
    if let Some(registration) = stores.registry.find(instance)? {
        connect_verified(&registration).await?;
        return stores.evidence.read_live_logs(instance);
    }
    let summary = stores.recover_terminal(instance).await?.with_context(|| {
        format!("no live or retained terminal world session `{instance}` was found")
    })?;
    stores.evidence.read_logs(&summary)
}

struct ListReport {
    live: Vec<LocalWorldRegistration>,
    terminal: Vec<TerminalWorldSummary>,
}

async fn load_list(stores: &Stores, all: bool) -> Result<ListReport> {
    let discoverable = stores.registry.registration_instances()?;
    let registered = stores.registry.list()?;
    let registered_ids = registered
        .iter()
        .map(|registration| registration.instance.to_string())
        .collect::<BTreeSet<_>>();
    if all {
        for instance in discoverable {
            if registered_ids.contains(&instance) {
                continue;
            }
            if let Err(error) = stores.recover_terminal(&instance).await {
                eprintln!(
                    "warning: stale world {instance} could not be finalized from durable evidence: {error:#}"
                );
            }
        }
    }
    let report = stores
        .evidence
        .prune(DEFAULT_TERMINAL_SESSION_LIMIT, &registered_ids)?;
    for path in report.incomplete {
        tracing::warn!(path = %path.display(), "incomplete world evidence was retained");
    }
    let mut live = Vec::new();
    for registration in registered {
        match connect_verified(&registration).await {
            Ok(_) => live.push(registration),
            Err(error) => eprintln!(
                "warning: world {} has a live local lease but its frozen bootstrap could not be verified: {error:#}",
                registration.instance
            ),
        }
    }
    let terminal = if all {
        stores
            .evidence
            .list_summaries()?
            .into_iter()
            .filter(|summary| !registered_ids.contains(&summary.instance.to_string()))
            .collect()
    } else {
        Vec::new()
    };
    Ok(ListReport { live, terminal })
}

async fn start_world(app: &AppContext, source: &Path) -> Result<StartedWorld> {
    let stores = Stores::discover()?;
    stores.prune()?;

    let staging = tempfile::Builder::new()
        .prefix(".world-launch-")
        .tempdir()
        .context("failed to create a world-bundle launch staging directory")?;
    let bundle_path = staging.path().join("world-bundle");
    let source = source.to_path_buf();
    let destination = bundle_path.clone();
    app.ui.info(format!("compiling world {}", source.display()));
    let compiled = tokio::task::spawn_blocking(move || {
        phoxal_cli_project::compile_world(&source, &destination)
    })
    .await
    .context("world compiler worker failed")??;

    let expected_world = compiled.bundle().world().id().clone();
    let expected_digest = compiled.digest();
    let framework = FrameworkVersion::CURRENT;
    let offline = app.offline;
    app.ui
        .info(format!("materializing simulation host train {framework}"));
    let tools = tokio::task::spawn_blocking(move || {
        phoxal_cli_project::materialize_webots_tools(
            framework,
            offline,
            &phoxal_cli_project::SilentReporter,
        )
    })
    .await
    .context("simulation host materializer worker failed")??;

    let (instance, host) = phoxal_cli_host::world_process::launch(
        tools.host(),
        compiled.path(),
        &stores.paths,
        DEFAULT_LOG_BYTE_LIMIT,
    )
    .await?;
    let registration = match stores.registry.resolve(&instance) {
        Ok(registration) => registration,
        Err(error) => return Err(rollback_host(host, error).await),
    };
    let validation = || -> Result<()> {
        ensure!(
            registration.framework == framework,
            "new world registered framework {}, expected exact host train {framework}",
            registration.framework
        );
        ensure!(
            registration.world.id == expected_world,
            "new world registered ID {}, compiled source was {}",
            registration.world.id,
            expected_world
        );
        ensure!(
            registration.world.digest == expected_digest,
            "new world registered digest {}, compiled bundle was {expected_digest}",
            registration.world.digest
        );
        Ok(())
    };
    if let Err(error) = validation() {
        return Err(rollback_host(host, error).await);
    }
    let ready = match current_verified(&registration).await {
        Ok((_, state)) => ensure_ready_and_paused(&state),
        Err(error) => Err(error),
    };
    if let Err(error) = ready {
        return Err(rollback_host(host, error).await);
    }
    drop(staging);
    Ok(StartedWorld { registration, host })
}

async fn rollback_host(host: LaunchedWorldHost, error: anyhow::Error) -> anyhow::Error {
    match host.stop().await {
        Ok(()) => error,
        Err(cleanup) => error.context(format!("world rollback also failed: {cleanup:#}")),
    }
}

fn connection_intent(app: &AppContext, spawn: Option<&str>) -> Result<ConnectionIntent> {
    let spawn = spawn
        .map(SpawnId::new)
        .transpose()
        .context("invalid world spawn name")?;
    let target = super::lifecycle::Target::resolve(None, app.project.root())?;
    Ok(ConnectionIntent { spawn, target })
}

async fn connect_world(
    app: &AppContext,
    instance: &str,
    intent: ConnectionIntent,
    mut started: Option<StartedWorld>,
) -> Result<()> {
    let stores = Stores::discover()?;
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
    let ConnectionIntent { spawn, target } = intent;
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

fn report_world_commands(app: &AppContext, instance: phoxal::model::world::WorldInstanceId) {
    app.ui.info(format!(
        "inspect the independent world with `phoxal simulation status {instance}`; while live, use `phoxal simulation open {instance}` or `phoxal simulation stop {instance}`"
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

async fn open_world_tui(app: &AppContext, registration: LocalWorldRegistration) -> Result<()> {
    let client = connect_verified(&registration).await?;
    let states = client
        .state_subscription()
        .await
        .context("failed to open world state subscription")?;
    let diagnostics = match client.diagnostics_subscription().await {
        Ok(diagnostics) => Some(diagnostics),
        Err(error) => {
            app.ui.warn(format!(
                "world diagnostics are unavailable, but authoritative state remains connected: {error}"
            ));
            None
        }
    };
    let initial_state = states.current().clone();
    let initial_diagnostics = diagnostics
        .as_ref()
        .map(|diagnostics| diagnostics.current());
    ensure_state_matches_registration(&initial_state, &registration)?;

    let (ingress_tx, ingress_rx) = tokio::sync::mpsc::channel(WORLD_UI_INGRESS_CAPACITY);
    let (controls_tx, controls_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut tasks = tokio::task::JoinSet::new();
    spawn_state_feed(
        &mut tasks,
        client.clone(),
        states,
        registration.clone(),
        ingress_tx.clone(),
    );
    if let Some(diagnostics) = diagnostics {
        spawn_diagnostics_feed(&mut tasks, client.clone(), diagnostics, ingress_tx.clone());
    }
    spawn_control_router(
        &mut tasks,
        client,
        registration.clone(),
        controls_rx,
        ingress_tx.clone(),
    );
    drop(ingress_tx);

    let outcome = phoxal_cli_ui::run_world(
        ingress_rx,
        controls_tx,
        phoxal_cli_ui::WorldUiOptions {
            title: "phoxal simulation",
            theme: app.output.theme,
        },
        initial_state,
        initial_diagnostics,
    )
    .await;
    tasks.shutdown().await;
    match outcome? {
        phoxal_cli_ui::WorldOutcome::Detached => app.ui.info(format!(
            "detached from world {}; inspect it with `phoxal simulation status {}`",
            registration.instance, registration.instance
        )),
        phoxal_cli_ui::WorldOutcome::Stopped => app
            .ui
            .success(format!("world {} stopped", registration.instance)),
        phoxal_cli_ui::WorldOutcome::Ended { reason } => {
            bail!(
                "world {} ended{}",
                registration.instance,
                reason.map_or_else(String::new, |reason| format!(": {reason}"))
            );
        }
    }
    Ok(())
}

fn spawn_state_feed(
    tasks: &mut tokio::task::JoinSet<()>,
    client: WorldSessionClient,
    mut states: phoxal::session::WorldStateSubscription,
    registration: LocalWorldRegistration,
    ingress: tokio::sync::mpsc::Sender<phoxal_cli_ui::WorldInput>,
) {
    tasks.spawn(async move {
        loop {
            match states.recv().await {
                Ok(state) => {
                    if let Err(error) = ensure_state_matches_registration(state, &registration) {
                        let _ = ingress
                            .send(phoxal_cli_ui::WorldInput::Disconnected {
                                reason: Some(error.to_string()),
                            })
                            .await;
                        return;
                    }
                    if ingress
                        .send(phoxal_cli_ui::WorldInput::State(state.clone()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Err(error) => {
                    match reconnect_state_subscription(&client, &registration, &ingress).await {
                        Ok(reconnected) => states = reconnected,
                        Err(reconnect) => {
                            let _ = ingress
                                .send(phoxal_cli_ui::WorldInput::Disconnected {
                                    reason: Some(format!(
                                        "world state stream ended: {error}; reconnect failed: {reconnect:#}"
                                    )),
                                })
                                .await;
                            return;
                        }
                    }
                }
            }
        }
    });
}

async fn reconnect_state_subscription(
    client: &WorldSessionClient,
    registration: &LocalWorldRegistration,
    ingress: &tokio::sync::mpsc::Sender<phoxal_cli_ui::WorldInput>,
) -> Result<phoxal::session::WorldStateSubscription> {
    tokio::time::sleep(STREAM_RECONNECT_DELAY).await;
    let states = client
        .state_subscription()
        .await
        .context("failed to reopen the world state subscription")?;
    let current = states.current().clone();
    ensure_state_matches_registration(&current, registration)?;
    ingress
        .send(phoxal_cli_ui::WorldInput::State(current))
        .await
        .context("world UI closed during state-stream recovery")?;
    Ok(states)
}

fn spawn_diagnostics_feed(
    tasks: &mut tokio::task::JoinSet<()>,
    client: WorldSessionClient,
    mut diagnostics: phoxal::session::WorldDiagnosticsSubscription,
    ingress: tokio::sync::mpsc::Sender<phoxal_cli_ui::WorldInput>,
) {
    tasks.spawn(async move {
        loop {
            match diagnostics.recv().await {
                Ok(diagnostics) => {
                    if ingress
                        .send(phoxal_cli_ui::WorldInput::Diagnostics(diagnostics))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Err(error) => {
                    match reconnect_diagnostics_subscription(&client, &ingress).await {
                        Ok(reconnected) => diagnostics = reconnected,
                        Err(reconnect) => {
                            let _ = ingress
                                .send(phoxal_cli_ui::WorldInput::DiagnosticsUnavailable {
                                    reason: format!(
                                        "diagnostics stream ended: {error}; reconnect failed: {reconnect:#}"
                                    ),
                                })
                                .await;
                            return;
                        }
                    }
                }
            }
        }
    });
}

async fn reconnect_diagnostics_subscription(
    client: &WorldSessionClient,
    ingress: &tokio::sync::mpsc::Sender<phoxal_cli_ui::WorldInput>,
) -> Result<phoxal::session::WorldDiagnosticsSubscription> {
    tokio::time::sleep(STREAM_RECONNECT_DELAY).await;
    let diagnostics = client
        .diagnostics_subscription()
        .await
        .context("failed to reopen the world diagnostics subscription")?;
    ingress
        .send(phoxal_cli_ui::WorldInput::Diagnostics(
            diagnostics.current(),
        ))
        .await
        .context("world UI closed during diagnostics-stream recovery")?;
    Ok(diagnostics)
}

fn spawn_control_router(
    tasks: &mut tokio::task::JoinSet<()>,
    client: WorldSessionClient,
    registration: LocalWorldRegistration,
    mut controls: tokio::sync::mpsc::UnboundedReceiver<WorldSessionControlRequest>,
    ingress: tokio::sync::mpsc::Sender<phoxal_cli_ui::WorldInput>,
) {
    tasks.spawn(async move {
        while let Some(request) = controls.recv().await {
            let input = match client.control(request).await {
                Ok(state) => match ensure_state_matches_registration(&state, &registration) {
                    Ok(()) => phoxal_cli_ui::WorldInput::State(state),
                    Err(error) => phoxal_cli_ui::WorldInput::Disconnected {
                        reason: Some(error.to_string()),
                    },
                },
                Err(error) => phoxal_cli_ui::WorldInput::ControlFailed {
                    request,
                    reason: error.to_string(),
                },
            };
            if ingress.send(input).await.is_err() {
                return;
            }
        }
    });
}

fn print_live_status(state: &WorldSessionState) {
    println!("instance:  {}", state.instance);
    println!("world:     {}", state.provenance.world);
    println!("digest:    {}", state.provenance.digest);
    println!("lifecycle: {}", lifecycle_text(state.lifecycle));
    println!("train:     {}", state.provenance.framework);
    println!(
        "adapter:   {} {}",
        state.provenance.adapter, state.provenance.adapter_version
    );
    println!("simulator: {}", state.provenance.simulator_version);
    println!("step:      {}", state.progress.completed_step());
    println!("world ns:  {}", state.progress.elapsed_ns());
    println!("members:   {}", state.members.len());
    for member in &state.members {
        println!(
            "  {}  {:?}  {}",
            member.robot, member.phase, member.execution
        );
    }
}

fn ensure_state_matches_registration(
    state: &WorldSessionState,
    registration: &LocalWorldRegistration,
) -> Result<()> {
    ensure!(
        state.instance == registration.instance,
        "world state instance {} disagrees with locator {}",
        state.instance,
        registration.instance
    );
    ensure!(
        state.provenance.framework == registration.framework
            && state.provenance.world == registration.world.id
            && state.provenance.digest == registration.world.digest,
        "world state provenance disagrees with the verified locator for {}",
        registration.instance
    );
    Ok(())
}

fn lifecycle_text(lifecycle: WorldLifecycle) -> String {
    match lifecycle {
        WorldLifecycle::Starting => "starting".to_owned(),
        WorldLifecycle::Ready { motion } => format!("ready/{motion:?}").to_lowercase(),
        WorldLifecycle::Stopping => "stopping".to_owned(),
        WorldLifecycle::Failed { reason } => format!("failed/{reason:?}").to_lowercase(),
    }
}

async fn stop_world(
    registration: LocalWorldRegistration,
    stores: &Stores,
) -> Result<TerminalWorldSummary> {
    let client = connect_verified(&registration).await?;
    let state = client
        .control(WorldSessionControlRequest::Stop)
        .await
        .context("world host refused stop")?;
    ensure_state_matches_registration(&state, &registration)?;

    let instance = registration.instance.to_string();
    let summary = tokio::time::timeout(STOP_BUDGET, async {
        loop {
            if stores.registry.find(&instance)?.is_none() {
                if let Some(summary) = stores.evidence.read_summary(&instance)? {
                    return Ok::<_, anyhow::Error>(summary);
                }
                if let Some(summary) = stores.recover_terminal(&instance).await? {
                    return Ok::<_, anyhow::Error>(summary);
                }
            }
            tokio::time::sleep(TERMINAL_POLL_INTERVAL).await;
        }
    })
    .await
    .with_context(|| {
        format!(
            "timed out after {}s waiting for world {instance} to persist terminal evidence",
            STOP_BUDGET.as_secs()
        )
    })??;
    if matches!(summary.outcome, TerminalOutcome::Failed { .. }) {
        bail!(
            "world {instance} ended as {}/{:?} while stopping{}",
            summary.outcome.kind(),
            summary.outcome.reason(),
            summary
                .outcome
                .detail()
                .map_or_else(String::new, |detail| format!(": {detail}"))
        );
    }
    Ok(summary)
}

fn print_terminal_status(summary: &TerminalWorldSummary, ended: &[TerminalMemberEvidence]) {
    println!("instance:  {}", summary.instance);
    println!("world:     {}", summary.provenance.world);
    println!("digest:    {}", summary.provenance.digest);
    println!("lifecycle: {}", summary.outcome.kind());
    println!("reason:    {:?}", summary.outcome.reason());
    if let Some(detail) = summary.outcome.detail() {
        println!("detail:    {detail}");
    }
    println!("train:     {}", summary.provenance.framework);
    println!(
        "adapter:   {} {}",
        summary.provenance.adapter, summary.provenance.adapter_version
    );
    println!("simulator: {}", summary.provenance.simulator_version);
    println!("platform:  {}", summary.provenance.platform);
    println!("seed:      {}", summary.provenance.random_seed);
    println!("quantum:   {} ns", summary.provenance.time_step_ns);
    println!("step:      {}", summary.progress.completed_step());
    println!("world ns:  {}", summary.progress.elapsed_ns());
    println!(
        "members:   {} at shutdown, {} ended",
        summary.members.len(),
        ended.len()
    );
    for member in &summary.members {
        println!(
            "  {}  at-shutdown/{:?}  {}",
            member.robot, member.phase, member.execution
        );
    }
    for member in ended {
        println!(
            "  {}  ended/{:?}  {}  cleanup/{:?}",
            member.terminal.robot,
            member.terminal.reason,
            member.terminal.execution,
            member.terminal.cleanup
        );
    }
    if let Some(process) = summary.failing.process {
        println!(
            "failure:   process {} born {}",
            process.pid, process.started_at_unix_s
        );
    }
    if let Some(producer) = summary.failing.producer {
        println!("failure:   producer {producer}");
    }
    println!(
        "cleanup:   {}{}",
        if summary.cleanup.complete {
            "complete"
        } else {
            "incomplete"
        },
        summary
            .cleanup
            .detail
            .as_deref()
            .map_or_else(String::new, |detail| format!(" ({detail})"))
    );
    if !summary.retention.truncated.is_empty() {
        println!("truncated: {}", summary.retention.truncated.join(", "));
    }
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

    async fn recover_terminal(&self, instance: &str) -> Result<Option<TerminalWorldSummary>> {
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
            let summary = TerminalWorldSummary {
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

        fn control(
            &self,
            request: WorldSessionControlRequest,
        ) -> WorldSessionOperation<'_, WorldSessionState> {
            Box::pin(async move {
                match request {
                    WorldSessionControlRequest::Stop => self.stop(),
                    WorldSessionControlRequest::Pause | WorldSessionControlRequest::Resume => {
                        Ok(self.state())
                    }
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

    fn member_ending(
        reason: WorldMemberEndReason,
        cleanup: WorldMemberCleanup,
    ) -> ConnectedSimulationEnding {
        ConnectedSimulationEnding::Member(TerminalMemberEvidence {
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
