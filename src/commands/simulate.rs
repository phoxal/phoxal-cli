use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use phoxal::check as graph_check;
use serde::Serialize;
use tokio::sync::mpsc;

use crate::AppContext;
use crate::catalog::CatalogRevision;
use crate::commands::MessageFormat;
use crate::commands::check::{
    CheckGraphContext, SourceParticipant, SourceParticipantKind, build_emit_apis_from_source,
    fetch_emit_apis_from_native_artifact, platform_artifact_refs_from_resolved,
    robot_graph_from_resolved, run_check_with_context, source_participants_building_only_crate,
    source_participants_from_resolved,
};
use crate::component_driver::{component_assets_dir, component_driver_crate_dir};
use crate::launch_plan::{
    CheckedRobotLaunchInput, DEFAULT_ROUTER_CONNECT, LaunchMode, LaunchPlan, SITE_TOOL_JOYPAD,
    SITE_TOOL_ROUTER, SubstitutedContract, SubstitutionRecord, build_launch_plan,
};
use crate::resolver::{
    ResolveOptions, ResolvedPlatformRuntime, ResolvedRobot, RobotManifestExtras, resolve,
};
use crate::simulate_staging::{
    ComponentTypeToStage, RobotToStage, StagedSimulationWorld, stage_simulation_world,
};
use crate::supervisor::{
    BoardBackend, ParticipantKind, ParticipantSpec, ParticipantState, ParticipantStatus,
    RouterOwnership, SupervisorLock, SupervisorOptions, default_connect_endpoint,
    local_router_reachable, router_ownership, start_bus_log_subscriber, supervise_until_shutdown,
    supervisor_actions_path, supervisor_state_path,
};
use crate::webots_stage_root;
use crate::world;

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

/// The robot-scoped participant id for the Webots controller artifact
/// (`phoxal-simulator-webots-controller`) that substitutes `robot_id`'s
/// component-driver contracts. One controller participant exists per robot;
/// this scheme generalizes to a multi-robot plan (each robot gets its own
/// controller id, so N robots keep N distinct substitution providers).
pub(crate) fn simulator_controller_provider_id(robot_id: &str) -> String {
    format!("simulator-webots-controller-{robot_id}")
}

#[derive(Debug, Args)]
pub struct Simulate {
    #[arg(
        value_name = "WORLD",
        help = "World file or bare name (e.g. `default`, or `worlds/foo.wbt`). Resolved against <project>/worlds/<world>.wbt, then <project>/<world>, then ~/.phoxal/worlds/<world>.wbt."
    )]
    pub world: String,
    #[arg(
        long,
        help = "Resolve and write run artifacts without starting simulation processes."
    )]
    pub dry_run: bool,
    #[arg(long, hide = true)]
    pub joypad: bool,
    #[arg(
        long,
        help = "Refresh native service artifacts and host tools instead of reusing compatible cached artifacts."
    )]
    pub pull: bool,
    #[arg(long, value_enum, default_value_t = MessageFormat::Human)]
    pub message_format: MessageFormat,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimulateMode {
    Live,
    DryRun,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SimulateOptions {
    pub world: String,
    pub joypad: bool,
    pub pull: bool,
    pub catalog_source: Option<String>,
    pub message_format: MessageFormat,
    pub watch: bool,
    pub overlays: Vec<String>,
    pub target: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SimulatePlan {
    pub robot_path: PathBuf,
    pub project_root: PathBuf,
    pub world_path: PathBuf,
    pub bus_connect: String,
    pub native_tools: Vec<String>,
    pub resolved: ResolvedRobot,
    pub launch_plan: LaunchPlan,
    pub source_participants: Vec<SourceParticipant>,
}

pub(crate) struct ResolvedSimulation {
    pub(crate) robot_path: PathBuf,
    pub(crate) project_root: PathBuf,
    pub(crate) world_path: PathBuf,
    pub(crate) resolved: ResolvedRobot,
    pub(crate) manifest_extras: RobotManifestExtras,
    pub(crate) catalog: Option<CatalogRevision>,
}

impl Simulate {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let options = SimulateOptions {
            world: self.world.clone(),
            joypad: self.joypad,
            pull: self.pull,
            catalog_source: app.catalog_source.clone(),
            message_format: self.message_format,
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
) -> Result<SimulatePlan> {
    match mode {
        SimulateMode::DryRun => {
            let project_root = app.project.root().to_path_buf();
            let message_format = options.message_format;
            let plan = tokio::task::spawn_blocking(move || prepare(&project_root, options))
                .await
                .context("simulate dry-run worker failed")??;
            report_plan_only(&plan, message_format)?;
            Ok(plan)
        }
        SimulateMode::Live => {
            let project_root = app.project.root().to_path_buf();
            let ui = app.ui;
            let prepared_options = options.clone();
            let plan = tokio::task::spawn_blocking(move || {
                prepare_with_mode(&project_root, prepared_options, SimulateMode::Live)
            })
            .await
            .context("simulate preparation worker failed")??;

            crate::host_doctor::preflight()
                .map_err(|error| anyhow!("{error}"))
                .context("Webots preflight failed; live simulate cannot launch the simulator")?;

            let run_dir = crate::host_paths::run_dir()?;
            let _lock = SupervisorLock::acquire(&run_dir)?;
            let state_file = supervisor_state_path()?;
            let action_file = supervisor_actions_path()?;
            let board = BoardBackend::new();
            let router_ownership =
                router_ownership(local_router_reachable(&default_connect_endpoint()));
            let mut specs = Vec::new();
            crate::commands::run::prepare_site_tools(
                &plan.launch_plan,
                &plan.resolved,
                &board,
                &mut specs,
                router_ownership,
            )?;
            crate::commands::run::prepare_robot_participants(
                &plan.launch_plan,
                &plan.resolved,
                &plan.project_root,
                &crate::commands::run::DriverPolicy::drivers_off_for_sim(),
                &board,
                &mut specs,
                &ui,
            )?;
            prepare_substitution_notes(&plan.launch_plan, &board);

            let webots_spec = stage_and_prepare_webots_spec(app, &plan)?;
            specs.push(webots_spec);

            app.ui.info(format!(
                "simulation launch plan resolved: {} robot(s), {} site tool(s)",
                plan.launch_plan.robots.len(),
                plan.launch_plan.site.len()
            ));
            match router_ownership {
                RouterOwnership::External => app.ui.info("reusing reachable external tool-router"),
                RouterOwnership::Managed => {
                    app.ui
                        .info("tool-router will be managed by this simulation session");
                }
            }
            crate::commands::run::report_launch_commands(&specs, options.message_format)?;

            let _log_tasks = plan
                .launch_plan
                .robots
                .iter()
                .map(|robot| {
                    start_bus_log_subscriber(
                        robot.namespace.clone(),
                        robot.id.clone(),
                        default_connect_endpoint(),
                        board.clone(),
                    )
                })
                .collect::<Vec<_>>();

            let (action_rx, watch_handle) = if options.watch {
                let (action_tx, action_rx) = mpsc::channel(16);
                let live_ids = specs
                    .iter()
                    .map(|spec| spec.id.clone())
                    .collect::<std::collections::BTreeSet<_>>();
                let handle = crate::watch::spawn_sim_watch(crate::watch::SimWatchConfig {
                    project_root: plan.project_root.clone(),
                    options: options.clone(),
                    resolved: plan.resolved.clone(),
                    source_participants: plan.source_participants.clone(),
                    live_ids,
                    board: board.clone(),
                    action_tx,
                });
                (Some(action_rx), Some(handle))
            } else {
                (None, None)
            };

            let outcome = supervise_until_shutdown(
                specs,
                board.clone(),
                SupervisorOptions {
                    state_file: Some(state_file),
                    action_file: Some(action_file),
                    action_rx,
                    ..SupervisorOptions::default()
                },
            )
            .await;
            if let Some(handle) = watch_handle {
                handle.abort();
            }
            let outcome = outcome?;

            if !outcome.graph_healthy() {
                bail!(
                    "supervisor graph ended unhealthy; failed participants: {}",
                    outcome.failed_participants.join(", ")
                );
            }
            Ok(plan)
        }
    }
}

pub fn prepare(project_start: &Path, options: SimulateOptions) -> Result<SimulatePlan> {
    prepare_with_mode(project_start, options, SimulateMode::DryRun)
}

fn prepare_with_mode(
    project_start: &Path,
    options: SimulateOptions,
    mode: SimulateMode,
) -> Result<SimulatePlan> {
    let resolved = resolve_project(project_start, options.clone(), mode)?;
    if mode == SimulateMode::Live {
        crate::native_artifacts::stage_component_bundles_into_robot_root(
            &resolved.project_root,
            &resolved.project_root,
            &resolved.resolved,
        )
        .context("failed to stage component assets into the simulation robot root")?;
    }
    let mut launch_plan = build_checked_sim_launch_plan(
        &resolved.project_root,
        &resolved.resolved,
        &resolved.manifest_extras,
        resolved.catalog.as_ref(),
    )?;
    if !options.joypad {
        // See `native_tool_labels`: joypad is opt-in for simulate now, so
        // drop it from the actual launch plan too (not just the dry-run
        // display list) - otherwise live simulate would still try to launch
        // it and hit the config-deserialization failure this change avoids.
        launch_plan.site.retain(|site| site.id != SITE_TOOL_JOYPAD);
    }
    let source_participants = sim_source_participants(
        &resolved.project_root,
        &resolved.resolved,
        resolved.catalog.as_ref(),
    )?;
    Ok(SimulatePlan {
        robot_path: resolved.robot_path,
        project_root: resolved.project_root,
        world_path: resolved.world_path,
        bus_connect: DEFAULT_ROUTER_CONNECT.to_string(),
        native_tools: native_tool_labels(options),
        resolved: resolved.resolved,
        launch_plan,
        source_participants,
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
    let catalog = crate::catalog::load_catalog(crate::catalog::CatalogLoadOptions {
        refresh: options.pull,
        cli_source: options.catalog_source.clone(),
        robot_source: manifest_extras.catalog_source.as_ref().map(|source| {
            if source.is_absolute() {
                source.clone()
            } else {
                project_root.join(source)
            }
        }),
    })?;

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

pub(crate) fn build_checked_sim_launch_plan(
    project_root: &Path,
    resolved: &ResolvedRobot,
    manifest_extras: &RobotManifestExtras,
    catalog: Option<&CatalogRevision>,
) -> Result<LaunchPlan> {
    build_checked_sim_launch_plan_with_scope(project_root, resolved, manifest_extras, catalog, None)
}

pub(crate) fn build_checked_sim_launch_plan_with_scope(
    project_root: &Path,
    resolved: &ResolvedRobot,
    manifest_extras: &RobotManifestExtras,
    catalog: Option<&CatalogRevision>,
    build_only_crate: Option<&Path>,
) -> Result<LaunchPlan> {
    let robot_graph = robot_graph_from_resolved(resolved);
    let mut source_participants = sim_source_participants(project_root, resolved, catalog)
        .with_context(|| "failed to prepare source participants for simulation metadata")?;
    if let Some(crate_dir) = build_only_crate {
        source_participants =
            source_participants_building_only_crate(&source_participants, crate_dir);
    }
    // A Catalog-sourced component driver is a platform ref here too (docs
    // #21), exactly like `check`/`run`/`deploy` - fetched from its packaged
    // release asset rather than built from source. Only a Path/Git-overridden
    // driver crate reaches the `build` closure below.
    let mut platform_refs = platform_artifact_refs_from_resolved(resolved);
    platform_refs
        .extend(crate::commands::check::component_driver_platform_refs_from_resolved(resolved));
    let mut official_by_ref = resolved
        .platform_runtimes
        .iter()
        .map(|runtime| (runtime.artifact_ref().to_string(), runtime))
        .collect::<BTreeMap<_, _>>();
    official_by_ref.extend(crate::commands::check::component_driver_runtimes_by_ref(
        resolved,
    ));

    let metadata_outcome = run_check_with_context(
        &platform_refs,
        &[],
        &source_participants,
        CheckGraphContext {
            robot_graph: &robot_graph,
            manifest_extras,
        },
        |artifact_ref| {
            let runtime = official_by_ref.get(artifact_ref).ok_or_else(|| {
                anyhow!("resolved official artifact {artifact_ref} is not in the catalog")
            })?;
            fetch_emit_apis_from_native_artifact(runtime)
        },
        |_| unreachable!("simulate does not check site tools as graph participants"),
        |participant| {
            if participant.kind == SourceParticipantKind::ComponentDriver {
                return build_emit_apis_from_source(participant)
                    .map_err(|error| driver_metadata_unavailable(participant, error));
            }
            build_emit_apis_from_source(participant)
        },
    )?;

    let mut checked_participants = metadata_outcome.checked_participants.clone();
    remap_simulator_participant_ids(&mut checked_participants, &resolved.robot.robot.id)?;
    checked_participants.extend(official_simulator_participants(resolved)?);
    let controller_provider_id = simulator_controller_provider_id(&resolved.robot.robot.id);
    let substitutions = simulated_component_records(&checked_participants, &controller_provider_id);
    let sim_participants = sim_checked_participants(&checked_participants);
    let report = graph_check::check_plan(graph_check::CheckInput {
        participants: &sim_participants,
        robot_graph: &robot_graph,
    });
    if !report.is_ok() {
        crate::commands::check::ensure_check_outcome_ok(
            &resolved.target_generation,
            &resolved.channel.to_string(),
            &crate::commands::check::CheckOutcome {
                missing_images: Vec::new(),
                report: report.clone(),
                checked_participants: sim_participants.clone(),
            },
        )?;
    }

    build_launch_plan(
        LaunchMode::Sim,
        &[CheckedRobotLaunchInput {
            project_root,
            resolved,
            manifest_extras,
            checked_participants: &sim_participants,
            substitutions: &substitutions,
            source_participants: &source_participants,
        }],
    )
}

fn official_simulator_participants(
    resolved: &ResolvedRobot,
) -> Result<Vec<graph_check::ParticipantApis>> {
    let robot_id = resolved.robot.robot.id.as_str();
    let mut participants = Vec::new();
    for runtime in resolved
        .simulators
        .iter()
        .filter(|runtime| runtime.source_path().is_none())
    {
        let raw = fetch_emit_apis_from_native_artifact(runtime).with_context(|| {
            format!(
                "failed to read packaged emit-apis for simulator {}",
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
        let mut participant = graph_check::ParticipantApis::try_from(raw).with_context(|| {
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
        participants.push(participant);
    }
    Ok(participants)
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
    _catalog: Option<&CatalogRevision>,
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
        "DriverMetadataUnavailable: component driver crate '{}' for instance '{}' could not produce emit-apis on this host: {error:#}\n\nCustom and git-sourced driver crates must compile far enough on the dev host for `emit-apis`; keep hardware transport behind a target cfg boundary such as `cfg(target_os = \"linux\")`. Alternatively publish packaged driver metadata in the verified artifact catalog and use that catalog driver instead.",
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
/// A driver's own `emit-apis` reports its component-template contracts with a
/// literal `{instance}` placeholder (the same driver binary can be launched
/// at any instance), so `{instance}` is filled in here with the driver's own
/// known `component_instance` before display - the one piece of materialization
/// this function can do unambiguously without consulting the robot graph. Any
/// remaining `{capability}` placeholder is left as-is: `substitution_topic_summary`
/// only keys off the `component/<instance>/` prefix, so this is enough to keep
/// the board's collapsed "component/<instance>/*" rendering working.
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
                            topic: contract.topic.replace("{instance}", instance),
                            direction: crate::commands::check::format_direction(contract.direction)
                                .to_string(),
                            schema_id: contract.schema_id.clone(),
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

fn report_plan_only(plan: &SimulatePlan, message_format: MessageFormat) -> Result<()> {
    let output = build_dry_run_output(plan);
    crate::commands::print_message(
        &output,
        || {
            println!(
                "target_generation: {} (channel {})",
                plan.resolved.target_generation, plan.resolved.channel
            );
            if let Some(revision) = &plan.resolved.catalog_revision {
                println!("catalog revision: {revision}");
            }
            println!(
                "official services ({}):",
                plan.resolved.platform_runtimes.len()
            );
            for runtime in &plan.resolved.platform_runtimes {
                println!("  - {} -> {}", runtime.name, runtime.artifact_ref());
            }
            println!("world: {}", plan.world_path.display());
            println!("router: {}", plan.bus_connect);
            // Out of the project tree now (`~/.phoxal/run/simulation/webots`,
            // see `webots_stage_root`), so print it explicitly for discoverability
            // even though nothing is written in dry-run mode.
            if let Ok(root) = webots_stage_root::root() {
                println!("staged simulation to {}", root.display());
            }
            println!("site tools:");
            for tool in &plan.native_tools {
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
        },
        message_format,
    )
}

/// Build the dry-run report body (Part 6): must show the Webots app as the
/// CLI-managed child, both simulator artifacts (supervisor + controller) with
/// their participant ids, and each simulator participant's SIMULATION-MANAGED
/// ownership + the intended staged world path. Never stages or launches
/// anything - the path is computed, not written.
fn build_dry_run_output(plan: &SimulatePlan) -> SimulateDryRunOutput {
    let substitutions = substitution_lines(&plan.launch_plan);
    let simulator_artifacts = simulator_artifact_lines(&plan.resolved);
    let simulation_managed = simulation_managed_lines(&plan.launch_plan);
    let intended_staged_world_path = intended_staged_world_path(&plan.world_path);
    SimulateDryRunOutput {
        mode: "dry-run",
        target_generation: plan.resolved.target_generation.clone(),
        channel: plan.resolved.channel.to_string(),
        catalog_revision: plan.resolved.catalog_revision.clone(),
        world_path: plan.world_path.clone(),
        bus_connect: plan.bus_connect.clone(),
        platform_service_count: plan.resolved.platform_runtimes.len(),
        native_tools: plan.native_tools.clone(),
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

#[derive(Debug, Serialize)]
struct SimulateDryRunOutput {
    mode: &'static str,
    target_generation: String,
    channel: String,
    catalog_revision: Option<String>,
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

fn prepare_substitution_notes(plan: &LaunchPlan, board: &BoardBackend) {
    for robot in &plan.robots {
        for substitution in &robot.substitutions {
            let mut status = ParticipantStatus::new(
                &substitution.component_instance,
                ParticipantKind::ComponentDriver,
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

fn substitution_topic_summary(substitution: &SubstitutionRecord) -> String {
    let component_prefix = format!("component/{}/", substitution.component_instance);
    if substitution.contracts.is_empty()
        || substitution
            .contracts
            .iter()
            .all(|contract| contract.topic.starts_with(&component_prefix))
    {
        return format!("component/{}/*", substitution.component_instance);
    }
    substitution
        .contracts
        .iter()
        .map(|contract| contract.topic.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// `tool-joypad` is peripheral teleop, not part of the sim contract graph, so
/// it no longer launches by default: the framework `tool-joypad` deserializes
/// its `PHOXAL_CONFIG` as a unit `()`, but this crate's shared site-tool
/// launch path (`launch_plan::build_site_launches` / `commands::run::site_env`)
/// unconditionally sends `PHOXAL_CONFIG={}` for every non-router site tool -
/// which fails a genuinely config-less tool's deserialization with `invalid
/// type: map, expected unit`, exactly the live-gate failure this fixes.
/// Making that encoding conditional is the more "correct" fix, but it is
/// shared with `run`/`deploy` (out of this fix's scope, and a behavior change
/// neither asked for); simulate instead just stops auto-launching joypad,
/// gated behind the pre-existing (till now unused) `--joypad`/`options.joypad`
/// flag - see the matching filter in `prepare_with_mode`.
fn native_tool_labels(options: SimulateOptions) -> Vec<String> {
    let mut labels = vec![SITE_TOOL_ROUTER.to_string()];
    if options.joypad {
        labels.push(SITE_TOOL_JOYPAD.to_string());
    }
    labels.push("webots".to_string());
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
fn stage_and_prepare_webots_spec(app: &AppContext, plan: &SimulatePlan) -> Result<ParticipantSpec> {
    let staged = stage_simulation_for_robot(
        &plan.project_root,
        &plan.world_path,
        &plan.resolved,
        &plan.launch_plan,
    )?;
    stage_simulator_controller_binaries(&plan.resolved, &app.ui)?;
    let webots_path = crate::host_doctor::webots_executable_path()
        .map_err(|error| anyhow!("{error}"))
        .context("failed to locate the Webots executable for live simulate")?;
    // The staged root now lives under `~/.phoxal/run/...` rather than the
    // project tree, so print it explicitly - it is no longer discoverable by
    // just looking under the project.
    app.ui.info(format!(
        "staged simulation to {}",
        webots_stage_root::root()?.display()
    ));
    app.ui.info(format!(
        "staged simulation world at {}",
        staged.staged_world_path.display()
    ));
    Ok(ParticipantSpec {
        id: WEBOTS_SITE_ID.to_string(),
        kind: ParticipantKind::SiteTool,
        executable: webots_path,
        args: vec![staged.staged_world_path.display().to_string()],
        cwd: None,
        env: Vec::new(),
        shutdown_grace: std::time::Duration::from_secs(10),
        note: None,
    })
}

/// Stage the two Webots controller BINARIES (supervisor + per-robot
/// controller) into the standard Webots layout,
/// `~/.phoxal/run/simulation/webots/controllers/<controller-name>/<controller-name>`
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
/// unstaged when the artifact was never pulled into the cache.
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
                "simulator '{}' has no native-artifact metadata (missing sha256/emit-apis); run `phoxal-cli pull` or pin a path override",
                runtime.name
            )
        })?;
    let cached = crate::native_artifacts::artifact_binary_path(&descriptor)?;
    if !cached.is_file() {
        bail!(
            "NativePending: simulator '{}' binary is not in the artifact cache ({}); run `phoxal-cli pull` to fetch it",
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
        let crate_dir = component_assets_dir(component, project_root)?;
        let component_model = phoxal::model::component::Component::read_from_dir(&crate_dir)
            .with_context(|| {
                format!(
                    "failed to read component.yaml for component type '{}' from {}",
                    component.source_name,
                    crate_dir.display()
                )
            })?
            .as_v1()
            .context("Webots staging only supports component.yaml version v1")?
            .clone();
        components.insert(component.source_name.clone(), component_model);
        component_type_dirs.insert(component.source_name.clone(), crate_dir);
    }

    let bundle = phoxal::model::v1::Robot {
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
        ArtifactKind, ArtifactStatus, Channel as CatalogChannel, fixture_catalog_for_tests,
        fixture_contract_for_tests, fixture_tool_entry_for_tests,
    };
    use crate::host_paths::test_support::ScratchPhoxalHome;
    use crate::resolver::{
        ResolvedComponent, ResolvedComponentSource, ResolvedPathOverride, ResolvedPathOverrideKind,
        ResolvedPlatformRuntime, ResolvedTool, ResolvedUserRuntime, host_target_triple,
        target_generation_for_robot,
    };
    use std::fs;

    #[test]
    fn live_resolve_path_only_project_needs_no_lock_or_network() -> Result<()> {
        // With no lockfile, a path-only / official-only project resolves live
        // with no network for either mode: there is nothing to look up remotely
        // (no git components), so resolution succeeds and writes no lock.
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

        assert_eq!(resolved.resolved.target_generation, "y2026_1");
        assert!(resolved.resolved.components.is_empty());
        Ok(())
    }

    #[test]
    fn dry_run_resolve_path_only_project_needs_no_lock_or_network() -> Result<()> {
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

        assert_eq!(resolved.resolved.target_generation, "y2026_1");
        Ok(())
    }

    #[test]
    fn no_components_sim_plan_matches_run_plan_participants() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut resolved = empty_resolved_robot("robot_v1")?;
        add_site_tools(&mut resolved);
        resolved.platform_runtimes.push(platform_runtime(
            "drive",
            vec![fixture_contract_for_tests(
                "drive::Target",
                "drive/target",
                "publish",
                "schema-drive",
            )],
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
            LaunchMode::Sim,
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
        let graph = component_graph(&["left_drive"]);
        let controller_id = simulator_controller_provider_id("robot_v1");
        let checked = vec![
            service_participant(
                "drive",
                vec![motor_command(graph_check::Direction::Publish)],
            ),
            driver_participant(
                "ddsm115",
                "left_drive",
                vec![motor_command(graph_check::Direction::Subscribe)],
            ),
            simulator_controller_participant(
                &controller_id,
                vec![motor_command(graph_check::Direction::Subscribe)],
            ),
        ];
        let substitutions = simulated_component_records(&checked, &controller_id);
        let sim_participants = sim_checked_participants(&checked);
        let report = graph_check::check_plan(graph_check::CheckInput {
            participants: &sim_participants,
            robot_graph: &graph,
        });
        assert!(report.is_ok(), "{report:?}");

        let plan = build_launch_plan(
            LaunchMode::Sim,
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
            service_participant(
                "drive",
                vec![motor_command(graph_check::Direction::Publish)],
            ),
            driver_participant(
                "ddsm115",
                "left_drive",
                vec![materialized_motor_command(
                    "left_drive",
                    graph_check::Direction::Subscribe,
                )],
            ),
            driver_participant(
                "ddsm115",
                "right_drive",
                vec![materialized_motor_command(
                    "right_drive",
                    graph_check::Direction::Subscribe,
                )],
            ),
            simulator_controller_participant(
                &controller_id,
                vec![motor_command(graph_check::Direction::Subscribe)],
            ),
        ];
        // Board display only (no checker involved, see module docs): each
        // dropped driver instance gets its own disjoint substitution record.
        let substitutions = simulated_component_records(&checked, &controller_id);
        let topics = substitutions
            .iter()
            .map(|substitution| {
                (
                    substitution.component_instance.as_str(),
                    substitution.contracts[0].topic.as_str(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            topics,
            vec![
                ("left_drive", "component/left_drive/motor/motor/command"),
                ("right_drive", "component/right_drive/motor/motor/command"),
            ]
        );
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
                api_version: "y2026_1".to_string(),
                bus_abi: None,
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
            LaunchMode::Sim,
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
            driver_participant(
                "ddsm115",
                "left_drive",
                vec![motor_command(graph_check::Direction::Subscribe)],
            ),
            simulator_controller_participant(
                &controller_id,
                vec![motor_command(graph_check::Direction::Subscribe)],
            ),
        ];
        let substitutions = simulated_component_records(&checked, &controller_id);
        let sources = vec![SourceParticipant::user_service(
            "mission",
            temp.path().join("runtimes/mission"),
        )];
        let sim_participants = sim_checked_participants(&checked);
        let plan = build_launch_plan(
            LaunchMode::Sim,
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
        let graph = component_graph(&["left_drive"]);
        let supervisor_id = SIMULATOR_SUPERVISOR_PROVIDER_ID.to_string();
        let controller_id = simulator_controller_provider_id("robot_v1");
        let checked = vec![
            service_participant(
                "drive",
                vec![motor_command(graph_check::Direction::Publish)],
            ),
            driver_participant(
                "ddsm115",
                "left_drive",
                vec![motor_command(graph_check::Direction::Subscribe)],
            ),
            graph_check::ParticipantApis {
                participant_id: supervisor_id.clone(),
                artifact_id: "webots-supervisor".to_string(),
                participant_kind: graph_check::ParticipantKind::Simulator,
                participant_class: graph_check::ParticipantClass::Checked,
                api_version: "y2026_1".to_string(),
                bus_abi: None,
                config_schema: None,
                scope: graph_check::ParticipantScope::Graph,
                contracts: Vec::new(),
            },
            simulator_controller_participant(
                &controller_id,
                vec![motor_command(graph_check::Direction::Subscribe)],
            ),
        ];
        let substitutions = simulated_component_records(&checked, &controller_id);
        let sim_participants = sim_checked_participants(&checked);
        let report = graph_check::check_plan(graph_check::CheckInput {
            participants: &sim_participants,
            robot_graph: &graph,
        });
        assert!(report.is_ok(), "{report:?}");

        let plan = build_launch_plan(
            LaunchMode::Sim,
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
        // Staging now lands under `~/.phoxal/run/simulation/webots` (home,
        // not project-scoped), so this must run under a scratch `PHOXAL_HOME`
        // rather than touching the real developer machine's staging root.
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
            "version: v1\ncapabilities: {}\n",
        )?;

        let mut resolved = resolved_with_drive_components(&["left_drive"], false)?;
        resolved
            .simulators
            .push(simulator_runtime(SIMULATOR_SUPERVISOR_ARTIFACT_NAME));
        let extras = RobotManifestExtras::default();
        let graph = component_graph(&["left_drive"]);
        let supervisor_id = SIMULATOR_SUPERVISOR_PROVIDER_ID.to_string();
        let controller_id = simulator_controller_provider_id("robot_v1");
        let checked = vec![
            service_participant(
                "drive",
                vec![motor_command(graph_check::Direction::Publish)],
            ),
            driver_participant(
                "ddsm115",
                "left_drive",
                vec![motor_command(graph_check::Direction::Subscribe)],
            ),
            graph_check::ParticipantApis {
                participant_id: supervisor_id.clone(),
                artifact_id: "webots-supervisor".to_string(),
                participant_kind: graph_check::ParticipantKind::Simulator,
                participant_class: graph_check::ParticipantClass::Checked,
                api_version: "y2026_1".to_string(),
                bus_abi: None,
                config_schema: None,
                scope: graph_check::ParticipantScope::Graph,
                contracts: Vec::new(),
            },
            simulator_controller_participant(
                &controller_id,
                vec![motor_command(graph_check::Direction::Subscribe)],
            ),
        ];
        let substitutions = simulated_component_records(&checked, &controller_id);
        let sim_participants = sim_checked_participants(&checked);
        let report = graph_check::check_plan(graph_check::CheckInput {
            participants: &sim_participants,
            robot_graph: &graph,
        });
        assert!(report.is_ok(), "{report:?}");

        let plan = build_launch_plan(
            LaunchMode::Sim,
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
        assert_eq!(staged.spawn_descriptors[0].name, "robot_v1");
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
            service_participant(
                "drive",
                vec![motor_command(graph_check::Direction::Publish)],
            ),
            graph_check::ParticipantApis {
                participant_id: supervisor_id.clone(),
                artifact_id: "webots-supervisor".to_string(),
                participant_kind: graph_check::ParticipantKind::Simulator,
                participant_class: graph_check::ParticipantClass::Checked,
                api_version: "y2026_1".to_string(),
                bus_abi: None,
                config_schema: None,
                scope: graph_check::ParticipantScope::Graph,
                contracts: Vec::new(),
            },
            graph_check::ParticipantApis {
                participant_id: controller_id.clone(),
                artifact_id: "webots-controller".to_string(),
                participant_kind: graph_check::ParticipantKind::Simulator,
                participant_class: graph_check::ParticipantClass::Checked,
                api_version: "y2026_1".to_string(),
                bus_abi: None,
                config_schema: None,
                scope: graph_check::ParticipantScope::Graph,
                contracts: Vec::new(),
            },
        ];
        let sim_participants = sim_checked_participants(&checked);

        let plan = build_launch_plan(
            LaunchMode::Sim,
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
            service_participant(
                "drive",
                vec![motor_command(graph_check::Direction::Publish)],
            ),
            graph_check::ParticipantApis {
                participant_id: supervisor_id.clone(),
                artifact_id: "webots-supervisor".to_string(),
                participant_kind: graph_check::ParticipantKind::Simulator,
                participant_class: graph_check::ParticipantClass::Checked,
                api_version: "y2026_1".to_string(),
                bus_abi: None,
                config_schema: None,
                scope: graph_check::ParticipantScope::Graph,
                contracts: Vec::new(),
            },
            graph_check::ParticipantApis {
                participant_id: controller_id.clone(),
                artifact_id: "webots-controller".to_string(),
                participant_kind: graph_check::ParticipantKind::Simulator,
                participant_class: graph_check::ParticipantClass::Checked,
                api_version: "y2026_1".to_string(),
                bus_abi: None,
                config_schema: None,
                scope: graph_check::ParticipantScope::Graph,
                contracts: Vec::new(),
            },
        ];
        let sim_participants = sim_checked_participants(&checked);

        let launch_plan = build_launch_plan(
            LaunchMode::Sim,
            &[CheckedRobotLaunchInput {
                project_root: temp.path(),
                resolved: &resolved,
                manifest_extras: &extras,
                checked_participants: &sim_participants,
                substitutions: &[],
                source_participants: &[],
            }],
        )?;

        let plan = SimulatePlan {
            robot_path: temp.path().join("robot.yaml"),
            project_root: temp.path().to_path_buf(),
            world_path: temp.path().join("worlds/test.wbt"),
            bus_connect: DEFAULT_ROUTER_CONNECT.to_string(),
            native_tools: native_tool_labels(SimulateOptions::default()),
            resolved,
            launch_plan,
            source_participants: Vec::new(),
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
            plan.launch_plan.robots[0]
                .substitutions
                .iter()
                .map(|substitution| substitution.component_instance.as_str())
                .collect::<Vec<_>>(),
            vec!["left_drive"]
        );
        assert_eq!(
            plan.launch_plan.robots[0].substitutions[0].provider_participant_id,
            "simulator-webots-controller-testbot"
        );
        Ok(())
    }

    #[test]
    fn custom_driver_metadata_unavailable_is_named() -> Result<()> {
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
        assert!(message.contains("packaged driver metadata"), "{message}");
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
        r#"schema: v0

robot:
  id: testbot
  namespace: test
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
  generation: y2026_1
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
        r#"schema: v0

robot:
  id: testbot
  namespace: test
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
  generation: y2026_1
  catalog: catalog.json
"#
    }

    /// Path pins are dev-overlay-only; `write_robot_project_with_component`
    /// pairs the base `robot.yaml` above with this overlay (loaded via
    /// `SimulateOptions.overlays: vec!["dev".into()]`).
    fn robot_yaml_with_component_dev_overlay() -> &'static str {
        r#"artifacts:
  pins:
    phoxal/component-ddsm115-assets:
      path: components/ddsm115
    phoxal/component-ddsm115-driver:
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
                "y2026_1",
                "0.1.0",
                CatalogChannel::Stable,
                &host_target_triple(),
                ArtifactStatus::Pending,
                Vec::new(),
            ),
            fixture_tool_entry_for_tests(
                "joypad",
                "y2026_1",
                "0.1.0",
                CatalogChannel::Stable,
                &host_target_triple(),
                ArtifactStatus::Pending,
                Vec::new(),
            ),
        ]);
        fs::write(&path, serde_json::to_string_pretty(&catalog)?)?;
        Ok(path)
    }

    fn write_driver_crate(root: &Path, name: &str) -> Result<()> {
        let dir = root.join("components").join(name);
        fs::create_dir_all(dir.join("src"))?;
        fs::write(
            dir.join("Cargo.toml"),
            format!(
                "[package]\nname = \"driver-{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"
            ),
        )?;
        fs::write(
            dir.join("src/main.rs"),
            format!(
                "fn main() {{\n    if std::env::args().nth(1).as_deref() == Some(\"emit-apis\") {{\n        println!(\"{{}}\", r#\"{{\"artifact\":{{\"kind\":\"driver\",\"id\":\"{name}\"}},\"participant_class\":\"checked\",\"api_version\":\"y2026_1\",\"required_contracts\":[{{\"family\":\"component::MotorCommand\",\"topic\":\"component/{{instance}}/motor/{{capability}}/command\",\"direction\":\"subscribe\",\"schema_id\":\"0123456789abcdef\"}}]}}\"#);\n    }}\n}}\n"
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
            api_version: "y2026_1".to_string(),
            bus_abi: None,
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
            api_version: "y2026_1".to_string(),
            bus_abi: None,
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
            api_version: "y2026_1".to_string(),
            bus_abi: None,
            config_schema: None,
            scope: graph_check::ParticipantScope::Graph,
            contracts,
        }
    }

    fn motor_command(direction: graph_check::Direction) -> graph_check::Contract {
        graph_check::Contract {
            family: "component::MotorCommand".to_string(),
            topic: "component/{instance}/motor/{capability}/command".to_string(),
            direction,
            schema_id: "schema-motor".to_string(),
        }
    }

    fn materialized_motor_command(
        instance: &str,
        direction: graph_check::Direction,
    ) -> graph_check::Contract {
        graph_check::Contract {
            family: "component::MotorCommand".to_string(),
            topic: format!("component/{instance}/motor/motor/command"),
            direction,
            schema_id: "schema-motor".to_string(),
        }
    }

    fn component_graph(instances: &[&str]) -> graph_check::RobotGraph {
        graph_check::RobotGraph {
            component_capabilities: instances
                .iter()
                .map(|instance| graph_check::ComponentCapability {
                    instance: (*instance).to_string(),
                    capability: "motor".to_string(),
                    kind: "motor".to_string(),
                })
                .collect(),
            motion_capabilities: instances
                .iter()
                .map(|instance| ((*instance).to_string(), "motor".to_string()))
                .collect(),
        }
    }

    fn empty_resolved_robot(id: &str) -> Result<ResolvedRobot> {
        let yaml = format!(
            r#"schema: v0
robot:
  id: {id}
  namespace: dev
  structure: structure.urdf
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
  components: {{}}
artifacts:
  generation: y2026_1
"#
        );
        let robot = phoxal::model::robot::v1::Robot::parse_from_string(&yaml)?;
        let generation = target_generation_for_robot(&robot, None)?;
        Ok(ResolvedRobot {
            robot,
            target_generation: generation,
            channel: phoxal::model::robot::v1::Channel::Stable,
            target: host_target_triple(),
            catalog_revision: None,
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
                assets: crate::resolver::ResolvedComponentPackage {
                    package: "phoxal/component-ddsm115-assets".to_string(),
                    kind: crate::catalog::ArtifactKind::ComponentAssets,
                    source: ResolvedComponentSource::Path {
                        path: PathBuf::from("components/ddsm115"),
                    },
                    path_override: None,
                    catalog_runtime: None,
                },
                driver: Some(crate::resolver::ResolvedComponentPackage {
                    package: "phoxal/component-ddsm115-driver".to_string(),
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

    fn platform_runtime(
        name: &str,
        contracts: Vec<crate::catalog::ContractUse>,
    ) -> ResolvedPlatformRuntime {
        ResolvedPlatformRuntime {
            name: name.to_string(),
            package: format!("phoxal/service-{name}"),
            kind: ArtifactKind::Service,
            generation: "y2026_1".to_string(),
            version: "0.1.0".to_string(),
            artifact_ref: format!(
                "service-{name}:0.1.0-y2026_1-stable-{}",
                host_target_triple()
            ),
            sha256: None,
            metadata: None,
            target_status: Some(ArtifactStatus::Pending),
            per_triple_status: BTreeMap::new(),
            changed_contracts: Vec::new(),
            contract_uses: contracts,
            path_override: None,
        }
    }

    fn simulator_runtime(name: &str) -> ResolvedPlatformRuntime {
        ResolvedPlatformRuntime {
            name: name.to_string(),
            package: format!("phoxal/simulator-{name}"),
            kind: ArtifactKind::Simulator,
            generation: "y2026_1".to_string(),
            version: "0.1.0".to_string(),
            artifact_ref: format!(
                "simulator-{name}:0.1.0-y2026_1-stable-{}",
                host_target_triple()
            ),
            sha256: None,
            metadata: None,
            target_status: Some(ArtifactStatus::Pending),
            per_triple_status: BTreeMap::new(),
            changed_contracts: Vec::new(),
            contract_uses: Vec::new(),
            path_override: None,
        }
    }

    fn add_site_tools(resolved: &mut ResolvedRobot) {
        resolved.tools.push(tool(SITE_TOOL_ROUTER));
        resolved.tools.push(tool(SITE_TOOL_JOYPAD));
    }

    fn tool(name: &str) -> ResolvedTool {
        ResolvedTool {
            name: name.to_string(),
            package: format!("phoxal/{name}"),
            requested: "0.1.0".to_string(),
            resolved: "0.1.0".to_string(),
            repo: "phoxal/framework".to_string(),
            asset: format!("{name}-0.1.0-{}.tar.gz", host_target_triple()),
            binary_name: name.to_string(),
            sha256: "0".repeat(64),
            metadata: None,
            path_override: None,
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
    /// (`~/.phoxal/run/simulation/webots/controllers/<name>/<name>`) for BOTH
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

        stage_simulator_controller_binaries(&resolved, &crate::Ui)?;

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

        let error = stage_simulator_controller_binaries(&resolved, &crate::Ui)
            .expect_err("a catalog simulator with no cached binary must error, not silently skip");
        let message = format!("{error:#}");
        assert!(
            message.contains("webots-supervisor"),
            "error should name the simulator that failed to provision: {message}"
        );

        Ok(())
    }

    /// The staging LOCATION change (Part 4 follow-up): the staged root must
    /// resolve under `~/.phoxal/run/simulation/webots` (relocatable via
    /// `PHOXAL_HOME`), never under the project tree, and each mounted
    /// component type's staged `meshes/<component_type>` entry must be a
    /// SYMLINK to the component's resolved mesh source directory - not a
    /// copy - so the cache/path-pin stays the single source of truth.
    #[test]
    fn stage_simulation_for_robot_resolves_under_home_and_symlinks_component_meshes() -> Result<()>
    {
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
            "version: v1\ncapabilities: {}\n",
        )?;
        let component_meshes_dir = temp.path().join("components/ddsm115/meshes");
        fs::create_dir_all(&component_meshes_dir)?;
        fs::write(component_meshes_dir.join("wheel.dae"), b"not a real mesh")?;

        let mut resolved = resolved_with_drive_components(&["left_drive"], false)?;
        resolved
            .simulators
            .push(simulator_runtime(SIMULATOR_SUPERVISOR_ARTIFACT_NAME));
        let extras = RobotManifestExtras::default();
        let graph = component_graph(&["left_drive"]);
        let supervisor_id = SIMULATOR_SUPERVISOR_PROVIDER_ID.to_string();
        let controller_id = simulator_controller_provider_id("robot_v1");
        let checked = vec![
            service_participant(
                "drive",
                vec![motor_command(graph_check::Direction::Publish)],
            ),
            driver_participant(
                "ddsm115",
                "left_drive",
                vec![motor_command(graph_check::Direction::Subscribe)],
            ),
            graph_check::ParticipantApis {
                participant_id: supervisor_id.clone(),
                artifact_id: "webots-supervisor".to_string(),
                participant_kind: graph_check::ParticipantKind::Simulator,
                participant_class: graph_check::ParticipantClass::Checked,
                api_version: "y2026_1".to_string(),
                bus_abi: None,
                config_schema: None,
                scope: graph_check::ParticipantScope::Graph,
                contracts: Vec::new(),
            },
            simulator_controller_participant(
                &controller_id,
                vec![motor_command(graph_check::Direction::Subscribe)],
            ),
        ];
        let substitutions = simulated_component_records(&checked, &controller_id);
        let sim_participants = sim_checked_participants(&checked);
        let report = graph_check::check_plan(graph_check::CheckInput {
            participants: &sim_participants,
            robot_graph: &graph,
        });
        assert!(report.is_ok(), "{report:?}");

        let plan = build_launch_plan(
            LaunchMode::Sim,
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
            root.ends_with("run/simulation/webots"),
            "staged root should resolve under run/simulation/webots, got {}",
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
            service_participant(
                "drive",
                vec![motor_command(graph_check::Direction::Publish)],
            ),
            graph_check::ParticipantApis {
                participant_id: supervisor_id.clone(),
                artifact_id: "webots-supervisor".to_string(),
                participant_kind: graph_check::ParticipantKind::Simulator,
                participant_class: graph_check::ParticipantClass::Checked,
                api_version: "y2026_1".to_string(),
                bus_abi: None,
                config_schema: None,
                scope: graph_check::ParticipantScope::Graph,
                contracts: Vec::new(),
            },
            graph_check::ParticipantApis {
                participant_id: controller_id.clone(),
                artifact_id: "webots-controller".to_string(),
                participant_kind: graph_check::ParticipantKind::Simulator,
                participant_class: graph_check::ParticipantClass::Checked,
                api_version: "y2026_1".to_string(),
                bus_abi: None,
                config_schema: None,
                scope: graph_check::ParticipantScope::Graph,
                contracts: Vec::new(),
            },
        ];
        let sim_participants = sim_checked_participants(&checked);
        let plan = build_launch_plan(
            LaunchMode::Sim,
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
