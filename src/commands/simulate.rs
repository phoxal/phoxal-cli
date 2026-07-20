use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand};
use phoxal::bus::{Codec, ContractBody, MessagePack, QueryFailure};
use phoxal::check as graph_check;
use phoxal::raw::{Bus, BusConfig};
use phoxal_api::v2::simulation::{RobotSpawn, SpawnRequest, SpawnSet};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::AppContext;
use crate::catalog::Catalog;
use crate::commands::check::{
    CheckGraphContext, SourceParticipant, SourceParticipantKind, build_emit_apis_from_source,
    check_artifact_refs_from_resolved, extract_emit_apis_from_staged_runtime,
    extract_emit_apis_from_staged_tool, fetch_emit_apis_from_tool, run_check_with_context,
    source_participants_from_resolved, tool_participants_from_resolved,
};
use crate::component_driver::{component_assets_dir, component_driver_crate_dir};
use crate::launch_plan::{
    CheckedRobotLaunchInput, DEFAULT_ROUTER_CONNECT, LaunchMode, LaunchPlan, PlanContext,
    STANDARD_SITE_TOOLS, SubstitutedContract, SubstitutionRecord, build_launch_plan,
};
use crate::resolver::{
    ResolveOptions, ResolvedPlatformRuntime, ResolvedRobot, RobotManifestExtras, resolve,
};
use crate::session::output::OutputContext;
use crate::simulate_staging::{
    ComponentTypeToStage, RobotToStage, StagedSimulationWorld, stage_simulation_world,
};
use crate::supervisor::{
    BoardBackend, ParticipantSpec, ParticipantState, ParticipantStatus, RequestedStop,
    SupervisionStage, SupervisorAction, SupervisorLock, SupervisorOptions,
    start_bus_log_subscriber, start_clock_feed, start_presence_heartbeat_subscriber,
    wait_for_endpoint,
};
use crate::webots_stage_root;
use phoxal_cli_core::session::ParticipantKind;
use phoxal_cli_core::simulation::world;

/// The world-scoped participant id for the Webots supervisor artifact
/// (`phoxal-simulator-webots-supervisor`). One supervisor exists per
/// world/session; it is the simulation world authority (clock, control,
/// robot_pose, contact, and the runtime robot-spawn authority), never a
/// component-driver substitution provider.
pub(crate) const SIMULATOR_SUPERVISOR_PROVIDER_ID: &str = "simulator-webots-supervisor";

/// The artifact name (post `simulator-` prefix strip) of the supervisor
/// artifact, as reported by `resolved.simulators[].name` / `emit-apis`
/// `artifact.id`.
const SIMULATOR_SUPERVISOR_ARTIFACT_NAME: &str = "webots-supervisor";

/// The artifact name (post `simulator-` prefix strip) of the controller
/// artifact, as reported by `resolved.simulators[].name` / `emit-apis`
/// `artifact.id`.
const SIMULATOR_CONTROLLER_ARTIFACT_NAME: &str = "webots-controller";

/// Bound on each non-interactive simulate participant-readiness stage. The
/// simulation clock is telemetry and never participates in this budget.
/// Generous enough to cover a first-run Webots GUI launch plus every
/// participant clearing its own `#[setup]` on a loaded host; a healthy
/// session reaches barrier success in a few seconds in practice.
const SIMULATE_READINESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The robot-scoped participant id for the Webots controller artifact
/// (`phoxal-simulator-webots-controller`) that substitutes `robot_id`'s
/// component-driver contracts. One controller participant exists per robot;
/// this scheme generalizes to a multi-robot plan (each robot gets its own
/// controller id, so N robots keep N distinct substitution providers).
pub(crate) fn simulator_controller_provider_id(robot_id: &str) -> String {
    format!("simulator-webots-controller-{robot_id}")
}

/// The `simulation` command group: `run` (this repo's original `simulate
/// <world>` verb, renamed) and `join` (a multi-robot join stub - see
/// [`SimulationJoin`]). Deliberately a clean cut, no `simulate` alias and no
/// bare `simulation <world>` shorthand - see the group's module docs.
#[derive(Debug, Args)]
pub struct Simulation {
    #[command(subcommand)]
    pub command: SimulationSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum SimulationSubcommand {
    #[command(about = "Resolve and report a Webots simulation launch plan.")]
    Run(SimulationRun),
    #[command(about = "Join a running multi-robot simulation session (not available yet).")]
    Join(SimulationJoin),
}

impl Simulation {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        match &self.command {
            SimulationSubcommand::Run(command) => command.run(app).await,
            SimulationSubcommand::Join(command) => command.run(app).await,
        }
    }
}

#[derive(Debug, Args)]
pub struct SimulationRun {
    #[arg(
        value_name = "WORLD",
        help = "World file or bare name (e.g. `default`, or `worlds/foo.wbt`). Resolved against <project>/worlds/<world>.wbt, then <project>/<world>."
    )]
    pub world: String,
    #[arg(
        long,
        help = "Resolve and write run artifacts without starting simulation processes."
    )]
    pub dry_run: bool,
    #[arg(
        long,
        help = "Watch local source artifacts and hot-reload checked changes."
    )]
    pub watch: bool,
    #[arg(
        long = "env",
        value_name = "ENV",
        help = "Apply a robot.<env>.yaml overlay before simulating (repeatable). Path pins are only legal through overlays."
    )]
    pub env: Vec<String>,
    #[arg(
        long,
        value_name = "TRIPLE",
        help = "Resolve the robot's official artifacts for this target instead of the host (e.g. aarch64, x86_64, or a full triple). The simulator itself still runs on the host. Use it to plan a Linux robot's simulation from a non-Linux host."
    )]
    pub target: Option<String>,
}

/// `phoxal-cli simulation join`: joins a running multi-robot simulation
/// session. Multi-robot join lands as its own slice - this is a stub that
/// prints a clear "not available yet" message and exits 0 (it is not an
/// error to ask for a feature that is on the roadmap but not yet wired up;
/// scripts should not need to special-case this verb's exit status).
#[derive(Debug, Args)]
pub struct SimulationJoin {}

impl SimulationJoin {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        app.ui.info(
            "simulation join: not available yet - multi-robot join lands in a separate slice",
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimulateMode {
    Live,
    DryRun,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SimulateOptions {
    pub world: String,
    pub catalog_source: Option<String>,
    pub watch: bool,
    pub overlays: Vec<String>,
    pub target: Option<String>,
}

/// Pairs the sim `LaunchPlan` with its `PlanContext` (Part 3/6): replaces the
/// old `SimulatePlan` wrapper, which re-declared `resolved`/`project_root`/
/// `source_participants`/`robot_path` fields `PlanContext` now owns, plus a
/// sim-only `world_path` now carried directly by `LaunchMode::Webots` and a
/// `bus_connect` that was always just `DEFAULT_ROUTER_CONNECT`, and a
/// `native_tools` display list now computed from the plan at print time (see
/// `native_tool_labels_from_plan`).
#[derive(Debug, Clone, PartialEq)]
pub struct SimPlan {
    pub plan: LaunchPlan,
    pub ctx: PlanContext,
    /// Finding A5: this session's launch-time participant metadata, resolved
    /// once in `prepare_with_mode` from `plan` and its (sim-filtered)
    /// contract surfaces - see `crate::stores::runtime_store::RuntimeStore`'s
    /// own docs.
    pub runtime_store: crate::stores::runtime_store::RuntimeStore,
}

pub(crate) struct ResolvedSimulation {
    pub(crate) robot_path: PathBuf,
    pub(crate) project_root: PathBuf,
    pub(crate) world_path: PathBuf,
    pub(crate) resolved: ResolvedRobot,
    pub(crate) manifest_extras: RobotManifestExtras,
    pub(crate) catalog: Option<Catalog>,
}

impl SimulationRun {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let options = SimulateOptions {
            world: self.world.clone(),
            catalog_source: app.catalog_source.clone(),
            watch: self.watch,
            overlays: self.env.clone(),
            target: self.target.clone(),
        };
        let mode = if self.dry_run {
            SimulateMode::DryRun
        } else {
            SimulateMode::Live
        };
        run(app, options, mode).await.map(|_| ())
    }
}

pub async fn run(
    app: &AppContext,
    options: SimulateOptions,
    mode: SimulateMode,
) -> Result<SimPlan> {
    match mode {
        SimulateMode::DryRun => {
            let project_root = app.project.root().to_path_buf();
            let sim = tokio::task::spawn_blocking(move || prepare(&project_root, options))
                .await
                .context("simulate dry-run worker failed")??;
            report_plan_only(&sim)?;
            Ok(sim)
        }
        SimulateMode::Live => {
            // One interactive surface for the whole session (Product
            // decision 1): the controller starts its renderer right now,
            // before preparation even begins - see `SessionController::new`.
            let mut controller = crate::session::controller::SessionController::new(
                app.output,
                crate::session::controller::SessionMode::Simulation,
                app.project.root(),
            )?;
            let events = controller.events();

            let project_root = app.project.root().to_path_buf();
            let ui = app.ui;
            let prepared_options = options.clone();
            let sim = controller
                .drive_prepare_phase(move || {
                    prepare_with_mode(&project_root, prepared_options, SimulateMode::Live)
                })
                .await?;

            // Fixes finding A1: preflight, lock acquisition, world/controller
            // staging, and spawn-responder startup used to run as a
            // synchronous gap AFTER preparation but BEFORE the controller's
            // own Ctrl-C-aware loop resumed - in a raw-mode TUI, Ctrl-C is a
            // KEY EVENT (crossterm disables ISIG), which was simply never
            // polled during that whole window. `drive_setup` keeps ONE
            // controller loop live (input, Ctrl-C, session events, redraws)
            // through this setup phase too, exactly like
            // `drive_prepare_phase` already does for preparation.
            let token = controller.token();
            let output = controller.output();
            let renders_tui = controller.renders_tui();
            let setup = live_simulate_setup(
                ui,
                sim.clone(),
                options.clone(),
                events,
                token,
                output,
                renders_tui,
            );
            let setup = controller.drive_setup(setup).await?;

            let LiveSimSetup {
                router,
                connect,
                _locks: locks,
                board,
                telemetry,
                runtime_store,
                orderly_shutdown_timeout,
                stages,
                supervisor_options,
                action_tx,
                background_tasks,
            } = setup;
            controller.set_bus_endpoint(connect);
            controller.set_restart_channel(action_tx);
            let supervise_task =
                tokio::spawn(router.supervise(stages, board.clone(), supervisor_options));

            let outcome = controller
                .drive_supervision(
                    board,
                    telemetry,
                    runtime_store,
                    orderly_shutdown_timeout,
                    supervise_task,
                )
                .await;
            drop(background_tasks);
            drop(locks);
            let outcome = outcome?;
            // Participant failures were already rendered continuously on the
            // board. `drive_supervision` has consumed and torn down the
            // controller, so retain a non-fatal plain-mode summary without
            // converting them into a command error.
            if !outcome.failed_participants.is_empty() {
                app.ui.warn(format!(
                    "simulation stopped with failed participants: {}",
                    outcome.failed_participants.join(", ")
                ));
            }
            Ok(sim)
        }
    }
}

/// Everything [`live_simulate_setup`] hands back to the caller once it
/// completes: the board/telemetry/supervisor task `drive_supervision` needs,
/// plus every ancillary task that must be aborted once supervision ends.
struct LiveSimSetup {
    router: crate::commands::run::InfrastructureRouter,
    connect: String,
    // Keep both session-wide locks alive for the entire supervision lifetime,
    // not merely while this setup future is being assembled.
    _locks: LiveSimulationLocks,
    board: BoardBackend,
    telemetry: crate::telemetry::TelemetryBackend,
    runtime_store: crate::stores::runtime_store::RuntimeStore,
    orderly_shutdown_timeout: std::time::Duration,
    stages: Vec<SupervisionStage>,
    supervisor_options: SupervisorOptions,
    action_tx: mpsc::Sender<SupervisorAction>,
    /// Every feed task that must stay alive for the whole session (log/
    /// presence subscribers, clock telemetry, live telemetry) -
    /// collected here instead of leaked under `_`-prefixed bindings (finding
    /// B6), so the caller can abort every one of them once supervision ends.
    background_tasks: crate::commands::run::AbortTasks,
}

struct LiveSimulationLocks {
    _run_lock: SupervisorLock,
    _simulator_lock: SupervisorLock,
}

impl LiveSimulationLocks {
    fn acquire(run_dir: &std::path::Path, simulator_lock_path: &std::path::Path) -> Result<Self> {
        Ok(Self {
            _run_lock: SupervisorLock::acquire(run_dir)?,
            _simulator_lock: SupervisorLock::acquire_path(simulator_lock_path)?,
        })
    }
}

/// Everything between preparation finishing and supervision beginning for a
/// live `simulation run`: Webots preflight, lock acquisition, world/
/// controller staging, and spawn-responder startup (finding A1's
/// "intermediate setup" gap), plus starting every feed/watcher task
/// supervision needs. Driven through `SessionController::drive_setup` (see
/// the call site) so Ctrl-C is observed the whole time this runs, not only
/// once it returns.
async fn live_simulate_setup(
    ui: crate::Ui,
    mut sim: SimPlan,
    options: SimulateOptions,
    events: mpsc::Sender<crate::session::event::SessionEvent>,
    token: tokio_util::sync::CancellationToken,
    output: OutputContext,
    renders_tui: bool,
) -> Result<LiveSimSetup> {
    let ensure_active = || {
        if token.is_cancelled() {
            bail!("simulation setup cancelled");
        }
        Ok(())
    };
    ensure_active()?;
    crate::host_doctor::preflight()
        .map_err(|error| anyhow!("{error}"))
        .context("Webots preflight failed; live simulate cannot launch the simulator")?;
    ensure_active()?;

    let run_dir = crate::host_paths::run_dir()?;
    let locks = LiveSimulationLocks::acquire(&run_dir, &crate::host_paths::simulator_lock_path()?)?;
    let runtime_root = crate::runtime_root::publish(&sim.ctx.project_root, &sim.ctx.resolved)
        .context("failed to publish the simulation runtime robot root")?;
    ensure_active()?;
    let board = BoardBackend::new();
    let runtime_store = sim.runtime_store.clone();
    let mut specs = Vec::new();
    crate::commands::run::prepare_site_tools(
        &sim.plan,
        &sim.ctx.resolved,
        &runtime_root,
        &board,
        &mut specs,
        &ui,
    )?;
    ensure_active()?;
    crate::commands::run::prepare_robot_participants(
        &sim.plan,
        &sim.ctx.resolved,
        &sim.ctx.project_root,
        &crate::commands::run::DriverPolicy::drivers_off_for_sim(),
        &board,
        &mut specs,
        &ui,
    )?;
    let (router, connect) = crate::commands::run::start_infrastructure_router(
        &sim.ctx.resolved,
        &sim.ctx.project_root,
        &ui,
    )
    .await?;
    crate::commands::run::apply_session_connect(&mut sim.plan, &mut specs, &connect);
    ensure_active()?;
    prepare_substitution_notes(&sim.plan, &board);

    let (webots_spec, spawn_descriptors) = stage_and_prepare_webots_spec(&ui, &sim)?;
    ensure_active()?;
    let mut background_tasks = crate::commands::run::AbortTasks::default();
    let spawn_responder = start_spawn_responder(&sim.plan, spawn_descriptors, &connect).await?;
    background_tasks.push(spawn_responder);
    ensure_active()?;
    let requested_stop = RequestedStop::new(WEBOTS_SITE_ID, webots_spec.shutdown_grace);
    specs.push(webots_spec);

    ui.info(format!(
        "simulation launch plan resolved: {} robot(s), {} site tool(s)",
        sim.plan.robots.len(),
        sim.plan.site.len()
    ));
    ui.info(format!("infrastructure router ready on {connect}"));
    crate::commands::run::report_launch_commands(&sim.plan, &specs, &ui)?;

    background_tasks.extend(
        sim.plan
            .robots
            .iter()
            .map(|robot| {
                start_bus_log_subscriber(
                    robot.namespace.clone(),
                    robot.id.clone(),
                    connect.clone(),
                    board.clone(),
                )
            })
            .collect::<Vec<_>>(),
    );
    // OBSERVED readiness: drive board state from each participant's own
    // presence/heartbeat, including SIMULATION-MANAGED ones (the
    // supervisor and every controller), which have no supervised
    // process of their own to poll.
    background_tasks.extend(sim.plan.robots.iter().map(|robot| {
        start_presence_heartbeat_subscriber(
            robot.namespace.clone(),
            robot.id.clone(),
            connect.clone(),
            board.clone(),
        )
    }));
    let clock_robot = sim
        .plan
        .robots
        .first()
        .context("sim launch plan has no robot for the clock telemetry feed")?;
    let (clock_rx, clock_task) = start_clock_feed(
        clock_robot.namespace.clone(),
        clock_robot.id.clone(),
        connect.clone(),
    );
    background_tasks.push(clock_task);
    // Clock observation is telemetry only. Startup and session state do not
    // wait for a sample; clocked services and drivers consume it independently
    // through their simulation-clock runner policy.
    let telemetry = crate::telemetry::TelemetryBackend::new();
    telemetry.set_clock_feed(clock_rx.clone());

    // The restart/hot-reload action channel always exists now (not just
    // under `--watch`), matching `commands::run`.
    let (action_tx, action_rx) = mpsc::channel(16);
    if options.watch {
        let live_ids = specs
            .iter()
            .map(|spec| spec.id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        background_tasks.push(crate::watch::spawn_sim_watch(
            crate::watch::SimWatchConfig {
                ctx: sim.ctx.clone(),
                options: options.clone(),
                live_ids,
                board: board.clone(),
                action_tx: action_tx.clone(),
            },
        ));
    }

    let stages = stages_for_simulate(specs, &sim.plan, output);

    // Live telemetry (CLI-UX Phase 3/4): only worth subscribing when
    // a real TUI is up to read it, same gate as `commands::run`. The
    // sim clock feed (`telemetry.set_clock_feed` above) is wired
    // unconditionally since it costs nothing extra - the SAME task
    // already exists for the title telemetry - but host/process/
    // router/joypad each open their own bus connection, so those
    // stay Tui-gated.
    let site_targets: Vec<(String, String)> = sim
        .plan
        .robots
        .iter()
        .map(|robot| (robot.namespace.clone(), robot.id.clone()))
        .collect();
    if renders_tui {
        background_tasks.extend(crate::commands::run::start_telemetry_feeds_at(
            &site_targets,
            &telemetry,
            &connect,
        ));
    }

    let starting = crate::session::state::SessionState::Preparing
        .start()
        .expect("the controller begins every session in Preparing");
    let _ = events
        .send(crate::session::event::SessionEvent::SessionChanged { state: starting })
        .await;
    let supervisor_options = SupervisorOptions {
        action_rx: Some(action_rx),
        requested_stop: Some(requested_stop),
        token: token.clone(),
        events: Some(events.clone()),
        emits_running_on_startup_complete: true,
        ..SupervisorOptions::default()
    };

    let orderly_shutdown_timeout = crate::supervisor::orderly_shutdown_budget(&stages);
    Ok(LiveSimSetup {
        router,
        connect,
        _locks: locks,
        board,
        telemetry,
        runtime_store,
        orderly_shutdown_timeout,
        stages,
        supervisor_options,
        action_tx,
        background_tasks,
    })
}

pub fn prepare(project_start: &Path, options: SimulateOptions) -> Result<SimPlan> {
    prepare_with_mode(project_start, options, SimulateMode::DryRun)
}

fn prepare_with_mode(
    project_start: &Path,
    options: SimulateOptions,
    mode: SimulateMode,
) -> Result<SimPlan> {
    let resolved = resolve_project(project_start, options.clone(), mode)?;
    if mode == SimulateMode::Live {
        let descriptors = crate::native_artifacts::descriptors_for(&resolved.resolved, true, true)?;
        crate::native_artifacts::prepare_descriptors_with_preflight(&descriptors, None)?;
    }
    let (plan, contract_surfaces) = build_checked_sim_launch_plan(
        &resolved.project_root,
        &resolved.world_path,
        &resolved.resolved,
        &resolved.manifest_extras,
        resolved.catalog.as_ref(),
    )?;
    // Finding A5: resolved once here, from the same `plan`/`contract_surfaces`
    // this function already has - see `RuntimeStore::from_launch_plan`'s docs.
    let runtime_store =
        crate::stores::runtime_store::RuntimeStore::from_launch_plan(&plan, &contract_surfaces);
    let source_participants = sim_source_participants(
        &resolved.project_root,
        &resolved.resolved,
        resolved.catalog.as_ref(),
    )?;
    Ok(SimPlan {
        plan,
        ctx: PlanContext {
            robot_path: resolved.robot_path,
            project_root: resolved.project_root,
            resolved: resolved.resolved,
            source_participants,
        },
        runtime_store,
    })
}

pub(crate) fn resolve_project(
    project_start: &Path,
    options: SimulateOptions,
    mode: SimulateMode,
) -> Result<ResolvedSimulation> {
    let robot_path = crate::resolver::discover_robot_yaml(project_start)
        .with_context(|| format!("failed to find robot.yaml from {}", project_start.display()))?;
    let project_root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?
        .to_path_buf();
    let world_path = world::resolve_world(&project_root, &options.world)?;
    let loaded = if options.overlays.is_empty() {
        crate::resolver::load_robot_with_extras(&robot_path)?
    } else {
        crate::resolver::load_robot_with_extras_and_overlays(&robot_path, &options.overlays)?
    };
    let robot = loaded.robot;
    let manifest_extras = loaded.extras;
    let catalog = crate::commands::catalog_or_vendored(crate::catalog::load_pinned_catalog(
        crate::catalog::CatalogLoadOptions {
            cli_source: options.catalog_source.clone(),
            robot_source: manifest_extras.catalog_source.as_ref().map(|source| {
                if source.is_absolute() {
                    source.clone()
                } else {
                    project_root.join(source)
                }
            }),
            offline: false,
        },
        crate::catalog::selection_channel(robot.artifacts.channel),
    ))?;

    // Always resolve live git component driver commits so driver metadata can
    // be staged. Component asset git refs are resolved only for live simulate,
    // where Webots world staging genuinely needs local asset files; dry-run
    // reports the intended staged paths without fetching assets.
    // The robot's own official artifacts (services + component drivers) resolve
    // for `--target` when set, so a Linux robot can be planned from a non-Linux
    // host; the simulator itself keeps the host target since Webots runs locally.
    let official_target = options
        .target
        .as_deref()
        .map(crate::resolver::resolve_target_triple)
        .transpose()?;
    let resolved = resolve(
        &robot,
        &project_root,
        catalog.as_ref(),
        ResolveOptions {
            resolve_source_commits: true,
            resolve_component_asset_commits: mode == SimulateMode::Live,
            official_target_triple: official_target,
            ..ResolveOptions::default()
        },
    )?;
    Ok(ResolvedSimulation {
        robot_path,
        project_root,
        world_path,
        resolved,
        manifest_extras,
        catalog,
    })
}

/// Build the checked simulation launch plan. Every source participant
/// (drivers, path-overridden services/simulators) rebuilds live - there is no
/// disk cache to scope a rebuild around (docs: `check::build_emit_apis_from_source`
/// never caches), so a `watch`-triggered recheck simply rebuilds the whole
/// source graph rather than just the one crate that changed.
/// Also returns the (already sim-filtered/remapped) contract surfaces
/// alongside the plan (finding A5) - the caller needs both to build a
/// `RuntimeStore`, and re-deriving them separately would duplicate the whole
/// metadata/check pass this function already ran.
pub(crate) fn build_checked_sim_launch_plan(
    project_root: &Path,
    world: &Path,
    resolved: &ResolvedRobot,
    manifest_extras: &RobotManifestExtras,
    catalog: Option<&Catalog>,
) -> Result<(LaunchPlan, Vec<graph_check::ParticipantContractSurface>)> {
    let source_participants = sim_source_participants(project_root, resolved, catalog)
        .with_context(|| "failed to prepare source participants for simulation metadata")?;
    // Finding A6: all three filters below admit exactly the standard site-
    // tool set (`STANDARD_SITE_TOOLS` - router/joypad/telemetry), derived
    // once in `launch_plan` and shared with `build_site_launches` there.
    // This used to hardcode only router+joypad, silently excluding
    // telemetry's declared graph contracts from validation even though
    // telemetry is started and readiness-waited exactly like the other two.
    let metadata_source_participants = source_participants
        .iter()
        .filter(|participant| {
            participant.kind != SourceParticipantKind::Tool
                || STANDARD_SITE_TOOLS.contains(&participant.name.as_str())
        })
        .cloned()
        .collect::<Vec<_>>();
    // A Catalog-sourced component driver is a platform ref here too (docs
    // #21), exactly like `check`/`run`/`deploy` - synthesized from catalog
    // metadata rather than built from source. Only a Path/Git-overridden
    // driver crate reaches the `build` closure below.
    let platform_refs = check_artifact_refs_from_resolved(resolved)
        .into_iter()
        .filter(|artifact| {
            artifact.kind != crate::catalog::ArtifactKind::Tool
                || STANDARD_SITE_TOOLS.contains(&artifact.name.as_str())
        })
        .collect::<Vec<_>>();
    let tool_participants = tool_participants_from_resolved(resolved)?
        .into_iter()
        .filter(|tool| STANDARD_SITE_TOOLS.contains(&tool.name.as_str()))
        .collect::<Vec<_>>();
    let mut official_by_ref = resolved
        .platform_runtimes
        .iter()
        .map(|runtime| (runtime.artifact_ref().to_string(), runtime))
        .collect::<BTreeMap<_, _>>();
    official_by_ref.extend(crate::commands::check::component_driver_runtimes_by_ref(
        resolved,
    ));
    let tools_by_ref = resolved
        .tools
        .iter()
        .map(|tool| (tool.asset.clone(), tool))
        .collect::<BTreeMap<_, _>>();

    let metadata_outcome = run_check_with_context(
        &platform_refs,
        &tool_participants,
        &metadata_source_participants,
        CheckGraphContext { manifest_extras },
        |artifact_ref| {
            if let Some(runtime) = official_by_ref.get(artifact_ref) {
                return extract_emit_apis_from_staged_runtime(runtime);
            }
            if let Some(tool) = tools_by_ref.get(artifact_ref) {
                return extract_emit_apis_from_staged_tool(tool);
            }
            Err(anyhow!(
                "resolved official artifact {artifact_ref} is not in the catalog"
            ))
        },
        fetch_emit_apis_from_tool,
        |participant| {
            if participant.kind == SourceParticipantKind::ComponentDriver {
                return build_emit_apis_from_source(participant)
                    .map_err(|error| driver_metadata_unavailable(participant, error));
            }
            build_emit_apis_from_source(participant)
        },
    )?;

    let mut checked_participants = metadata_outcome.checked_participants.clone();
    let mut contract_surfaces = metadata_outcome.contract_surfaces.clone();
    remap_simulator_participant_ids(&mut checked_participants, &resolved.robot.robot.id)?;
    remap_simulator_surface_ids(&checked_participants, &mut contract_surfaces);
    let (official_simulators, official_simulator_surfaces) =
        official_simulator_participants(resolved)?;
    checked_participants.extend(official_simulators);
    contract_surfaces.extend(official_simulator_surfaces);
    let controller_provider_id = simulator_controller_provider_id(&resolved.robot.robot.id);
    let substitutions = simulated_component_records(&checked_participants, &controller_provider_id);
    let sim_participants = sim_checked_participants(&checked_participants);
    let sim_ids = sim_participants
        .iter()
        .map(|participant| participant.participant_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    contract_surfaces.retain(|surface| sim_ids.contains(surface.participant_id.as_str()));
    let report = graph_check::check_graph(&sim_participants);
    if !report.is_ok() {
        crate::commands::check::ensure_check_outcome_ok(
            &resolved.channel.to_string(),
            &crate::commands::check::CheckOutcome {
                missing_images: Vec::new(),
                report: report.clone(),
                checked_participants: sim_participants.clone(),
                contract_surfaces: Vec::new(),
            },
        )?;
    }

    let plan = build_launch_plan(
        LaunchMode::Webots {
            world: world.to_path_buf(),
        },
        &[CheckedRobotLaunchInput {
            project_root,
            resolved,
            manifest_extras,
            checked_participants: &sim_participants,
            substitutions: &substitutions,
            source_participants: &source_participants,
        }],
    )?;
    let coherence_graph = crate::commands::check::robot_contract_surfaces(
        &resolved.robot.robot.id,
        &contract_surfaces,
    );
    let coherence = crate::commands::check::coherence_for_launch_plan(&plan, &[coherence_graph])?;
    crate::commands::check::enforce_coherence(
        crate::commands::check::CoherenceVerb::Simulate,
        &coherence,
    )?;
    Ok((plan, contract_surfaces))
}

fn official_simulator_participants(
    resolved: &ResolvedRobot,
) -> Result<(
    Vec<graph_check::ParticipantApis>,
    Vec<graph_check::ParticipantContractSurface>,
)> {
    let robot_id = resolved.robot.robot.id.as_str();
    let mut participants = Vec::new();
    let mut surfaces = Vec::new();
    for runtime in resolved
        .simulators
        .iter()
        .filter(|runtime| runtime.source_path().is_none())
    {
        let raw = extract_emit_apis_from_staged_runtime(runtime).with_context(|| {
            format!(
                "failed to synthesize catalog emit-apis for simulator {}",
                runtime.name
            )
        })?;
        if raw.artifact.kind != "simulator" || raw.artifact.id != runtime.name {
            bail!(
                "official simulator emit-apis artifact {} '{}' does not match expected simulator '{}'",
                raw.artifact.kind,
                raw.artifact.id,
                runtime.name
            );
        }
        let mut participant =
            graph_check::ParticipantApis::try_from(raw.clone()).with_context(|| {
                format!(
                    "failed to interpret emit-apis for simulator {}",
                    runtime.name
                )
            })?;
        participant.participant_id =
            simulator_participant_id_for_resolved_artifact(&runtime.name, robot_id).ok_or_else(
                || {
                    anyhow!(
                        "unrecognized simulator artifact name '{}'; expected '{}' or '{}'",
                        runtime.name,
                        SIMULATOR_SUPERVISOR_ARTIFACT_NAME,
                        SIMULATOR_CONTROLLER_ARTIFACT_NAME
                    )
                },
            )?;
        surfaces.push(crate::commands::check::contract_surface(
            &raw,
            participant.participant_id.clone(),
        ));
        participants.push(participant);
    }
    Ok((participants, surfaces))
}

fn remap_simulator_surface_ids(
    participants: &[graph_check::ParticipantApis],
    surfaces: &mut [graph_check::ParticipantContractSurface],
) {
    let simulator_ids = participants
        .iter()
        .filter(|participant| {
            participant.participant_kind == graph_check::ParticipantKind::Simulator
        })
        .map(|participant| {
            (
                participant.artifact_id.as_str(),
                participant.participant_id.as_str(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    for surface in surfaces {
        if let Some(participant_id) = simulator_ids.get(surface.participant_id.as_str()) {
            surface.participant_id = (*participant_id).to_string();
        }
    }
}

fn remap_simulator_participant_ids(
    participants: &mut [graph_check::ParticipantApis],
    robot_id: &str,
) -> Result<()> {
    for participant in participants.iter_mut().filter(|participant| {
        participant.participant_kind == graph_check::ParticipantKind::Simulator
    }) {
        participant.participant_id =
            simulator_participant_id_for_resolved_artifact(&participant.artifact_id, robot_id)
                .ok_or_else(|| {
                    anyhow!(
                        "unrecognized simulator artifact name '{}'; expected '{}' or '{}'",
                        participant.artifact_id,
                        SIMULATOR_SUPERVISOR_ARTIFACT_NAME,
                        SIMULATOR_CONTROLLER_ARTIFACT_NAME
                    )
                })?;
    }
    Ok(())
}

/// Map a resolved simulator artifact name (`ResolvedPlatformRuntime::name`,
/// e.g. `"webots-supervisor"` / `"webots-controller"`) to its participant id:
/// the supervisor gets the stable world-scoped id, the controller gets the
/// robot-scoped substitution-provider id. `None` for any other simulator
/// artifact name - callers decide whether that is a hard error (constructing
/// the participant) or simply "not one of the two known roles" (computing the
/// expected id set for parity checks).
pub(crate) fn simulator_participant_id_for_resolved_artifact(
    artifact_name: &str,
    robot_id: &str,
) -> Option<String> {
    if artifact_name == SIMULATOR_SUPERVISOR_ARTIFACT_NAME {
        Some(SIMULATOR_SUPERVISOR_PROVIDER_ID.to_string())
    } else if artifact_name == SIMULATOR_CONTROLLER_ARTIFACT_NAME {
        Some(simulator_controller_provider_id(robot_id))
    } else {
        None
    }
}

/// Simulate's source participants: identical to
/// `source_participants_from_resolved` (a Catalog-sourced driver is never a
/// source participant - see `component_driver_platform_refs_from_resolved`),
/// sorted for stable dry-run/watch-target output. `catalog` is accepted for
/// signature symmetry with the other `sim_*` helpers but no longer consulted
/// here now that catalog drivers route entirely through the platform-ref path.
pub(crate) fn sim_source_participants(
    project_root: &Path,
    resolved: &ResolvedRobot,
    _catalog: Option<&Catalog>,
) -> Result<Vec<SourceParticipant>> {
    let mut participants =
        source_participants_from_resolved(project_root, resolved, component_driver_crate_dir)?;
    participants.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(participants)
}

fn driver_metadata_unavailable(
    participant: &SourceParticipant,
    error: anyhow::Error,
) -> anyhow::Error {
    anyhow!(
        "DriverMetadataUnavailable: component driver crate '{}' for instance '{}' could not build on this host to extract its compiled-in API metadata section: {error:#}\n\nCustom and git-sourced driver crates must compile far enough on the dev host to emit the `#[derive(phoxal::Api)]` linker section; keep hardware transport behind a target cfg boundary such as `cfg(target_os = \"linux\")`. Alternatively use a verified artifact catalog entry with inlined driver metadata.",
        participant.expected_artifact_id,
        participant.name
    )
}

/// Board display only: which component-driver participants sim dropped from
/// the checked set (see `sim_checked_participants`) are instead "simulated by"
/// the Webots controller. This is not a checked fact - `phoxal::check` (0.28+)
/// no longer has a substitution concept, materializes nothing, and exposes no
/// way to recover a materialized topic from a report - it is purely the CLI's
/// own record of a caller-side plan choice, so it can render "component X
/// simulated by webots-controller" on the sim board and dry-run output.
///
/// A driver's own contract report carries only `family` per contract now (no
/// `schema_id`/`topic`/`direction`, D1), so there is nothing left to
/// materialize per instance here - the record just carries the family for
/// display.
pub(crate) fn simulated_component_records(
    participants: &[graph_check::ParticipantApis],
    provider_participant_id: &str,
) -> Vec<SubstitutionRecord> {
    let mut records = participants
        .iter()
        .filter(|participant| {
            participant.participant_class.is_checked()
                && participant.participant_kind == graph_check::ParticipantKind::Driver
        })
        .filter_map(|participant| match &participant.scope {
            graph_check::ParticipantScope::ComponentInstance(instance) => {
                Some(SubstitutionRecord {
                    component_instance: instance.clone(),
                    provider_participant_id: provider_participant_id.to_string(),
                    provider_artifact_id: SIMULATOR_CONTROLLER_ARTIFACT_NAME.to_string(),
                    provider_kind: "simulator".to_string(),
                    contracts: participant
                        .contracts
                        .iter()
                        .map(|contract| SubstitutedContract {
                            family: contract.family.clone(),
                        })
                        .collect(),
                })
            }
            graph_check::ParticipantScope::Graph => None,
        })
        .collect::<Vec<_>>();
    records.sort_by(|left, right| {
        left.component_instance
            .cmp(&right.component_instance)
            .then_with(|| {
                left.provider_participant_id
                    .cmp(&right.provider_participant_id)
            })
    });
    records
}

fn sim_checked_participants(
    participants: &[graph_check::ParticipantApis],
) -> Vec<graph_check::ParticipantApis> {
    participants
        .iter()
        .filter(|participant| participant.participant_kind != graph_check::ParticipantKind::Driver)
        .cloned()
        .collect()
}

fn report_plan_only(sim: &SimPlan) -> Result<()> {
    let output = build_dry_run_output(sim);
    println!("channel: {}", sim.ctx.resolved.channel);
    if let Some(revision) = &sim.ctx.resolved.catalog_snapshot {
        println!("catalog revision: {revision}");
    }
    println!(
        "official services ({}):",
        sim.ctx.resolved.platform_runtimes.len()
    );
    for runtime in &sim.ctx.resolved.platform_runtimes {
        println!("  - {} -> {}", runtime.name, runtime.artifact_ref());
    }
    println!("world: {}", output.world_path.display());
    println!("router: {}", output.bus_connect);
    // Out of the project tree now (`<project>/.phoxal/webots`,
    // see `webots_stage_root`), so print it explicitly for discoverability
    // even though nothing is written in dry-run mode.
    if let Ok(root) = webots_stage_root::root() {
        println!("staged simulation to {}", root.display());
    }
    println!("site tools:");
    for tool in &output.native_tools {
        println!("  - {tool}");
    }
    println!(
        "webots app (CLI-managed, id \"{WEBOTS_SITE_ID}\"): would launch pointed at staged world {}",
        output.webots_app.intended_staged_world_path.display()
    );
    if !output.simulator_artifacts.is_empty() {
        println!("simulator artifacts:");
        for artifact in &output.simulator_artifacts {
            println!("  - {artifact}");
        }
    }
    if !output.simulation_managed_participants.is_empty() {
        println!("simulation-managed participants (launched by Webots, not the CLI):");
        for participant in &output.simulation_managed_participants {
            println!("  - {participant}");
        }
    }
    if !output.substitutions.is_empty() {
        println!("substitutions:");
        for substitution in &output.substitutions {
            println!("  - {substitution}");
        }
    }
    println!("dry-run - no files written and no simulation processes started");
    Ok(())
}

/// Build the dry-run report body (Part 6): must show the Webots app as the
/// CLI-managed child, both simulator artifacts (supervisor + controller) with
/// their participant ids, and each simulator participant's SIMULATION-MANAGED
/// ownership + the intended staged world path. Never stages or launches
/// anything - the path is computed, not written.
fn build_dry_run_output(sim: &SimPlan) -> SimulateDryRunOutput {
    let substitutions = substitution_lines(&sim.plan);
    let simulator_artifacts = simulator_artifact_lines(&sim.ctx.resolved);
    let simulation_managed = simulation_managed_lines(&sim.plan);
    let world_path = webots_world(&sim.plan.mode).to_path_buf();
    let intended_staged_world_path = intended_staged_world_path(&world_path);
    let native_tools = native_tool_labels_from_plan(&sim.plan);
    SimulateDryRunOutput {
        mode: "dry-run",
        channel: sim.ctx.resolved.channel.to_string(),
        catalog_snapshot: sim.ctx.resolved.catalog_snapshot.clone(),
        world_path,
        bus_connect: DEFAULT_ROUTER_CONNECT.to_string(),
        platform_service_count: sim.ctx.resolved.platform_runtimes.len(),
        native_tools,
        substitutions,
        webots_app: WebotsAppSummary {
            site_id: WEBOTS_SITE_ID.to_string(),
            launch_ownership: "cli_managed".to_string(),
            intended_staged_world_path,
        },
        simulator_artifacts,
        simulation_managed_participants: simulation_managed,
    }
}

/// Extract the resolved `.wbt` world path a sim `LaunchPlan`'s mode carries.
/// `simulate` always builds `LaunchMode::Webots`, so any other mode here is a
/// caller bug, not a user-facing error.
fn webots_world(mode: &LaunchMode) -> &Path {
    match mode {
        LaunchMode::Webots { world } => world.as_path(),
        _ => unreachable!("simulate always builds a plan with LaunchMode::Webots"),
    }
}

#[derive(Debug, Serialize)]
struct SimulateDryRunOutput {
    mode: &'static str,
    channel: String,
    catalog_snapshot: Option<String>,
    world_path: PathBuf,
    bus_connect: String,
    platform_service_count: usize,
    native_tools: Vec<String>,
    substitutions: Vec<String>,
    webots_app: WebotsAppSummary,
    simulator_artifacts: Vec<String>,
    simulation_managed_participants: Vec<String>,
}

#[derive(Debug, Serialize)]
struct WebotsAppSummary {
    site_id: String,
    launch_ownership: String,
    intended_staged_world_path: PathBuf,
}

/// The staged world path `simulate --dry-run` would produce, without actually
/// staging (Part 6: dry-run reports the intended path but never launches
/// Webots or writes staged files). Home-based (`webots_stage_root`), not
/// project-relative - see the module doc for why.
fn intended_staged_world_path(world_path: &Path) -> PathBuf {
    let world_name = world_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("default");
    webots_stage_root::world_path(world_name).unwrap_or_else(|_| world_path.to_path_buf())
}

/// One line per resolved simulator artifact (supervisor + controller), naming
/// the artifact and its participant id.
fn simulator_artifact_lines(resolved: &ResolvedRobot) -> Vec<String> {
    let robot_id = resolved.robot.robot.id.as_str();
    resolved
        .simulators
        .iter()
        .filter_map(|runtime| {
            let participant_id =
                simulator_participant_id_for_resolved_artifact(&runtime.name, robot_id)?;
            Some(format!(
                "{} (artifact {}, participant id {participant_id})",
                runtime.name,
                runtime.artifact_ref()
            ))
        })
        .collect()
}

/// One line per SIMULATION-MANAGED participant in the plan: Webots (via the
/// supervisor) owns its lifecycle, not the CLI supervisor.
fn simulation_managed_lines(plan: &LaunchPlan) -> Vec<String> {
    plan.robots
        .iter()
        .flat_map(|robot| &robot.participants)
        .filter(|participant| {
            participant.launch_ownership == crate::launch_plan::LaunchOwnership::SimulationManaged
        })
        .map(|participant| {
            format!(
                "{} (artifact {})",
                participant.launch.participant_id, participant.artifact_id
            )
        })
        .collect()
}

/// The bare participant ids of every SIMULATION-MANAGED participant in the
/// plan (the Webots supervisor plus one controller per robot) - the readiness
/// barrier's counterpart to `simulation_managed_lines`, which formats the same
/// filtered set for display instead of returning ids to wait on.
fn simulation_managed_participant_ids(plan: &LaunchPlan) -> Vec<String> {
    plan.robots
        .iter()
        .flat_map(|robot| &robot.participants)
        .filter(|participant| {
            participant.launch_ownership == crate::launch_plan::LaunchOwnership::SimulationManaged
        })
        .map(|participant| participant.launch.participant_id.clone())
        .collect()
}

/// Partition `simulation run`'s already-built spec list (site tools +
/// services + the Webots app, in that order - see the call site) plus the
/// plan's SIMULATION-MANAGED (wait-only, spec-less) ids into the staged
/// startup order (Part 2): router < other tools < Webots app < simulation
/// supervisor (wait-only) < services < robot/controllers (wait-only). The
/// deferred-spawn machinery (the
/// `SpawnSet` bus responder, importable PROTOs, controller-readiness
/// observation) is unchanged, this only reorders WHEN the CLI hands specs to
/// the supervisor and adds explicit wait-only stages for the participants
/// Webots itself spawns. Once the controller stage is ready the simulation
/// session is initialized; clock samples are independent telemetry and do
/// not form another stage.
fn stages_for_simulate(
    specs: Vec<ParticipantSpec>,
    plan: &LaunchPlan,
    output: OutputContext,
) -> Vec<SupervisionStage> {
    let mut tools = Vec::new();
    let mut webots = Vec::new();
    let mut services = Vec::new();
    for spec in specs {
        if spec.id == WEBOTS_SITE_ID {
            webots.push(spec);
        } else if spec.kind == ParticipantKind::Tool {
            tools.push(spec);
        } else {
            services.push(spec);
        }
    }
    let (supervisor_ids, controller_ids): (Vec<String>, Vec<String>) =
        simulation_managed_participant_ids(plan)
            .into_iter()
            .partition(|id| id == SIMULATOR_SUPERVISOR_PROVIDER_ID);
    // Product decision 6: no unconditional 60s teardown for an interactive
    // session - see `OutputContext::wait_budget`.
    let timeout = output.wait_budget(SIMULATE_READINESS_TIMEOUT);
    vec![
        SupervisionStage::new("starting tools", tools, timeout),
        SupervisionStage::new("starting Webots", webots, timeout),
        SupervisionStage::new("waiting for the simulation supervisor", Vec::new(), timeout)
            .with_extra_ready_ids(supervisor_ids),
        SupervisionStage::new("starting services", services, timeout),
        SupervisionStage::new("waiting for robot controllers", Vec::new(), timeout)
            .with_extra_ready_ids(controller_ids),
    ]
}

fn prepare_substitution_notes(plan: &LaunchPlan, board: &BoardBackend) {
    for robot in &plan.robots {
        for substitution in &robot.substitutions {
            let mut status = ParticipantStatus::new(
                &substitution.component_instance,
                ParticipantKind::Driver,
                ParticipantState::Ready,
            );
            status.note = Some(substitution_note(substitution));
            board.upsert(status);
        }
    }
}

fn substitution_lines(plan: &LaunchPlan) -> Vec<String> {
    plan.robots
        .iter()
        .flat_map(|robot| robot.substitutions.iter().map(render_substitution))
        .collect()
}

fn render_substitution(substitution: &SubstitutionRecord) -> String {
    format!(
        "{} : satisfied by {} ({})",
        substitution_topic_summary(substitution),
        substitution.provider_participant_id,
        substitution.provider_artifact_id
    )
}

fn substitution_note(substitution: &SubstitutionRecord) -> String {
    format!(
        "simulated by {} ({})",
        substitution.provider_participant_id, substitution.provider_artifact_id
    )
}

/// A driver's own contract report now carries only `family` per contract (no
/// `schema_id`/`topic`, D1), so there is no per-topic wire detail left to
/// summarize here - this collapses to the component instance's wildcard,
/// same as the old "every contract is under this component" shortcut.
fn substitution_topic_summary(substitution: &SubstitutionRecord) -> String {
    format!("component/{}/*", substitution.component_instance)
}

/// Site tool labels are derived straight from the resolved `LaunchPlan` (Part
/// 3/6): router, `tool-joypad`, and `tool-telemetry` are standard, hard-
/// required site tools in every mode including Webots (product decision 9),
/// so they always appear here alongside the Webots app itself. This function
/// never needs `options` - it replaces the old `SimulatePlan::native_tools`
/// stored field.
fn native_tool_labels_from_plan(plan: &LaunchPlan) -> Vec<String> {
    let mut labels = plan
        .site
        .iter()
        .map(|site| site.id.clone())
        .collect::<Vec<_>>();
    labels.push(WEBOTS_SITE_ID.to_string());
    labels
}

/// The board id + `ParticipantKind` the CLI registers the Webots app under.
/// Webots is the CLI's only simulator-side child (Part 5): the CLI launches
/// it pointed at the staged world; everything downstream (the supervisor,
/// each robot's controller) is Webots-spawned and SIMULATION-MANAGED instead.
pub(crate) const WEBOTS_SITE_ID: &str = "webots";

/// Stage the simulation world (Part 4) for the resolved robot and build the
/// `ParticipantSpec` that launches the Webots app pointed at it - the CLI's
/// only simulator-side child. The supervisor and controller participants are
/// registered separately by `prepare_robot_participants` (SIMULATION-MANAGED,
/// no spec of their own); this function only produces Webots's own spec.
fn stage_and_prepare_webots_spec(
    ui: &crate::Ui,
    sim: &SimPlan,
) -> Result<(ParticipantSpec, Vec<RobotSpawn>)> {
    let world = webots_world(&sim.plan.mode);
    let staged =
        stage_simulation_for_robot(&sim.ctx.project_root, world, &sim.ctx.resolved, &sim.plan)?;
    stage_simulator_controller_binaries(&sim.ctx.resolved, ui)?;
    let webots_path = crate::host_doctor::webots_executable_path()
        .map_err(|error| anyhow!("{error}"))
        .context("failed to locate the Webots executable for live simulate")?;
    // Print the generated project-local staging root explicitly.
    ui.info(format!(
        "staged simulation to {}",
        webots_stage_root::root()?.display()
    ));
    ui.info(format!(
        "staged simulation world at {}",
        staged.staged_world_path.display()
    ));
    let spec = ParticipantSpec {
        id: WEBOTS_SITE_ID.to_string(),
        kind: ParticipantKind::Tool,
        executable: webots_path,
        args: webots_launch_args(&staged.staged_world_path),
        cwd: None,
        env: Vec::new(),
        shutdown_grace: std::time::Duration::from_secs(20),
        process_group: true,
        note: None,
        // The Webots application itself has no bus identity of its own - it
        // never publishes a presence/heartbeat (the supervisor and each
        // controller Webots launches do, and those are tracked separately as
        // SIMULATION-MANAGED participants). Its readiness is necessarily
        // process-lifecycle-only, so it keeps the old spawn-is-ready behavior.
        bus_participant: false,
    };
    Ok((spec, staged.spawn_descriptors))
}

/// Declare and keep alive the query responder that owns the simulation spawn
/// set. With an external router, declaration completes before the caller gives
/// Webots to the process supervisor. With a CLI-managed router, this task
/// retries until that router starts, while the Webots supervisor retries its
/// bounded query. The task and its bus session live until simulation ends.
async fn start_spawn_responder(
    launch_plan: &LaunchPlan,
    robots: Vec<RobotSpawn>,
    connect: &str,
) -> Result<JoinHandle<()>> {
    let robot = launch_plan
        .robots
        .first()
        .context("sim launch plan has no robot for the spawn responder bus root")?;
    let bus_config = BusConfig {
        namespace: robot.namespace.clone(),
        robot_id: robot.id.clone(),
        participant: "phoxal-cli-simulation-spawn".to_string(),
        incarnation: 0,
        connect_endpoints: vec![connect.to_string()],
    };
    let response = MessagePack::encode(&SpawnSet {
        revision: 1,
        robots,
    })
    .context("failed to encode simulation spawn set")?;

    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
        let mut ready_tx = Some(ready_tx);
        loop {
            wait_for_endpoint(&bus_config.connect_endpoints[0]).await;
            let bus = match Bus::open(bus_config.clone()).await {
                Ok(bus) => bus,
                Err(error) => {
                    tracing::debug!(%error, "simulation spawn responder waiting for router");
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    continue;
                }
            };
            let queryable = match bus
                .declare_server(<SpawnRequest as ContractBody>::TOPIC)
                .await
            {
                Ok(queryable) => queryable,
                Err(error) => {
                    tracing::debug!(%error, "simulation spawn responder declaration failed");
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    continue;
                }
            };
            if let Some(ready_tx) = ready_tx.take() {
                let _ = ready_tx.send(());
            }

            if let Err(error) = serve_spawn_queries(&bus, &queryable, &response).await {
                tracing::warn!(%error, "simulation spawn responder disconnected; retrying");
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    });

    // With an external router, declaration must finish before Webots is added
    // to supervision. With a CLI-managed router, the router is itself launched
    // by that supervision call, so the responder retries in parallel and the
    // supervisor's bounded query retry bridges the bootstrap dependency.
    ready_rx
        .await
        .context("simulation spawn responder exited before declaring its queryable")?;

    Ok(handle)
}

async fn serve_spawn_queries(
    bus: &Bus,
    queryable: &phoxal::raw::ServerQueryable,
    response: &[u8],
) -> Result<()> {
    loop {
        let incoming = queryable.recv().await?;
        let request = incoming
            .request_metadata()
            .and_then(|_| incoming.request_bytes())
            .and_then(|bytes| {
                MessagePack::decode::<SpawnRequest>(&bytes)
                    .map_err(|error| phoxal::bus::BusError::Transport(error.to_string()))
            });
        match request {
            Ok(request) => {
                tracing::debug!(
                    known_revision = request.known_revision,
                    "serving simulation spawn set"
                );
                incoming.reply(bus, response.to_vec()).await?;
            }
            Err(error) => {
                let failure = QueryFailure::invalid_argument(format!(
                    "invalid simulation spawn request: {error}"
                ));
                incoming.reply_err(&failure).await?;
            }
        }
    }
}

/// Build Webots' argv for a live simulate launch.
///
/// `--mode=realtime` is load-bearing, not cosmetic: Webots opens a world in the
/// PAUSED state by default, so without an explicit run mode the supervisor's
/// `#[step]` is never called, `simulation/clock` never advances, and services
/// that use simulation time remain idle (only wall-clock traffic like
/// `presence/heartbeat` flows). `realtime` starts the simulation running,
/// synced to wall time so the operator watches the robot move at a natural
/// speed; the clock authority (the Webots supervisor) still owns logical time.
///
/// `--batch` suppresses Webots' blocking modal dialogs (notably the "save world
/// changes?" prompt on quit), so the CLI's requested SIGTERM stop can complete
/// without an operator having to dismiss a popup.
fn webots_launch_args(staged_world_path: &Path) -> Vec<String> {
    vec![
        "--mode=realtime".to_string(),
        "--batch".to_string(),
        staged_world_path.display().to_string(),
    ]
}

/// Stage the two Webots controller BINARIES (supervisor + per-robot
/// controller) into the standard Webots layout,
/// `<project>/.phoxal/webots/controllers/<controller-name>/<controller-name>`
/// (`webots_stage_root` names these paths). Webots looks up a world node's
/// `controller "<name>"` field under exactly this `controllers/<name>/<name>`
/// path; when the executable is missing it silently falls back to its own
/// built-in `generic` controller instead of running ours, so this staging
/// step is load-bearing for live simulate, not cosmetic.
///
/// The staged entry is a SYMLINK to the resolved binary, not a copy - the
/// cache (or the path-pinned dev build's `target/` directory) stays the
/// single source of truth. Webots execs `controllers/<name>/<name>` directly
/// and gets its runtime lib env from its own launch, so the physical location
/// the symlink resolves to does not matter.
///
/// For a PATH-OVERRIDDEN simulator (`runtime.source_path()` is `Some`, the
/// local-dev / live-gate case), the binary is built fresh with
/// `crate::commands::run::build_source_binary`, which runs `cargo build --bin
/// <name>` in the simulator crate and returns the built path - the same
/// mechanism every other path-overridden participant (services, tools,
/// drivers) already uses via `run.rs`. Cargo's own `target_directory` is
/// always absolute, so this is already a legal symlink target.
///
/// For a CATALOG simulator (no path override), the binary is obtained the
/// same way every other official/native artifact is provisioned: resolve a
/// `NativeArtifactDescriptor` from the runtime and look it up in the artifact
/// cache via `native_artifacts::artifact_binary_path` - mirroring
/// `commands::run::locate_official_binary`. The cache lives under the
/// (already absolute) `host_paths::cache_dir()`. If the cache entry is
/// missing, this is a hard error (`NativePending`-style), not a silent skip:
/// a missing controller binary must never be allowed to fall through to
/// Webots' `generic` controller unnoticed.
fn stage_simulator_controller_binaries(resolved: &ResolvedRobot, ui: &crate::Ui) -> Result<()> {
    let webots_home = detected_webots_home_for_build_env();
    for runtime in &resolved.simulators {
        let controller_name = webots_controller_name_for_simulator_artifact(&runtime.name)
            .ok_or_else(|| {
                anyhow!(
                    "unrecognized simulator artifact '{}'; expected '{}' or '{}'",
                    runtime.name,
                    SIMULATOR_SUPERVISOR_ARTIFACT_NAME,
                    SIMULATOR_CONTROLLER_ARTIFACT_NAME
                )
            })?;
        let resolved_binary = if let Some(crate_dir) = runtime.source_path() {
            let preferred_name = format!("phoxal-simulator-{}", runtime.name);
            let _env_guard = webots_home
                .as_ref()
                .map(|home| WebotsHomeEnvGuard::set(home));
            crate::commands::run::build_source_binary(crate_dir, &preferred_name, ui).with_context(
                || {
                    format!(
                        "failed to build path-overridden simulator '{}' from {}",
                        runtime.name,
                        crate_dir.display()
                    )
                },
            )?
        } else {
            provisioned_official_simulator_binary(runtime)?
        };
        require_absolute_symlink_target("resolved simulator binary", &resolved_binary)?;
        let staged_dir = webots_stage_root::controller_dir(controller_name)?;
        std::fs::create_dir_all(&staged_dir).with_context(|| {
            format!(
                "failed to create staged controller directory {}",
                staged_dir.display()
            )
        })?;
        let staged_binary = staged_dir.join(controller_name);
        std::os::unix::fs::symlink(&resolved_binary, &staged_binary).with_context(|| {
            format!(
                "failed to symlink simulator binary {} to staged controller path {}",
                resolved_binary.display(),
                staged_binary.display()
            )
        })?;
        ui.info(format!(
            "staged simulator controller binary {} at {} (symlink to {})",
            runtime.name,
            staged_binary.display(),
            resolved_binary.display()
        ));
    }
    Ok(())
}

/// Symlink targets into the staged simulation must be absolute (Webots' cwd
/// when it execs `controllers/<name>/<name>` is not the staged tree, so a
/// relative symlink would not resolve). Both sources this crate ever
/// symlinks from - the native-artifact cache and a path-pinned crate's cargo
/// `target_directory` - are already absolute by construction; this asserts
/// that rather than silently trying to fix up a relative one.
fn require_absolute_symlink_target(label: &str, path: &Path) -> Result<()> {
    if path.is_absolute() {
        Ok(())
    } else {
        bail!(
            "{label} must be an absolute path to symlink into the staged simulation, got {}",
            path.display()
        );
    }
}

/// Map a resolved simulator artifact name to its Webots controller directory
/// name (the value that must appear in the staged world's `controller "..."`
/// field and the `controllers/<name>/<name>` staged path) - the inverse
/// mapping of participant ids, but keyed to the on-disk Webots layout instead
/// of the bus participant id.
fn webots_controller_name_for_simulator_artifact(artifact_name: &str) -> Option<&'static str> {
    if artifact_name == SIMULATOR_SUPERVISOR_ARTIFACT_NAME {
        Some("phoxal-simulator-webots-supervisor")
    } else if artifact_name == SIMULATOR_CONTROLLER_ARTIFACT_NAME {
        Some("phoxal-simulator-webots-controller")
    } else {
        None
    }
}

/// Obtain the cached native-artifact binary path for a CATALOG (non
/// path-overridden) simulator runtime, mirroring how
/// `commands::run::locate_official_binary` resolves every other official
/// artifact. Errors clearly rather than leaving the controller silently
/// unstaged when the artifact was never vendored into the project store.
fn provisioned_official_simulator_binary(runtime: &ResolvedPlatformRuntime) -> Result<PathBuf> {
    let descriptor = crate::native_artifacts::NativeArtifactDescriptor::from_runtime(runtime)
        .with_context(|| {
            format!(
                "failed to resolve native-artifact descriptor for simulator '{}'",
                runtime.name
            )
        })?
        .ok_or_else(|| {
            anyhow!(
                "simulator '{}' has no built native artifact for this target; run `phoxal update` or pin a path override",
                runtime.name
            )
        })?;
    let cached = crate::native_artifacts::artifact_binary_path(&descriptor).with_context(|| {
        format!(
            "failed to locate vendored simulator '{}' in the artifact store",
            runtime.name
        )
    })?;
    if !cached.is_file() {
        bail!(
            "NativePending: simulator '{}' binary is not vendored ({}); run `phoxal update` to fetch it",
            runtime.name,
            cached.display()
        );
    }
    Ok(cached)
}

/// The Webots-linked simulator crates need `WEBOTS_HOME` to build (their
/// `phoxal-api`/webots-sys build script links against the Webots controller
/// library). `build_source_binary` inherits the CLI process environment, so
/// when the live simulate flow already has `WEBOTS_HOME` set (or the caller
/// relies on the orchestrator to set it) this is a no-op; this only fills the
/// gap defensively when host_doctor can detect an install but the process
/// environment does not already carry `WEBOTS_HOME`.
fn detected_webots_home_for_build_env() -> Option<PathBuf> {
    if std::env::var_os("WEBOTS_HOME").is_some() {
        return None;
    }
    crate::host_doctor::webots_home_path().ok()
}

/// RAII guard that sets `WEBOTS_HOME` for the duration of a `build_source_binary`
/// call when the process environment does not already carry it, and restores
/// the previous (absent) state afterwards. Process env mutation is otherwise
/// unsafe to interleave with other threads; live simulate's staging runs
/// single-threaded ahead of any concurrent build, so this is scoped as
/// tightly as possible and only used when `WEBOTS_HOME` was confirmed absent.
struct WebotsHomeEnvGuard;

impl WebotsHomeEnvGuard {
    fn set(home: &Path) -> Self {
        // SAFETY: staging runs before any concurrent participant build is
        // spawned in the live-simulate path, and this guard only ever sets a
        // variable it first confirmed was absent (`detected_webots_home_for_build_env`).
        unsafe {
            std::env::set_var("WEBOTS_HOME", home);
        }
        Self
    }
}

impl Drop for WebotsHomeEnvGuard {
    fn drop(&mut self) {
        // SAFETY: see `set` - this only ever clears the variable this guard set.
        unsafe {
            std::env::remove_var("WEBOTS_HOME");
        }
    }
}

/// Stage a simulation world for the resolved robot: locate the controller and
/// supervisor `ParticipantLaunch` records already built by `build_launch_plan`
/// (Sim mode), load the robot's own structure + mounted component types, and
/// call `simulate_staging::stage_simulation_world`.
///
/// `world_source_path` is the already-resolved authored `.wbt`
/// (`world::resolve_world`, run during `prepare()`); staging copies and
/// augments it, it is never mutated in place.
pub(crate) fn stage_simulation_for_robot(
    project_root: &Path,
    world_source_path: &Path,
    resolved: &ResolvedRobot,
    launch_plan: &LaunchPlan,
) -> Result<StagedSimulationWorld> {
    // Wipe-and-restage per play: the staged root is a single, home-based
    // location shared across every `simulate` invocation (not project-scoped
    // any more), and Webots only ever runs one world per play, so a previous
    // play's stale worlds/protos/meshes/controllers must never linger. This
    // must run before any of this play's own staging below writes anything.
    webots_stage_root::wipe_and_recreate()?;

    let base_world_text = std::fs::read_to_string(world_source_path)
        .with_context(|| format!("failed to read {}", world_source_path.display()))?;
    let world_name = world_source_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .context("world source path has no file stem")?;

    let robot = launch_plan
        .robots
        .first()
        .context("sim launch plan has no robot")?;
    let robot_id = &resolved.robot.robot.id;
    let controller_id = simulator_controller_provider_id(robot_id);
    let controller_launch = robot
        .participants
        .iter()
        .find(|participant| participant.launch.participant_id == controller_id)
        .map(|participant| participant.launch.clone())
        .ok_or_else(|| {
            anyhow!("sim launch plan is missing the controller participant '{controller_id}'")
        })?;
    let supervisor_launch = robot
        .participants
        .iter()
        .find(|participant| participant.launch.participant_id == SIMULATOR_SUPERVISOR_PROVIDER_ID)
        .map(|participant| participant.launch.clone())
        .ok_or_else(|| {
            anyhow!(
                "sim launch plan is missing the supervisor participant '{SIMULATOR_SUPERVISOR_PROVIDER_ID}'"
            )
        })?;

    let structure_path = project_root.join(&resolved.robot.robot.structure);
    let structure = phoxal::model::structure::Structure::read_from_file(&structure_path)
        .with_context(|| {
            format!(
                "failed to read robot structure declared by robot.yaml structure: {}",
                resolved.robot.robot.structure.display()
            )
        })?;
    structure
        .validate()
        .context("robot structure failed validation")?;

    let mut components = BTreeMap::new();
    let mut component_type_dirs = BTreeMap::new();
    for component in &resolved.components {
        if component_type_dirs.contains_key(&component.source_name) {
            continue;
        }
        let crate_dir = component_assets_dir(component, project_root)?.ok_or_else(|| {
            anyhow!(
                "component instance '{}' (type '{}') has no resolved component_assets package; \
                 simulation needs its component.yaml/structure.urdf to stage the robot model. \
                 Passive components without an official assets package cannot be simulated yet - \
                 pin artifacts.pins.phoxal/component-{} to a path/git override that provides one.",
                component.instance,
                component.source_name,
                component.source_name
            )
        })?;
        let component_model = phoxal::model::component::Component::read_from_dir(&crate_dir)
            .with_context(|| {
                format!(
                    "failed to read component.yaml for component type '{}' from {}",
                    component.source_name,
                    crate_dir.display()
                )
            })?
            .as_v0()
            .context("Webots staging only supports component.yaml version v0")?
            .clone();
        components.insert(component.source_name.clone(), component_model);
        component_type_dirs.insert(component.source_name.clone(), crate_dir);
    }

    let bundle = phoxal::model::v0::Robot {
        manifest: resolved.robot.clone(),
        components,
        structure,
    };
    // Only stage a PROTO for component types that actually carry Webots
    // simulation data - a component with no `simulation.yaml` has nothing for
    // `generate_component_proto` to render and is not expected to be staged.
    let component_types = component_type_dirs
        .iter()
        .filter(|(_, source_dir)| {
            source_dir.join("simulation.yaml").is_file()
                || source_dir.join("simulation.yml").is_file()
        })
        .map(|(component_type, source_dir)| ComponentTypeToStage {
            component_type,
            source_dir,
        })
        .collect::<Vec<_>>();

    // `require_native` tells the supervisor whether it must resolve native
    // (packaged) controller/component artifacts rather than accepting a local
    // dev/path-overridden build; false whenever any simulator artifact is
    // path-overridden for local simulator development.
    let require_native = resolved
        .simulators
        .iter()
        .all(|runtime| runtime.source_path().is_none());

    let mesh_root = webots_stage_root::meshes_dir()?;
    // The Phase-6 mesh-staging gap: the generated PROTOs reference mesh assets
    // relative to `mesh_root` (the robot's own meshes directly under it, each
    // component's under `<mesh_root>/<component_type>/` per
    // `component_mesh_prefix`), but nothing copied the physical mesh files
    // there before this fix - the robot spawned with no visible geometry.
    // The robot's own meshes stay a real copy directly under `mesh_root`
    // (it shares that directory with every component's symlinked subdir, so
    // it cannot itself be a symlink); each component type's own `meshes/` is
    // symlinked instead - see `stage_component_meshes`.
    stage_robot_meshes(project_root, &resolved.robot.robot.structure, &mesh_root)?;
    for (component_type, source_dir) in &component_type_dirs {
        stage_component_meshes(source_dir, component_type, &mesh_root)?;
    }
    stage_simulation_world(
        &base_world_text,
        &webots_stage_root::protos_dir()?,
        &mesh_root,
        &webots_stage_root::world_path(world_name)?,
        supervisor_launch,
        require_native,
        &[RobotToStage {
            robot_id: robot_id.clone(),
            bundle: &bundle,
            component_types,
            controller_launch,
        }],
    )
}

/// The mesh source directory convention: a `meshes/` sibling of the file a
/// URDF/robot document is anchored at (the robot's own `structure.urdf` at
/// the project root, or a component's `structure.urdf` in its source dir).
const MESHES_DIR: &str = "meshes";

/// Stage the robot's own `meshes/` directory (if any) directly under
/// `mesh_root` - `WebotsSceneDescription::from_robot` renders with
/// `component_mesh_prefix: None`, so the robot's own mesh URDF references
/// (`meshes/<file>`) resolve unprefixed, one level under `mesh_root` itself.
///
/// This stays a real COPY, not a symlink: `mesh_root` also hosts every
/// mounted component type's own symlinked `<component_type>/` subdirectory
/// side by side (see `stage_component_meshes`), so `mesh_root` itself must
/// remain a real directory the robot's own files sit in directly - there is
/// no single source directory a whole-`mesh_root` symlink could point at.
fn stage_robot_meshes(project_root: &Path, structure_path: &Path, mesh_root: &Path) -> Result<()> {
    let structure_dir = project_root
        .join(structure_path)
        .parent()
        .map_or_else(|| project_root.to_path_buf(), std::path::Path::to_path_buf);
    let source = structure_dir.join(MESHES_DIR);
    if !source.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(mesh_root).with_context(|| {
        format!(
            "failed to create staged mesh directory {}",
            mesh_root.display()
        )
    })?;
    copy_dir_recursive(&source, mesh_root)
}

/// Stage one component type's `meshes/` directory (if any) as a SYMLINK at
/// `<mesh_root>/<component_type>/` pointing at the component's resolved mesh
/// source directory (the unpacked cached asset bundle's `meshes/` for a
/// catalog component, or the local `components/<id>/meshes/` for a
/// path-pinned one - both already absolute, see `component_assets_dir`) - the
/// cache/path-pin stays the single source of truth instead of a copy. The
/// prefix `WebotsSceneDescription::from_component`'s `component_mesh_prefix`
/// embeds into the component's own mesh URDF references (`meshes/<file>` ->
/// `<component_type>/<file>`, see `staged_mesh_path_from_urdf_filename`), so
/// the generated PROTOs resolve through the symlinked directory exactly as
/// they would a copied one. `mesh_root` itself must already exist (see
/// `webots_stage_root::wipe_and_recreate`) - not every robot has its own
/// meshes to trigger `stage_robot_meshes`' `create_dir_all`.
fn stage_component_meshes(source_dir: &Path, component_type: &str, mesh_root: &Path) -> Result<()> {
    let source = source_dir.join(MESHES_DIR);
    if !source.is_dir() {
        return Ok(());
    }
    require_absolute_symlink_target("component mesh source directory", &source)?;
    let dest = mesh_root.join(component_type);
    std::os::unix::fs::symlink(&source, &dest).with_context(|| {
        format!(
            "failed to symlink component meshes {} to staged path {}",
            source.display(),
            dest.display()
        )
    })
}

fn copy_dir_recursive(source: &Path, dest: &Path) -> Result<()> {
    for entry in std::fs::read_dir(source)
        .with_context(|| format!("failed to read mesh source directory {}", source.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let source_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if file_type.is_dir() {
            std::fs::create_dir_all(&dest_path).with_context(|| {
                format!(
                    "failed to create staged mesh directory {}",
                    dest_path.display()
                )
            })?;
            copy_dir_recursive(&source_path, &dest_path)?;
        } else if file_type.is_file() {
            std::fs::copy(&source_path, &dest_path).with_context(|| {
                format!(
                    "failed to stage mesh file {} to {}",
                    source_path.display(),
                    dest_path.display()
                )
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{
        ArtifactKind, SelectionChannel as CatalogChannel, fixture_catalog_for_tests,
        fixture_contract_for_tests, fixture_tool_entry_for_tests,
    };
    use crate::host_paths::test_support::ScratchPhoxalHome;
    use crate::launch_plan::{SITE_TOOL_JOYPAD, SITE_TOOL_TELEMETRY};
    use crate::resolver::{
        ResolvedComponent, ResolvedComponentSource, ResolvedPathOverride, ResolvedPathOverrideKind,
        ResolvedPlatformRuntime, ResolvedTool, ResolvedUserRuntime, host_target_triple,
    };
    use std::fs;

    /// A `LaunchMode::Webots` for tests that only care about exercising the
    /// Webots-mode participant/ownership rules, not the world path itself.
    fn webots_mode_for_tests() -> LaunchMode {
        LaunchMode::Webots {
            world: PathBuf::from("worlds/test.wbt"),
        }
    }

    #[test]
    fn webots_launch_starts_realtime_batch_mode() {
        assert_eq!(
            webots_launch_args(Path::new("/tmp/staged.wbt")),
            vec!["--mode=realtime", "--batch", "/tmp/staged.wbt"]
        );
    }

    #[test]
    fn live_simulation_locks_remain_held_until_the_setup_owner_drops() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let simulator_lock = temp.path().join("simulator.lock");
        let locks = LiveSimulationLocks::acquire(temp.path(), &simulator_lock)?;

        assert!(
            SupervisorLock::acquire(temp.path()).is_err(),
            "the run lock must remain held after setup returns"
        );
        assert!(
            SupervisorLock::acquire_path(&simulator_lock).is_err(),
            "the simulator lock must remain held after setup returns"
        );

        drop(locks);
        SupervisorLock::acquire(temp.path())?;
        SupervisorLock::acquire_path(&simulator_lock)?;
        Ok(())
    }

    #[test]
    fn spawn_responder_wire_shape_matches_additive_simulation_contract() {
        assert_eq!(
            <SpawnRequest as ContractBody>::TOPIC,
            format!(
                "{}/simulation/spawn",
                <SpawnRequest as ContractBody>::VERSION
            )
        );
        let value = serde_json::to_value(SpawnSet {
            revision: 1,
            robots: vec![RobotSpawn {
                robot_id: "robot-a".to_string(),
                node_string: "Robot { name \"robot-a\" }".to_string(),
            }],
        })
        .unwrap();

        assert_eq!(value["revision"], 1);
        assert_eq!(value["robots"][0]["robot_id"], "robot-a");
        assert_eq!(
            value["robots"][0]["node_string"],
            "Robot { name \"robot-a\" }"
        );
        assert!(value.get("spawn").is_none());
        assert!(value["robots"][0].get("name").is_none());
    }

    #[test]
    fn live_resolve_path_only_project_needs_no_lock_or_network() -> Result<()> {
        // With no lockfile, a path-only / official-only project resolves live
        // with no network for either mode: there is nothing to look up remotely
        // (no git components), so resolution succeeds and writes no lock. A
        // scratch home still isolates the process-global `PHOXAL_PROJECT_ROOT`
        // so `resolve()`'s ambient `artifacts_dir()` check can't race a
        // concurrent test's real store lock.
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let temp = tempfile::tempdir()?;
        write_robot_project(temp.path())?;

        let resolved = resolve_project(
            temp.path(),
            SimulateOptions {
                world: "test".to_string(),
                ..SimulateOptions::default()
            },
            SimulateMode::Live,
        )?;

        assert_eq!(
            resolved.resolved.channel,
            crate::catalog::SelectionChannel::Stable
        );
        assert!(resolved.resolved.components.is_empty());
        Ok(())
    }

    #[test]
    fn dry_run_resolve_path_only_project_needs_no_lock_or_network() -> Result<()> {
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let temp = tempfile::tempdir()?;
        write_robot_project(temp.path())?;

        let resolved = resolve_project(
            temp.path(),
            SimulateOptions {
                world: "test".to_string(),
                ..SimulateOptions::default()
            },
            SimulateMode::DryRun,
        )?;

        assert_eq!(
            resolved.resolved.channel,
            crate::catalog::SelectionChannel::Stable
        );
        Ok(())
    }

    #[test]
    fn no_components_sim_plan_matches_run_plan_participants() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut resolved = empty_resolved_robot("robot_v1")?;
        add_site_tools(&mut resolved);
        resolved.platform_runtimes.push(platform_runtime(
            "drive",
            vec![fixture_contract_for_tests("v1::drive::Target", "publish")],
        ));
        resolved.user_runtimes.push(ResolvedUserRuntime {
            name: "mission".to_string(),
            path: PathBuf::from("runtimes/mission"),
            source_hash: "hash".to_string(),
        });
        let extras = RobotManifestExtras::default();
        let sources = vec![SourceParticipant::user_service(
            "mission",
            temp.path().join("runtimes/mission"),
        )];
        let checked = vec![
            service_participant("drive", Vec::new()),
            service_participant("mission", Vec::new()),
        ];

        let run_plan = build_launch_plan(
            LaunchMode::Run,
            &[CheckedRobotLaunchInput {
                project_root: temp.path(),
                resolved: &resolved,
                manifest_extras: &extras,
                checked_participants: &checked,
                substitutions: &[],
                source_participants: &sources,
            }],
        )?;
        let sim_plan = build_launch_plan(
            webots_mode_for_tests(),
            &[CheckedRobotLaunchInput {
                project_root: temp.path(),
                resolved: &resolved,
                manifest_extras: &extras,
                checked_participants: &checked,
                substitutions: &[],
                source_participants: &sources,
            }],
        )?;

        assert_eq!(participant_ids(&sim_plan), participant_ids(&run_plan));
        assert!(sim_plan.robots[0].substitutions.is_empty());
        Ok(())
    }

    #[test]
    fn one_instance_substitution_is_checked_and_rendered() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let resolved = resolved_with_drive_components(&["left_drive"], false)?;
        let extras = RobotManifestExtras::default();
        let controller_id = simulator_controller_provider_id("robot_v1");
        let checked = vec![
            service_participant("drive", vec![motor_command()]),
            driver_participant("ddsm115", "left_drive", vec![motor_command()]),
            simulator_controller_participant(&controller_id, vec![motor_command()]),
        ];
        let substitutions = simulated_component_records(&checked, &controller_id);
        let sim_participants = sim_checked_participants(&checked);
        let report = graph_check::check_graph(&sim_participants);
        assert!(report.is_ok(), "{report:?}");

        let plan = build_launch_plan(
            webots_mode_for_tests(),
            &[CheckedRobotLaunchInput {
                project_root: temp.path(),
                resolved: &resolved,
                manifest_extras: &extras,
                checked_participants: &sim_participants,
                substitutions: &substitutions,
                source_participants: &[],
            }],
        )?;

        assert_eq!(
            substitution_lines(&plan),
            vec![format!(
                "component/left_drive/* : satisfied by {controller_id} (webots-controller)"
            )]
        );
        // The controller is a SIMULATION-MANAGED robot launch participant: it
        // appears here for board presence + controllerArgs rendering, but
        // never gets a CLI-spawned process (Part 5).
        assert_eq!(
            participant_ids(&plan),
            vec!["drive", controller_id.as_str()]
        );
        Ok(())
    }

    #[test]
    fn two_identical_instances_get_disjoint_substitution_sets() {
        let controller_id = simulator_controller_provider_id("robot_v1");
        let checked = vec![
            service_participant("drive", vec![motor_command()]),
            driver_participant("ddsm115", "left_drive", vec![motor_command()]),
            driver_participant("ddsm115", "right_drive", vec![motor_command()]),
            simulator_controller_participant(&controller_id, vec![motor_command()]),
        ];
        // Board display only (no checker involved, see module docs): each
        // dropped driver instance gets its own disjoint substitution record.
        let substitutions = simulated_component_records(&checked, &controller_id);
        let instances = substitutions
            .iter()
            .map(|substitution| substitution.component_instance.as_str())
            .collect::<Vec<_>>();
        assert_eq!(instances, vec!["left_drive", "right_drive"]);
    }

    #[test]
    fn supervisor_and_controller_get_distinct_stable_provider_ids() {
        // Part 1 acceptance: the supervisor is world-scoped and stable; the
        // controller is robot-scoped; the two never collide.
        let supervisor_id = simulator_participant_id_for_resolved_artifact(
            SIMULATOR_SUPERVISOR_ARTIFACT_NAME,
            "robot_v1",
        )
        .expect("supervisor artifact name should map to an id");
        let controller_id = simulator_participant_id_for_resolved_artifact(
            SIMULATOR_CONTROLLER_ARTIFACT_NAME,
            "robot_v1",
        )
        .expect("controller artifact name should map to an id");

        assert_eq!(supervisor_id, SIMULATOR_SUPERVISOR_PROVIDER_ID);
        assert_eq!(controller_id, "simulator-webots-controller-robot_v1");
        assert_ne!(supervisor_id, controller_id);

        // The supervisor id is stable across robots (world-scoped); the
        // controller id is not (robot-scoped), so it generalizes to a
        // multi-robot plan without id collisions.
        let supervisor_id_other_robot = simulator_participant_id_for_resolved_artifact(
            SIMULATOR_SUPERVISOR_ARTIFACT_NAME,
            "robot_v2",
        )
        .expect("supervisor artifact name should map to an id");
        let controller_id_other_robot = simulator_participant_id_for_resolved_artifact(
            SIMULATOR_CONTROLLER_ARTIFACT_NAME,
            "robot_v2",
        )
        .expect("controller artifact name should map to an id");
        assert_eq!(supervisor_id, supervisor_id_other_robot);
        assert_ne!(controller_id, controller_id_other_robot);
    }

    #[test]
    fn path_overridden_simulators_use_the_same_provider_ids_as_official() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut resolved = resolved_with_drive_components(&[], false)?;
        resolved.simulators.clear();

        let supervisor_path = temp.path().join("framework/simulator/webots-supervisor");
        let controller_path = temp.path().join("framework/simulator/webots-controller");
        let mut supervisor = simulator_runtime(SIMULATOR_SUPERVISOR_ARTIFACT_NAME);
        supervisor.path_override = Some(supervisor_path.clone());
        supervisor.artifact_ref = format!("path:{}", supervisor_path.display());
        let mut controller = simulator_runtime(SIMULATOR_CONTROLLER_ARTIFACT_NAME);
        controller.path_override = Some(controller_path.clone());
        controller.artifact_ref = format!("path:{}", controller_path.display());
        resolved.simulators.extend([supervisor, controller]);
        resolved.path_overrides = vec![
            ResolvedPathOverride {
                key: "simulator-webots-supervisor".to_string(),
                kind: ResolvedPathOverrideKind::Simulator,
                artifact_name: SIMULATOR_SUPERVISOR_ARTIFACT_NAME.to_string(),
                path: supervisor_path.clone(),
            },
            ResolvedPathOverride {
                key: "simulator-webots-controller".to_string(),
                kind: ResolvedPathOverrideKind::Simulator,
                artifact_name: SIMULATOR_CONTROLLER_ARTIFACT_NAME.to_string(),
                path: controller_path.clone(),
            },
        ];

        let supervisor_id = SIMULATOR_SUPERVISOR_PROVIDER_ID.to_string();
        let controller_id = simulator_controller_provider_id("robot_v1");
        let mut checked = vec![
            service_participant("drive", Vec::new()),
            graph_check::ParticipantApis {
                participant_id: SIMULATOR_SUPERVISOR_ARTIFACT_NAME.to_string(),
                artifact_id: SIMULATOR_SUPERVISOR_ARTIFACT_NAME.to_string(),
                participant_kind: graph_check::ParticipantKind::Simulator,
                participant_class: graph_check::ParticipantClass::Checked,
                api_version: "v1".to_string(),
                config_schema: None,
                scope: graph_check::ParticipantScope::Graph,
                contracts: Vec::new(),
            },
            simulator_controller_participant(SIMULATOR_CONTROLLER_ARTIFACT_NAME, Vec::new()),
        ];
        remap_simulator_participant_ids(&mut checked, &resolved.robot.robot.id)?;

        assert!(checked.iter().any(|participant| {
            participant.participant_id == supervisor_id
                && participant.artifact_id == SIMULATOR_SUPERVISOR_ARTIFACT_NAME
        }));
        assert!(checked.iter().any(|participant| {
            participant.participant_id == controller_id
                && participant.artifact_id == SIMULATOR_CONTROLLER_ARTIFACT_NAME
        }));

        let extras = RobotManifestExtras::default();
        let sources = sim_source_participants(temp.path(), &resolved, None)?;
        let plan = build_launch_plan(
            webots_mode_for_tests(),
            &[CheckedRobotLaunchInput {
                project_root: temp.path(),
                resolved: &resolved,
                manifest_extras: &extras,
                checked_participants: &checked,
                substitutions: &[],
                source_participants: &sources,
            }],
        )?;

        assert_eq!(
            participant_ids(&plan),
            vec!["drive", controller_id.as_str(), supervisor_id.as_str()]
        );
        for id in [&controller_id, &supervisor_id] {
            let participant = plan.robots[0]
                .participants
                .iter()
                .find(|participant| participant.launch.participant_id == *id)
                .expect("simulator participant should be present");
            assert_eq!(
                participant.launch_ownership,
                crate::launch_plan::LaunchOwnership::SimulationManaged
            );
            assert!(matches!(
                &participant.execution,
                crate::launch_plan::ParticipantExecution::SourceArtifact { .. }
            ));
        }

        let artifact_lines = simulator_artifact_lines(&resolved);
        assert_eq!(artifact_lines.len(), 2);
        assert!(
            artifact_lines
                .iter()
                .any(|line| line.contains(&supervisor_id))
        );
        assert!(
            artifact_lines
                .iter()
                .any(|line| line.contains(&controller_id))
        );

        Ok(())
    }

    #[test]
    fn sim_launch_set_matches_checked_robot_participants_without_drivers() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let resolved = resolved_with_drive_components(&["left_drive"], true)?;
        let extras = RobotManifestExtras::default();
        let controller_id = simulator_controller_provider_id("robot_v1");
        let checked = vec![
            service_participant("drive", Vec::new()),
            service_participant("mission", Vec::new()),
            driver_participant("ddsm115", "left_drive", vec![motor_command()]),
            simulator_controller_participant(&controller_id, vec![motor_command()]),
        ];
        let substitutions = simulated_component_records(&checked, &controller_id);
        let sources = vec![SourceParticipant::user_service(
            "mission",
            temp.path().join("runtimes/mission"),
        )];
        let sim_participants = sim_checked_participants(&checked);
        let plan = build_launch_plan(
            webots_mode_for_tests(),
            &[CheckedRobotLaunchInput {
                project_root: temp.path(),
                resolved: &resolved,
                manifest_extras: &extras,
                checked_participants: &sim_participants,
                substitutions: &substitutions,
                source_participants: &sources,
            }],
        )?;

        assert_eq!(
            participant_ids(&plan),
            vec!["drive", "mission", controller_id.as_str()]
        );
        assert_eq!(
            plan.robots[0]
                .substitutions
                .iter()
                .map(|substitution| substitution.component_instance.as_str())
                .collect::<Vec<_>>(),
            vec!["left_drive"]
        );
        Ok(())
    }

    #[test]
    fn sim_plan_carries_both_supervisor_and_controller_under_distinct_ids() -> Result<()> {
        // Part 1 acceptance (full): a sim launch plan for a robot with >=1
        // component driver has (a) the supervisor present under the
        // world-scoped id, (b) the driver substitution's
        // provider_participant_id is the controller id, and (c) supervisor id
        // != controller id.
        let temp = tempfile::tempdir()?;
        let mut resolved = resolved_with_drive_components(&["left_drive"], false)?;
        resolved
            .simulators
            .push(simulator_runtime(SIMULATOR_SUPERVISOR_ARTIFACT_NAME));
        let extras = RobotManifestExtras::default();
        let supervisor_id = SIMULATOR_SUPERVISOR_PROVIDER_ID.to_string();
        let controller_id = simulator_controller_provider_id("robot_v1");
        let checked = vec![
            service_participant("drive", vec![motor_command()]),
            driver_participant("ddsm115", "left_drive", vec![motor_command()]),
            graph_check::ParticipantApis {
                participant_id: supervisor_id.clone(),
                artifact_id: "webots-supervisor".to_string(),
                participant_kind: graph_check::ParticipantKind::Simulator,
                participant_class: graph_check::ParticipantClass::Checked,
                api_version: "v1".to_string(),
                config_schema: None,
                scope: graph_check::ParticipantScope::Graph,
                contracts: Vec::new(),
            },
            simulator_controller_participant(&controller_id, vec![motor_command()]),
        ];
        let substitutions = simulated_component_records(&checked, &controller_id);
        let sim_participants = sim_checked_participants(&checked);
        let report = graph_check::check_graph(&sim_participants);
        assert!(report.is_ok(), "{report:?}");

        let plan = build_launch_plan(
            webots_mode_for_tests(),
            &[CheckedRobotLaunchInput {
                project_root: temp.path(),
                resolved: &resolved,
                manifest_extras: &extras,
                checked_participants: &sim_participants,
                substitutions: &substitutions,
                source_participants: &[],
            }],
        )?;

        // (a) the supervisor is present under the world-scoped id.
        assert!(participant_ids(&plan).contains(&supervisor_id.as_str()));
        // (b) the driver substitution's provider is the controller id.
        assert_eq!(
            plan.robots[0].substitutions[0].provider_participant_id,
            controller_id
        );
        // (c) supervisor id != controller id.
        assert_ne!(supervisor_id, controller_id);
        Ok(())
    }

    #[test]
    fn stage_simulation_for_robot_produces_a_webots_free_staged_world() -> Result<()> {
        // The Part 5 Live-path plumbing (`stage_and_prepare_webots_spec` ->
        // `stage_simulation_for_robot`) end to end, without Webots: given a
        // sim LaunchPlan carrying both the supervisor and controller
        // participants, staging must write a `.wbt` declaring the robot's
        // EXTERNPROTO and the supervisor's static node, with no static robot
        // instance node.
        //
        // Staging lands under `<project>/.phoxal/webots`, so this test selects
        // a scratch project root rather than touching the working project.
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let temp = tempfile::tempdir()?;
        fs::write(
            temp.path().join("structure.urdf"),
            r#"<robot name="testbot"><link name="base_footprint"/><link name="base_link"/><joint name="base_joint" type="fixed"><parent link="base_footprint"/><child link="base_link"/></joint></robot>"#,
        )?;
        fs::create_dir_all(temp.path().join("worlds"))?;
        let world_source_path = temp.path().join("worlds/default.wbt");
        fs::write(
            &world_source_path,
            "#VRML_SIM R2023b utf8\n\nWorldInfo {\n}\n",
        )?;
        write_driver_crate(temp.path(), "ddsm115")?;
        fs::write(
            temp.path().join("components/ddsm115/component.yaml"),
            "schema: component/v0\ncapabilities: {}\n",
        )?;

        let mut resolved = resolved_with_drive_components(&["left_drive"], false)?;
        resolved
            .simulators
            .push(simulator_runtime(SIMULATOR_SUPERVISOR_ARTIFACT_NAME));
        let extras = RobotManifestExtras::default();
        let supervisor_id = SIMULATOR_SUPERVISOR_PROVIDER_ID.to_string();
        let controller_id = simulator_controller_provider_id("robot_v1");
        let checked = vec![
            service_participant("drive", vec![motor_command()]),
            driver_participant("ddsm115", "left_drive", vec![motor_command()]),
            graph_check::ParticipantApis {
                participant_id: supervisor_id.clone(),
                artifact_id: "webots-supervisor".to_string(),
                participant_kind: graph_check::ParticipantKind::Simulator,
                participant_class: graph_check::ParticipantClass::Checked,
                api_version: "v1".to_string(),
                config_schema: None,
                scope: graph_check::ParticipantScope::Graph,
                contracts: Vec::new(),
            },
            simulator_controller_participant(&controller_id, vec![motor_command()]),
        ];
        let substitutions = simulated_component_records(&checked, &controller_id);
        let sim_participants = sim_checked_participants(&checked);
        let report = graph_check::check_graph(&sim_participants);
        assert!(report.is_ok(), "{report:?}");

        let plan = build_launch_plan(
            webots_mode_for_tests(),
            &[CheckedRobotLaunchInput {
                project_root: temp.path(),
                resolved: &resolved,
                manifest_extras: &extras,
                checked_participants: &sim_participants,
                substitutions: &substitutions,
                source_participants: &[],
            }],
        )?;

        let staged = stage_simulation_for_robot(temp.path(), &world_source_path, &resolved, &plan)?;

        let staged_text = std::fs::read_to_string(&staged.staged_world_path)?;
        assert!(
            staged_text.contains("EXTERNPROTO"),
            "staged world should declare the robot's EXTERNPROTO:\n{staged_text}"
        );
        assert!(
            staged_text.contains("supervisor TRUE"),
            "staged world should contain the supervisor node:\n{staged_text}"
        );
        assert!(
            staged_text.contains("phoxal-simulator-webots-supervisor"),
            "staged world should name the supervisor controller:\n{staged_text}"
        );
        assert_eq!(
            staged_text.matches("Robot {").count(),
            1,
            "the staged world should contain exactly one root Robot node (the supervisor)"
        );
        assert_eq!(staged.spawn_descriptors.len(), 1);
        assert_eq!(staged.spawn_descriptors[0].robot_id, "robot_v1");
        assert!(
            staged.spawn_descriptors[0]
                .node_string
                .contains("phoxal-simulator-webots-controller")
        );

        Ok(())
    }

    #[test]
    fn stage_simulation_for_robot_writes_world_with_supervisor_and_no_static_robot() -> Result<()> {
        // Part 5 acceptance: the Live path's staging helper produces a real
        // staged .wbt on disk with the supervisor node and no static robot
        // instance node, using the controller/supervisor ParticipantLaunch
        // records already carried by the Sim LaunchPlan.
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let temp = tempfile::tempdir()?;
        fs::write(
            temp.path().join("structure.urdf"),
            r#"<robot name="testbot"><link name="base_footprint"/><link name="base_link"/><joint name="base_joint" type="fixed"><parent link="base_footprint"/><child link="base_link"/></joint></robot>"#,
        )?;
        fs::create_dir_all(temp.path().join("worlds"))?;
        let world_source = temp.path().join("worlds/test.wbt");
        fs::write(&world_source, "#VRML_SIM R2023b utf8\n\nWorldInfo {\n}\n")?;

        let mut resolved = resolved_with_drive_components(&[], false)?;
        resolved
            .simulators
            .push(simulator_runtime(SIMULATOR_SUPERVISOR_ARTIFACT_NAME));
        resolved
            .simulators
            .push(simulator_runtime(SIMULATOR_CONTROLLER_ARTIFACT_NAME));
        let extras = RobotManifestExtras::default();
        let supervisor_id = SIMULATOR_SUPERVISOR_PROVIDER_ID.to_string();
        let controller_id = simulator_controller_provider_id("robot_v1");
        let checked = vec![
            service_participant("drive", vec![motor_command()]),
            graph_check::ParticipantApis {
                participant_id: supervisor_id.clone(),
                artifact_id: "webots-supervisor".to_string(),
                participant_kind: graph_check::ParticipantKind::Simulator,
                participant_class: graph_check::ParticipantClass::Checked,
                api_version: "v1".to_string(),
                config_schema: None,
                scope: graph_check::ParticipantScope::Graph,
                contracts: Vec::new(),
            },
            graph_check::ParticipantApis {
                participant_id: controller_id.clone(),
                artifact_id: "webots-controller".to_string(),
                participant_kind: graph_check::ParticipantKind::Simulator,
                participant_class: graph_check::ParticipantClass::Checked,
                api_version: "v1".to_string(),
                config_schema: None,
                scope: graph_check::ParticipantScope::Graph,
                contracts: Vec::new(),
            },
        ];
        let sim_participants = sim_checked_participants(&checked);

        let plan = build_launch_plan(
            webots_mode_for_tests(),
            &[CheckedRobotLaunchInput {
                project_root: temp.path(),
                resolved: &resolved,
                manifest_extras: &extras,
                checked_participants: &sim_participants,
                substitutions: &[],
                source_participants: &[],
            }],
        )?;

        let staged = stage_simulation_for_robot(temp.path(), &world_source, &resolved, &plan)?;

        assert!(staged.staged_world_path.is_file());
        let staged_text = fs::read_to_string(&staged.staged_world_path)?;
        assert!(
            staged_text.contains("supervisor TRUE"),
            "staged world should contain the supervisor node:\n{staged_text}"
        );
        assert!(
            staged_text.contains("phoxal-simulator-webots-supervisor"),
            "staged world should name the supervisor controller:\n{staged_text}"
        );
        assert_eq!(
            staged_text.matches("Robot {").count(),
            1,
            "the staged world should have exactly one root Robot node (the supervisor):\n{staged_text}"
        );
        assert_eq!(staged.controller_launches.len(), 1);
        assert_eq!(staged.controller_launches[0].0, "robot_v1");
        Ok(())
    }

    #[test]
    fn dry_run_output_shows_webots_supervisor_controller_and_ownership() -> Result<()> {
        // Part 6 acceptance: `simulate --dry-run` must show, explicitly, the
        // Webots app as the CLI-managed child, both simulator artifacts
        // (supervisor + controller) with their ids, and each simulator
        // participant's SIMULATION-MANAGED ownership + the staged world path
        // - without staging or launching anything.
        let temp = tempfile::tempdir()?;
        // `resolved_with_drive_components` already registers a controller
        // simulator runtime; add only the supervisor to get exactly one of
        // each.
        let mut resolved = resolved_with_drive_components(&[], false)?;
        resolved
            .simulators
            .push(simulator_runtime(SIMULATOR_SUPERVISOR_ARTIFACT_NAME));
        let extras = RobotManifestExtras::default();
        let supervisor_id = SIMULATOR_SUPERVISOR_PROVIDER_ID.to_string();
        let controller_id = simulator_controller_provider_id("robot_v1");
        let checked = vec![
            service_participant("drive", vec![motor_command()]),
            graph_check::ParticipantApis {
                participant_id: supervisor_id.clone(),
                artifact_id: "webots-supervisor".to_string(),
                participant_kind: graph_check::ParticipantKind::Simulator,
                participant_class: graph_check::ParticipantClass::Checked,
                api_version: "v1".to_string(),
                config_schema: None,
                scope: graph_check::ParticipantScope::Graph,
                contracts: Vec::new(),
            },
            graph_check::ParticipantApis {
                participant_id: controller_id.clone(),
                artifact_id: "webots-controller".to_string(),
                participant_kind: graph_check::ParticipantKind::Simulator,
                participant_class: graph_check::ParticipantClass::Checked,
                api_version: "v1".to_string(),
                config_schema: None,
                scope: graph_check::ParticipantScope::Graph,
                contracts: Vec::new(),
            },
        ];
        let sim_participants = sim_checked_participants(&checked);

        let launch_plan = build_launch_plan(
            webots_mode_for_tests(),
            &[CheckedRobotLaunchInput {
                project_root: temp.path(),
                resolved: &resolved,
                manifest_extras: &extras,
                checked_participants: &sim_participants,
                substitutions: &[],
                source_participants: &[],
            }],
        )?;

        let plan = SimPlan {
            plan: launch_plan,
            ctx: PlanContext {
                robot_path: temp.path().join("robot.yaml"),
                project_root: temp.path().to_path_buf(),
                resolved,
                source_participants: Vec::new(),
            },
            runtime_store: crate::stores::runtime_store::RuntimeStore::new(),
        };

        let output = build_dry_run_output(&plan);

        assert_eq!(output.webots_app.site_id, WEBOTS_SITE_ID);
        assert_eq!(output.webots_app.launch_ownership, "cli_managed");
        assert!(
            output
                .webots_app
                .intended_staged_world_path
                .to_string_lossy()
                .contains("test.wbt")
        );

        assert_eq!(output.simulator_artifacts.len(), 2);
        assert!(
            output
                .simulator_artifacts
                .iter()
                .any(|line| line.contains("webots-supervisor") && line.contains(&supervisor_id))
        );
        assert!(
            output
                .simulator_artifacts
                .iter()
                .any(|line| line.contains("webots-controller") && line.contains(&controller_id))
        );

        assert_eq!(output.simulation_managed_participants.len(), 2);
        assert!(
            output
                .simulation_managed_participants
                .iter()
                .any(|line| line.contains(&supervisor_id))
        );
        assert!(
            output
                .simulation_managed_participants
                .iter()
                .any(|line| line.contains(&controller_id))
        );

        Ok(())
    }

    #[test]
    fn real_sim_plan_with_component_names_missing_simulator_provider_still_succeeds() -> Result<()>
    {
        // phoxal 0.28 dropped the substitution-completeness gate entirely
        // (see `phoxal::check` module docs): whether a simulator provider is
        // actually present is a caller/deployment choice, not something the
        // checker judges. So a sim plan with a component driver but no
        // resolved simulator must still succeed - and the board still labels
        // the driver's component as "simulated by" the controller, because
        // that label is a CLI-side display fact computed from the plan, not
        // a checked one.
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let temp = tempfile::tempdir()?;
        write_robot_project_with_component(temp.path())?;
        let catalog_path = write_catalog_with_driver(temp.path())?;

        let plan = prepare(
            temp.path(),
            SimulateOptions {
                world: "test".to_string(),
                catalog_source: Some(catalog_path.display().to_string()),
                overlays: vec!["dev".to_string()],
                ..SimulateOptions::default()
            },
        )?;

        assert_eq!(
            plan.plan.robots[0]
                .substitutions
                .iter()
                .map(|substitution| substitution.component_instance.as_str())
                .collect::<Vec<_>>(),
            vec!["left_drive"]
        );
        assert_eq!(
            plan.plan.robots[0].substitutions[0].provider_participant_id,
            "simulator-webots-controller-testbot"
        );
        Ok(())
    }

    #[test]
    fn custom_driver_metadata_unavailable_is_named() -> Result<()> {
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let temp = tempfile::tempdir()?;
        write_robot_project_with_custom_component(temp.path())?;
        let catalog_path = write_catalog_with_driver(temp.path())?;

        let error = prepare(
            temp.path(),
            SimulateOptions {
                world: "test".to_string(),
                catalog_source: Some(catalog_path.display().to_string()),
                overlays: vec!["dev".to_string()],
                ..SimulateOptions::default()
            },
        )
        .expect_err("custom driver that cannot build host-side should fail");
        let message = format!("{error:#}");
        assert!(message.contains("DriverMetadataUnavailable"), "{message}");
        assert!(message.contains("ddsm115"), "{message}");
        assert!(message.contains("cfg(target_os = \"linux\")"), "{message}");
        assert!(message.contains("inlined driver metadata"), "{message}");
        Ok(())
    }

    fn write_robot_project(root: &Path) -> Result<()> {
        fs::write(root.join("robot.yaml"), minimal_robot_yaml())?;
        write_catalog_with_site_tools(root)?;
        fs::write(
            root.join("structure.urdf"),
            r#"<robot name="testbot"><link name="base_footprint"/><link name="base_link"/><joint name="base_joint" type="fixed"><parent link="base_footprint"/><child link="base_link"/></joint></robot>"#,
        )?;
        fs::create_dir_all(root.join("worlds"))?;
        fs::write(
            root.join("worlds/test.wbt"),
            "#VRML_SIM R2023b utf8\n\nWorldInfo {\n}\n",
        )?;
        Ok(())
    }

    fn write_robot_project_with_custom_component(root: &Path) -> Result<()> {
        fs::write(root.join("robot.yaml"), robot_yaml_with_custom_component())?;
        fs::write(
            root.join("robot.dev.yaml"),
            robot_yaml_with_component_dev_overlay(),
        )?;
        write_catalog_with_site_tools(root)?;
        fs::write(
            root.join("structure.urdf"),
            r#"<robot name="testbot"><link name="base_footprint"/><link name="base_link"/><joint name="base_joint" type="fixed"><parent link="base_footprint"/><child link="base_link"/></joint></robot>"#,
        )?;
        fs::create_dir_all(root.join("worlds"))?;
        fs::write(
            root.join("worlds/test.wbt"),
            "#VRML_SIM R2023b utf8\n\nWorldInfo {\n}\n",
        )?;
        Ok(())
    }

    fn minimal_robot_yaml() -> &'static str {
        r#"schema: robot/v0

robot:
  id: testbot
  namespace: test
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  structure: structure.urdf
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
  components: {}

artifacts:
  channel: stable
  catalog: catalog.json
"#
    }

    fn write_robot_project_with_component(root: &Path) -> Result<()> {
        fs::write(root.join("robot.yaml"), robot_yaml_with_component())?;
        fs::write(
            root.join("robot.dev.yaml"),
            robot_yaml_with_component_dev_overlay(),
        )?;
        write_catalog_with_site_tools(root)?;
        write_driver_crate(root, "ddsm115")?;
        fs::write(
            root.join("structure.urdf"),
            r#"<robot name="testbot"><link name="base_footprint"/><link name="base_link"/><joint name="base_joint" type="fixed"><parent link="base_footprint"/><child link="base_link"/></joint></robot>"#,
        )?;
        fs::create_dir_all(root.join("worlds"))?;
        fs::write(
            root.join("worlds/test.wbt"),
            "#VRML_SIM R2023b utf8\n\nWorldInfo {\n}\n",
        )?;
        Ok(())
    }

    fn robot_yaml_with_component() -> &'static str {
        r#"schema: robot/v0

robot:
  id: testbot
  namespace: test
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  structure: structure.urdf
  kinematic:
    kind: omnidirectional
    actuators: [left_drive.motor]
    encoders: []
  components:
    left_drive:
      component: ddsm115
      mount_link: left_wheel
      parameters:
        motor:
          kind: motor
      driver:
        connection: { type: can, bus: 0, node_id: 1 }

artifacts:
  channel: stable
  catalog: catalog.json
"#
    }

    /// Path pins are dev-overlay-only; `write_robot_project_with_component`
    /// pairs the base `robot.yaml` above with this overlay (loaded via
    /// `SimulateOptions.overlays: vec!["dev".into()]`).
    fn robot_yaml_with_component_dev_overlay() -> &'static str {
        r#"artifacts:
  pins:
    phoxal/component-ddsm115:
      path: components/ddsm115
"#
    }

    fn robot_yaml_with_custom_component() -> &'static str {
        robot_yaml_with_component()
    }

    fn write_catalog_with_driver(root: &Path) -> Result<PathBuf> {
        write_catalog_with_site_tools(root)
    }

    fn write_catalog_with_site_tools(root: &Path) -> Result<PathBuf> {
        let path = root.join("catalog.json");
        let catalog = fixture_catalog_for_tests(vec![
            fixture_tool_entry_for_tests(
                "router",
                "0.1.0",
                CatalogChannel::Stable,
                &host_target_triple(),
                false,
                Vec::new(),
            ),
            fixture_tool_entry_for_tests(
                "joypad",
                "0.1.0",
                CatalogChannel::Stable,
                &host_target_triple(),
                false,
                Vec::new(),
            ),
        ]);
        fs::write(&path, serde_json::to_string_pretty(&catalog)?)?;
        Ok(path)
    }

    /// Writes a fake component-driver crate whose binary carries a
    /// hand-rolled participant-metadata linker section - not by
    /// depending on `phoxal`/`phoxal-macros` (too heavy for a per-test throwaway
    /// crate), but by placing the exact same JSON bytes
    /// `phoxal-macros` would emit directly under a manually-attributed
    /// `#[link_section]` static. `build_emit_apis_from_source` extracts
    /// contracts straight from the built binary's object-file section (the
    /// `emit-apis` runtime subcommand this used to fake via stdout is gone),
    /// so the fixture fakes the section instead of the subprocess output.
    fn write_driver_crate(root: &Path, name: &str) -> Result<()> {
        let dir = root.join("components").join(name);
        fs::create_dir_all(dir.join("src"))?;
        fs::write(
            dir.join("Cargo.toml"),
            format!(
                "[package]\nname = \"driver-{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"
            ),
        )?;
        let json = r#"{"participant_api":"Api","contracts":[{"role":"subscribe","version":"v1","contract":"component::MotorCommand","external":false}],"config_schema":{"type":"null"}}"#;
        let len = json.len();
        fs::write(
            dir.join("src/main.rs"),
            format!(
                "#[used]\n#[cfg_attr(target_os = \"macos\", unsafe(link_section = \"__DATA,__phoxal_meta\"))]\n#[cfg_attr(not(target_os = \"macos\"), unsafe(link_section = \".phoxal_api_meta\"))]\nstatic PHOXAL_API_META: [u8; {len}] = *b{json:?};\n\nfn main() {{}}\n"
            ),
        )?;
        Ok(())
    }

    fn participant_ids(plan: &LaunchPlan) -> Vec<&str> {
        plan.robots[0]
            .participants
            .iter()
            .map(|participant| participant.launch.participant_id.as_str())
            .collect()
    }

    fn service_participant(
        id: &str,
        contracts: Vec<graph_check::Contract>,
    ) -> graph_check::ParticipantApis {
        graph_check::ParticipantApis {
            participant_id: id.to_string(),
            artifact_id: id.to_string(),
            participant_kind: graph_check::ParticipantKind::Service,
            participant_class: graph_check::ParticipantClass::Checked,
            api_version: "v1".to_string(),
            config_schema: None,
            scope: graph_check::ParticipantScope::Graph,
            contracts,
        }
    }

    fn driver_participant(
        artifact_id: &str,
        instance: &str,
        contracts: Vec<graph_check::Contract>,
    ) -> graph_check::ParticipantApis {
        graph_check::ParticipantApis {
            participant_id: instance.to_string(),
            artifact_id: artifact_id.to_string(),
            participant_kind: graph_check::ParticipantKind::Driver,
            participant_class: graph_check::ParticipantClass::Checked,
            api_version: "v1".to_string(),
            config_schema: None,
            scope: graph_check::ParticipantScope::ComponentInstance(instance.to_string()),
            contracts,
        }
    }

    fn simulator_controller_participant(
        provider_participant_id: &str,
        contracts: Vec<graph_check::Contract>,
    ) -> graph_check::ParticipantApis {
        graph_check::ParticipantApis {
            participant_id: provider_participant_id.to_string(),
            artifact_id: "webots-controller".to_string(),
            participant_kind: graph_check::ParticipantKind::Simulator,
            participant_class: graph_check::ParticipantClass::Checked,
            api_version: "v1".to_string(),
            config_schema: None,
            scope: graph_check::ParticipantScope::Graph,
            contracts,
        }
    }

    fn motor_command() -> graph_check::Contract {
        graph_check::Contract {
            family: "component::MotorCommand".to_string(),
        }
    }

    fn empty_resolved_robot(id: &str) -> Result<ResolvedRobot> {
        let yaml = format!(
            r#"schema: robot/v0
robot:
  id: {id}
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  structure: structure.urdf
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
  components: {{}}
"#
        );
        let robot = phoxal::model::robot::v0::Robot::parse_from_string(&yaml)?;
        Ok(ResolvedRobot {
            robot,
            channel: crate::catalog::SelectionChannel::Stable,
            target: host_target_triple(),
            catalog_snapshot: None,
            platform_runtimes: Vec::new(),
            simulators: Vec::new(),
            user_runtimes: Vec::new(),
            components: Vec::new(),
            tools: Vec::new(),
            path_overrides: Vec::new(),
        })
    }

    fn resolved_with_drive_components(
        instances: &[&str],
        include_user: bool,
    ) -> Result<ResolvedRobot> {
        let mut resolved = empty_resolved_robot("robot_v1")?;
        add_site_tools(&mut resolved);
        resolved
            .platform_runtimes
            .push(platform_runtime("drive", Vec::new()));
        resolved
            .simulators
            .push(simulator_runtime(SIMULATOR_CONTROLLER_ARTIFACT_NAME));
        if include_user {
            resolved.user_runtimes.push(ResolvedUserRuntime {
                name: "mission".to_string(),
                path: PathBuf::from("runtimes/mission"),
                source_hash: "hash".to_string(),
            });
        }
        for instance in instances {
            resolved.components.push(ResolvedComponent {
                instance: (*instance).to_string(),
                source_name: "ddsm115".to_string(),
                assets: Some(crate::resolver::ResolvedComponentPackage {
                    package: "phoxal/component-ddsm115".to_string(),
                    kind: crate::catalog::ArtifactKind::ComponentAssets,
                    source: ResolvedComponentSource::Path {
                        path: PathBuf::from("components/ddsm115"),
                    },
                    path_override: None,
                    catalog_runtime: None,
                }),
                driver: Some(crate::resolver::ResolvedComponentPackage {
                    package: "phoxal/component-ddsm115".to_string(),
                    kind: crate::catalog::ArtifactKind::ComponentDriver,
                    source: ResolvedComponentSource::Path {
                        path: PathBuf::from("components/ddsm115"),
                    },
                    path_override: None,
                    catalog_runtime: None,
                }),
                has_driver: true,
            });
        }
        Ok(resolved)
    }

    fn platform_runtime(name: &str, _contracts: Vec<()>) -> ResolvedPlatformRuntime {
        ResolvedPlatformRuntime {
            name: name.to_string(),
            package: format!("phoxal/service-{name}"),
            kind: ArtifactKind::Service,
            version: "0.1.0".to_string(),
            artifact_ref: format!("service-{name}:0.1.0-v1-stable-{}", host_target_triple()),
            sha256: None,
            url: None,
            size: None,
            published: false,
            published_triples: Vec::new(),
            path_override: None,
            channel: crate::catalog::SelectionChannel::Stable,
            target: Some(host_target_triple()),
        }
    }

    fn simulator_runtime(name: &str) -> ResolvedPlatformRuntime {
        ResolvedPlatformRuntime {
            name: name.to_string(),
            package: format!("phoxal/simulator-{name}"),
            kind: ArtifactKind::Simulator,
            version: "0.1.0".to_string(),
            artifact_ref: format!("simulator-{name}:0.1.0-v1-stable-{}", host_target_triple()),
            sha256: None,
            url: None,
            size: None,
            published: false,
            published_triples: Vec::new(),
            path_override: None,
            channel: crate::catalog::SelectionChannel::Stable,
            target: Some(host_target_triple()),
        }
    }

    fn add_site_tools(resolved: &mut ResolvedRobot) {
        resolved.tools.push(tool(crate::launch_plan::SITE_TOOL_BUS));
        resolved.tools.push(tool(SITE_TOOL_JOYPAD));
        resolved.tools.push(tool(SITE_TOOL_TELEMETRY));
    }

    fn tool(name: &str) -> ResolvedTool {
        ResolvedTool {
            kind: crate::catalog::ArtifactKind::Tool,
            name: name.to_string(),
            package: format!("phoxal/{name}"),
            requested: "0.1.0".to_string(),
            resolved: "0.1.0".to_string(),
            repo: "phoxal/framework".to_string(),
            asset: format!("{name}-0.1.0-{}.tar.gz", host_target_triple()),
            binary_name: name.to_string(),
            sha256: "0".repeat(64),
            url: None,
            size: None,
            published: false,
            path_override: None,
            channel: crate::catalog::SelectionChannel::Stable,
            target: host_target_triple(),
        }
    }

    /// Write a minimal, fast-building binary crate at `dir` whose package and
    /// `[[bin]]` name is `bin_name` - stands in for the real
    /// `phoxal-simulator-webots-{supervisor,controller}` crates without
    /// depending on the framework workspace or Webots being installed, so
    /// this test runs in CI regardless of host Webots availability.
    fn write_fake_simulator_crate(dir: &Path, bin_name: &str) -> Result<()> {
        fs::create_dir_all(dir.join("src"))?;
        fs::write(
            dir.join("Cargo.toml"),
            format!(
                "[package]\nname = \"{bin_name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[[bin]]\nname = \"{bin_name}\"\npath = \"src/main.rs\"\n"
            ),
        )?;
        fs::write(
            dir.join("src/main.rs"),
            "fn main() {\n    println!(\"fake simulator binary\");\n}\n",
        )?;
        Ok(())
    }

    /// Bug 1 regression test: staging must produce a real, executable
    /// controller binary at the standard Webots layout
    /// (`<project>/.phoxal/webots/controllers/<name>/<name>`) for BOTH
    /// the supervisor and the per-robot controller when the simulators are
    /// path-overridden (the live-gate case), by actually running `cargo
    /// build` against fake stand-in crates - not just asserting a path string
    /// was computed. Also covers the copy->symlink change: the staged entry
    /// must be a symlink to the built binary, not a copy.
    #[test]
    fn path_overridden_simulators_are_built_and_staged_as_webots_controllers() -> Result<()> {
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let temp = tempfile::tempdir()?;

        let supervisor_dir = temp.path().join("framework/simulator/webots-supervisor");
        let controller_dir = temp.path().join("framework/simulator/webots-controller");
        write_fake_simulator_crate(&supervisor_dir, "phoxal-simulator-webots-supervisor")?;
        write_fake_simulator_crate(&controller_dir, "phoxal-simulator-webots-controller")?;

        let mut resolved = resolved_with_drive_components(&[], false)?;
        resolved.simulators.clear();
        let mut supervisor = simulator_runtime(SIMULATOR_SUPERVISOR_ARTIFACT_NAME);
        supervisor.path_override = Some(supervisor_dir.clone());
        let mut controller = simulator_runtime(SIMULATOR_CONTROLLER_ARTIFACT_NAME);
        controller.path_override = Some(controller_dir.clone());
        resolved.simulators.extend([supervisor, controller]);

        stage_simulator_controller_binaries(&resolved, &crate::Ui::from_env())?;

        let supervisor_binary =
            webots_stage_root::controller_dir("phoxal-simulator-webots-supervisor")?
                .join("phoxal-simulator-webots-supervisor");
        let controller_binary =
            webots_stage_root::controller_dir("phoxal-simulator-webots-controller")?
                .join("phoxal-simulator-webots-controller");

        for binary in [&supervisor_binary, &controller_binary] {
            assert!(
                binary.is_file(),
                "expected staged controller binary to exist at {}",
                binary.display()
            );
            assert!(
                fs::symlink_metadata(binary)?.file_type().is_symlink(),
                "staged controller binary {} should be a symlink, not a copy",
                binary.display()
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(binary)?.permissions().mode();
                assert!(
                    mode & 0o111 != 0,
                    "staged controller binary {} must be executable (mode {mode:o})",
                    binary.display()
                );
            }
        }

        // Confirm it is genuinely the built binary, not an empty placeholder:
        // running it must succeed and print the fake marker.
        let output = std::process::Command::new(&supervisor_binary).output()?;
        assert!(output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("fake simulator binary"),
            "staged supervisor binary did not run as expected"
        );

        Ok(())
    }

    /// A catalog (non path-overridden) simulator with no native-artifact
    /// metadata and nothing in the artifact cache must fail loudly during
    /// staging rather than silently leaving the controller unstaged - the
    /// exact "generic controller" trap bug 1 exists to close.
    #[test]
    fn catalog_simulator_missing_from_cache_is_a_hard_error() -> Result<()> {
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let mut resolved = resolved_with_drive_components(&[], false)?;
        resolved.simulators.clear();
        resolved
            .simulators
            .push(simulator_runtime(SIMULATOR_SUPERVISOR_ARTIFACT_NAME));

        let error = stage_simulator_controller_binaries(&resolved, &crate::Ui::from_env())
            .expect_err("a catalog simulator with no cached binary must error, not silently skip");
        let message = format!("{error:#}");
        assert!(
            message.contains("webots-supervisor"),
            "error should name the simulator that failed to provision: {message}"
        );

        Ok(())
    }

    /// The staged root must resolve under `<project>/.phoxal/webots`, and each mounted
    /// component type's staged `meshes/<component_type>` entry must be a
    /// SYMLINK to the component's resolved mesh source directory - not a
    /// copy - so the vendored/path-pinned source stays authoritative.
    #[test]
    fn stage_simulation_uses_project_store_and_symlinks_component_meshes() -> Result<()> {
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let temp = tempfile::tempdir()?;
        fs::write(
            temp.path().join("structure.urdf"),
            r#"<robot name="testbot"><link name="base_footprint"/><link name="base_link"/><joint name="base_joint" type="fixed"><parent link="base_footprint"/><child link="base_link"/></joint></robot>"#,
        )?;
        fs::create_dir_all(temp.path().join("worlds"))?;
        let world_source_path = temp.path().join("worlds/default.wbt");
        fs::write(
            &world_source_path,
            "#VRML_SIM R2023b utf8\n\nWorldInfo {\n}\n",
        )?;
        write_driver_crate(temp.path(), "ddsm115")?;
        fs::write(
            temp.path().join("components/ddsm115/component.yaml"),
            "schema: component/v0\ncapabilities: {}\n",
        )?;
        let component_meshes_dir = temp.path().join("components/ddsm115/meshes");
        fs::create_dir_all(&component_meshes_dir)?;
        fs::write(component_meshes_dir.join("wheel.dae"), b"not a real mesh")?;

        let mut resolved = resolved_with_drive_components(&["left_drive"], false)?;
        resolved
            .simulators
            .push(simulator_runtime(SIMULATOR_SUPERVISOR_ARTIFACT_NAME));
        let extras = RobotManifestExtras::default();
        let supervisor_id = SIMULATOR_SUPERVISOR_PROVIDER_ID.to_string();
        let controller_id = simulator_controller_provider_id("robot_v1");
        let checked = vec![
            service_participant("drive", vec![motor_command()]),
            driver_participant("ddsm115", "left_drive", vec![motor_command()]),
            graph_check::ParticipantApis {
                participant_id: supervisor_id.clone(),
                artifact_id: "webots-supervisor".to_string(),
                participant_kind: graph_check::ParticipantKind::Simulator,
                participant_class: graph_check::ParticipantClass::Checked,
                api_version: "v1".to_string(),
                config_schema: None,
                scope: graph_check::ParticipantScope::Graph,
                contracts: Vec::new(),
            },
            simulator_controller_participant(&controller_id, vec![motor_command()]),
        ];
        let substitutions = simulated_component_records(&checked, &controller_id);
        let sim_participants = sim_checked_participants(&checked);
        let report = graph_check::check_graph(&sim_participants);
        assert!(report.is_ok(), "{report:?}");

        let plan = build_launch_plan(
            webots_mode_for_tests(),
            &[CheckedRobotLaunchInput {
                project_root: temp.path(),
                resolved: &resolved,
                manifest_extras: &extras,
                checked_participants: &sim_participants,
                substitutions: &substitutions,
                source_participants: &[],
            }],
        )?;

        let staged = stage_simulation_for_robot(temp.path(), &world_source_path, &resolved, &plan)?;

        let root = webots_stage_root::root()?;
        assert!(
            root.ends_with(".phoxal/webots"),
            "staged root should resolve under project-local .phoxal/webots, got {}",
            root.display()
        );
        assert!(
            staged.staged_world_path.starts_with(&root),
            "staged world {} should live under the staged root {}",
            staged.staged_world_path.display(),
            root.display()
        );

        let staged_component_meshes = root.join("meshes").join("ddsm115");
        let meta = fs::symlink_metadata(&staged_component_meshes)?;
        assert!(
            meta.file_type().is_symlink(),
            "staged component meshes {} should be a symlink, not a copy",
            staged_component_meshes.display()
        );
        let link_target = fs::read_link(&staged_component_meshes)?;
        assert_eq!(link_target, component_meshes_dir);
        assert!(
            link_target.is_absolute(),
            "mesh symlink target must be absolute: {}",
            link_target.display()
        );
        assert_eq!(
            fs::read(staged_component_meshes.join("wheel.dae"))?,
            b"not a real mesh"
        );

        Ok(())
    }

    /// The wipe-per-play guarantee: a previous play's stale staged content
    /// must never linger into the next `simulate` invocation - it is one
    /// Webots world per run, so a clean slate every play is correct.
    #[test]
    fn stage_simulation_for_robot_wipes_previous_play_before_restaging() -> Result<()> {
        let _phoxal_home = ScratchPhoxalHome::new()?;
        let temp = tempfile::tempdir()?;
        fs::write(
            temp.path().join("structure.urdf"),
            r#"<robot name="testbot"><link name="base_footprint"/><link name="base_link"/><joint name="base_joint" type="fixed"><parent link="base_footprint"/><child link="base_link"/></joint></robot>"#,
        )?;
        fs::create_dir_all(temp.path().join("worlds"))?;
        let world_source = temp.path().join("worlds/test.wbt");
        fs::write(&world_source, "#VRML_SIM R2023b utf8\n\nWorldInfo {\n}\n")?;

        let mut resolved = resolved_with_drive_components(&[], false)?;
        resolved
            .simulators
            .push(simulator_runtime(SIMULATOR_SUPERVISOR_ARTIFACT_NAME));
        resolved
            .simulators
            .push(simulator_runtime(SIMULATOR_CONTROLLER_ARTIFACT_NAME));
        let extras = RobotManifestExtras::default();
        let supervisor_id = SIMULATOR_SUPERVISOR_PROVIDER_ID.to_string();
        let controller_id = simulator_controller_provider_id("robot_v1");
        let checked = vec![
            service_participant("drive", vec![motor_command()]),
            graph_check::ParticipantApis {
                participant_id: supervisor_id.clone(),
                artifact_id: "webots-supervisor".to_string(),
                participant_kind: graph_check::ParticipantKind::Simulator,
                participant_class: graph_check::ParticipantClass::Checked,
                api_version: "v1".to_string(),
                config_schema: None,
                scope: graph_check::ParticipantScope::Graph,
                contracts: Vec::new(),
            },
            graph_check::ParticipantApis {
                participant_id: controller_id.clone(),
                artifact_id: "webots-controller".to_string(),
                participant_kind: graph_check::ParticipantKind::Simulator,
                participant_class: graph_check::ParticipantClass::Checked,
                api_version: "v1".to_string(),
                config_schema: None,
                scope: graph_check::ParticipantScope::Graph,
                contracts: Vec::new(),
            },
        ];
        let sim_participants = sim_checked_participants(&checked);
        let plan = build_launch_plan(
            webots_mode_for_tests(),
            &[CheckedRobotLaunchInput {
                project_root: temp.path(),
                resolved: &resolved,
                manifest_extras: &extras,
                checked_participants: &sim_participants,
                substitutions: &[],
                source_participants: &[],
            }],
        )?;

        let first = stage_simulation_for_robot(temp.path(), &world_source, &resolved, &plan)?;
        assert!(first.staged_world_path.is_file());

        // Plant a stray marker directly under the staged root, simulating
        // leftover content from a previous play that must not survive the
        // next one's staging.
        let root = webots_stage_root::root()?;
        let marker = root.join("stale-from-previous-play.txt");
        fs::write(&marker, b"stale")?;
        assert!(marker.is_file());

        let second = stage_simulation_for_robot(temp.path(), &world_source, &resolved, &plan)?;
        assert!(second.staged_world_path.is_file());
        assert!(
            !marker.exists(),
            "a second stage must wipe the staged root before restaging, but {} survived",
            marker.display()
        );

        Ok(())
    }
}
