use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use phoxal::check as graph_check;
use serde::Serialize;
use tokio::sync::mpsc;

use crate::AppContext;
use crate::catalog::{ArtifactKind, CatalogEntry, CatalogRevision, Channel as CatalogChannel};
use crate::commands::MessageFormat;
use crate::commands::check::{
    CheckGraphContext, RawEmitApis, SourceParticipant, SourceParticipantKind,
    build_emit_apis_from_source, fetch_emit_apis_from_native_artifact,
    platform_artifact_refs_from_resolved, robot_graph_from_resolved, run_check_with_context,
    source_participants_building_only_crate, source_participants_from_resolved,
};
use crate::component_driver::component_crate_dir;
use crate::launch_plan::{
    CheckedRobotLaunchInput, DEFAULT_ROUTER_CONNECT, LaunchMode, LaunchPlan, SITE_TOOL_JOYPAD,
    SITE_TOOL_ROUTER, SubstitutionRecord, build_launch_plan,
};
use crate::resolver::{
    ResolveOptions, ResolvedComponent, ResolvedPlatformRuntime, ResolvedRobot, RobotManifestExtras,
    resolve,
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
            let plan =
                tokio::task::spawn_blocking(move || prepare(&project_root, prepared_options))
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
    let resolved = resolve_project(project_start, options.clone(), SimulateMode::DryRun)?;
    let launch_plan = build_checked_sim_launch_plan(
        &resolved.project_root,
        &resolved.resolved,
        &resolved.manifest_extras,
        resolved.catalog.as_ref(),
    )?;
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
    _mode: SimulateMode,
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

    // Always resolve live git component commits so component drivers can be
    // staged. A path-only / official-only graph needs no component network; a
    // git component pinned to a commit SHA resolves offline; a tag/branch ref is
    // resolved live via `git ls-remote` with an actionable error if the network
    // is unavailable.
    let resolved = resolve(
        &robot,
        &project_root,
        catalog.as_ref(),
        ResolveOptions {
            resolve_source_commits: true,
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
    let platform_refs = platform_artifact_refs_from_resolved(resolved);
    let official_by_ref = resolved
        .platform_runtimes
        .iter()
        .map(|runtime| (runtime.artifact_ref().to_string(), runtime))
        .collect::<BTreeMap<_, _>>();
    let catalog_driver_raws = catalog_driver_raws_by_instance(resolved, catalog)?;

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
                if let Some(raw) = catalog_driver_raws.get(participant.name.as_str()) {
                    return Ok(raw.clone());
                }
                return build_emit_apis_from_source(participant)
                    .map_err(|error| driver_metadata_unavailable(participant, error));
            }
            build_emit_apis_from_source(participant)
        },
    )?;

    let mut checked_participants = metadata_outcome.checked_participants.clone();
    remap_simulator_participant_ids(&mut checked_participants, &resolved.robot.identity.id)?;
    checked_participants.extend(official_simulator_participants(resolved)?);
    let controller_provider_id = simulator_controller_provider_id(&resolved.robot.identity.id);
    let substitutions =
        contract_substitutions_from_driver_metadata(&checked_participants, &controller_provider_id);
    let sim_participants = sim_checked_participants(&checked_participants);
    let report = graph_check::check_plan(graph_check::CheckInput {
        mode: graph_check::PlanMode::Sim,
        participants: &sim_participants,
        robot_graph: &robot_graph,
        substitutions: &substitutions,
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
            accepted_substitutions: &report.accepted_substitutions,
            source_participants: &source_participants,
        }],
    )
}

fn official_simulator_participants(
    resolved: &ResolvedRobot,
) -> Result<Vec<graph_check::ParticipantApis>> {
    let robot_id = resolved.robot.identity.id.as_str();
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

pub(crate) fn sim_source_participants(
    project_root: &Path,
    resolved: &ResolvedRobot,
    catalog: Option<&CatalogRevision>,
) -> Result<Vec<SourceParticipant>> {
    let mut participants =
        source_participants_from_resolved(project_root, resolved, |component, project_root| {
            if catalog_driver_entry(resolved, catalog, component).is_some() {
                Ok(PathBuf::from(format!(
                    "<catalog:{}>",
                    component.source_name
                )))
            } else {
                component_crate_dir(component, project_root)
            }
        })?;
    participants.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(participants)
}

fn catalog_driver_raws_by_instance(
    resolved: &ResolvedRobot,
    catalog: Option<&CatalogRevision>,
) -> Result<BTreeMap<String, RawEmitApis>> {
    let mut raws = BTreeMap::new();
    for component in resolved
        .components
        .iter()
        .filter(|component| component.has_driver)
    {
        let Some(entry) = catalog_driver_entry(resolved, catalog, component) else {
            continue;
        };
        let runtime = crate::resolver::resolved_runtime_from_entry(entry, &resolved.target)?;
        let raw = fetch_emit_apis_from_native_artifact(&runtime).with_context(|| {
            format!(
                "failed to read packaged emit-apis for catalog driver {}",
                component.source_name
            )
        })?;
        raws.insert(component.instance.clone(), raw);
    }
    Ok(raws)
}

fn catalog_driver_entry<'a>(
    resolved: &ResolvedRobot,
    catalog: Option<&'a CatalogRevision>,
    component: &ResolvedComponent,
) -> Option<&'a CatalogEntry> {
    let catalog = catalog?;
    if component.driver_path_override.is_some() {
        return None;
    }
    if !is_catalog_driver_source(component) {
        return None;
    }
    let channel = CatalogChannel::from(resolved.channel);
    catalog
        .entries
        .iter()
        .filter(|entry| entry.kind == ArtifactKind::Driver)
        .filter(|entry| entry.artifact_name() == Some(component.source_name.as_str()))
        .filter(|entry| entry.channels.contains_key(&channel))
        .filter(|entry| {
            entry
                .target_triples
                .iter()
                .any(|target| target == &resolved.target)
        })
        .filter(|entry| entry.release_assets.contains_key(&resolved.target))
        .filter(|entry| {
            !crate::catalog::compare_generations(&entry.api_generation, &resolved.target_generation)
                .is_gt()
        })
        .max_by(|left, right| {
            crate::catalog::compare_generations(&left.api_generation, &right.api_generation)
                .then_with(|| compare_versions(&left.version, &right.version))
        })
}

fn is_catalog_driver_source(component: &ResolvedComponent) -> bool {
    let crate::resolver::ResolvedComponentSource::Git { git, directory, .. } = &component.source
    else {
        return false;
    };
    let normalized_git = git.trim_end_matches(".git");
    let official_repo = normalized_git == "https://github.com/phoxal/framework"
        || normalized_git == "git@github.com:phoxal/framework";
    let expected_directory = Path::new("component").join(&component.source_name);
    official_repo && directory.as_deref() == Some(expected_directory.as_path())
}

fn compare_versions(left: &str, right: &str) -> std::cmp::Ordering {
    match (semver::Version::parse(left), semver::Version::parse(right)) {
        (Ok(left), Ok(right)) => left.cmp(&right),
        _ => left.cmp(right),
    }
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

pub(crate) fn contract_substitutions_from_driver_metadata(
    participants: &[graph_check::ParticipantApis],
    provider_participant_id: &str,
) -> Vec<graph_check::ContractSubstitution> {
    let mut substitutions = participants
        .iter()
        .filter(|participant| {
            participant.participant_class.is_checked()
                && participant.participant_kind == graph_check::ParticipantKind::Driver
        })
        .filter_map(|participant| match &participant.scope {
            graph_check::ParticipantScope::ComponentInstance(instance) => {
                Some(graph_check::ContractSubstitution {
                    component_instance: instance.clone(),
                    provider_participant_id: provider_participant_id.to_string(),
                    contracts: participant.contracts.clone(),
                })
            }
            graph_check::ParticipantScope::Graph => None,
        })
        .collect::<Vec<_>>();
    substitutions.sort_by(|left, right| {
        left.component_instance
            .cmp(&right.component_instance)
            .then_with(|| {
                left.provider_participant_id
                    .cmp(&right.provider_participant_id)
            })
    });
    substitutions
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
    let intended_staged_world_path =
        intended_staged_world_path(&plan.project_root, &plan.world_path);
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
/// Webots or writes staged files).
fn intended_staged_world_path(project_root: &Path, world_path: &Path) -> PathBuf {
    let world_name = world_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("default");
    crate::project::Project::new(project_root)
        .map(|project| project.staged_webots_world(world_name))
        .unwrap_or_else(|_| world_path.to_path_buf())
}

/// One line per resolved simulator artifact (supervisor + controller), naming
/// the artifact and its participant id.
fn simulator_artifact_lines(resolved: &ResolvedRobot) -> Vec<String> {
    let robot_id = resolved.robot.identity.id.as_str();
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

fn native_tool_labels(options: SimulateOptions) -> Vec<String> {
    let _ = options;
    let mut labels = vec![SITE_TOOL_ROUTER.to_string(), SITE_TOOL_JOYPAD.to_string()];
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
    stage_simulator_controller_binaries(&plan.project_root, &plan.resolved, &app.ui)?;
    let webots_path = crate::host_doctor::webots_executable_path()
        .map_err(|error| anyhow!("{error}"))
        .context("failed to locate the Webots executable for live simulate")?;
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
/// `dist/simulator/webots/controllers/<controller-name>/<controller-name>`
/// (`project.rs` names these paths; nothing populated them before this fix).
/// Webots looks up a world node's `controller "<name>"` field under exactly
/// this `controllers/<name>/<name>` path; when the executable is missing it
/// silently falls back to its own built-in `generic` controller instead of
/// running ours, so this staging step is load-bearing for live simulate, not
/// cosmetic.
///
/// For a PATH-OVERRIDDEN simulator (`runtime.source_path()` is `Some`, the
/// local-dev / live-gate case), the binary is built fresh with
/// `crate::commands::run::build_source_binary`, which runs `cargo build --bin
/// <name>` in the simulator crate and returns the built path - the same
/// mechanism every other path-overridden participant (services, tools,
/// drivers) already uses via `run.rs`.
///
/// For a CATALOG simulator (no path override), the binary is obtained the
/// same way every other official/native artifact is provisioned: resolve a
/// `NativeArtifactDescriptor` from the runtime and look it up in the artifact
/// cache via `native_artifacts::artifact_binary_path` - mirroring
/// `commands::run::locate_official_binary`. If that cache entry is missing,
/// this is a hard error (`NativePending`-style), not a silent skip: a missing
/// controller binary must never be allowed to fall through to Webots'
/// `generic` controller unnoticed.
fn stage_simulator_controller_binaries(
    project_root: &Path,
    resolved: &ResolvedRobot,
    ui: &crate::Ui,
) -> Result<()> {
    let project = crate::project::Project::new(project_root)?;
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
        let built_binary = if let Some(crate_dir) = runtime.source_path() {
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
        let staged_dir = project.staged_webots_controller_dir(controller_name);
        std::fs::create_dir_all(&staged_dir).with_context(|| {
            format!(
                "failed to create staged controller directory {}",
                staged_dir.display()
            )
        })?;
        let staged_binary = staged_dir.join(controller_name);
        std::fs::copy(&built_binary, &staged_binary).with_context(|| {
            format!(
                "failed to copy simulator binary {} to staged controller path {}",
                built_binary.display(),
                staged_binary.display()
            )
        })?;
        crate::utils::make_executable(&staged_binary).with_context(|| {
            format!(
                "failed to mark staged controller binary executable: {}",
                staged_binary.display()
            )
        })?;
        ui.info(format!(
            "staged simulator controller binary {} at {}",
            runtime.name,
            staged_binary.display()
        ));
    }
    Ok(())
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
    let robot_id = &resolved.robot.identity.id;
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

    let structure_path = project_root.join(&resolved.robot.structure);
    let structure = phoxal::model::structure::Structure::read_from_file(&structure_path)
        .with_context(|| {
            format!(
                "failed to read robot structure declared by robot.yaml structure: {}",
                resolved.robot.structure.display()
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
        let crate_dir = component_crate_dir(component, project_root)?;
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

    let project = crate::project::Project::new(project_root)?;
    stage_simulation_world(
        &base_world_text,
        &project.staged_webots_protos_dir(),
        &project.staged_webots_meshes_dir(),
        &project.staged_webots_world(world_name),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{
        ArtifactStatus, Channel as CatalogChannel, fixture_catalog_for_tests,
        fixture_contract_for_tests, fixture_tool_entry_for_tests,
    };
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
            framework: "y2026_1".to_string(),
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
                accepted_substitutions: &[],
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
                accepted_substitutions: &[],
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
        let substitutions = contract_substitutions_from_driver_metadata(&checked, &controller_id);
        let sim_participants = sim_checked_participants(&checked);
        let report = graph_check::check_plan(graph_check::CheckInput {
            mode: graph_check::PlanMode::Sim,
            participants: &sim_participants,
            robot_graph: &graph,
            substitutions: &substitutions,
        });
        assert!(report.is_ok(), "{report:?}");

        let plan = build_launch_plan(
            LaunchMode::Sim,
            &[CheckedRobotLaunchInput {
                project_root: temp.path(),
                resolved: &resolved,
                manifest_extras: &extras,
                checked_participants: &sim_participants,
                accepted_substitutions: &report.accepted_substitutions,
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
        let graph = component_graph(&["left_drive", "right_drive"]);
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
            driver_participant(
                "ddsm115",
                "right_drive",
                vec![motor_command(graph_check::Direction::Subscribe)],
            ),
            simulator_controller_participant(
                &controller_id,
                vec![motor_command(graph_check::Direction::Subscribe)],
            ),
        ];
        let substitutions = contract_substitutions_from_driver_metadata(&checked, &controller_id);
        let sim_participants = sim_checked_participants(&checked);
        let report = graph_check::check_plan(graph_check::CheckInput {
            mode: graph_check::PlanMode::Sim,
            participants: &sim_participants,
            robot_graph: &graph,
            substitutions: &substitutions,
        });

        assert!(report.is_ok(), "{report:?}");
        let topics = report
            .accepted_substitutions
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
    fn provider_not_covering_instance_surfaces_core_failure() {
        let graph = component_graph(&["left_drive", "right_drive"]);
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
            driver_participant(
                "ddsm115",
                "right_drive",
                vec![motor_command(graph_check::Direction::Subscribe)],
            ),
            simulator_controller_participant(
                &controller_id,
                vec![materialized_motor_command(
                    "left_drive",
                    graph_check::Direction::Subscribe,
                )],
            ),
        ];
        let substitutions = contract_substitutions_from_driver_metadata(&checked, &controller_id);
        let sim_participants = sim_checked_participants(&checked);
        let report = graph_check::check_plan(graph_check::CheckInput {
            mode: graph_check::PlanMode::Sim,
            participants: &sim_participants,
            robot_graph: &graph,
            substitutions: &substitutions,
        });

        assert!(matches!(
            report.problems.as_slice(),
            [graph_check::Problem::IncompleteSubstitution {
                component_instance,
                missing_contracts,
                ..
            }] if component_instance == "right_drive"
                && missing_contracts
                    .iter()
                    .any(|contract| contract.topic == "component/right_drive/motor/motor/command")
        ));
    }

    #[test]
    fn deploy_plan_with_substitution_is_a_hard_error() {
        let substitution = graph_check::ContractSubstitution {
            component_instance: "left_drive".to_string(),
            provider_participant_id: simulator_controller_provider_id("robot_v1"),
            contracts: vec![motor_command(graph_check::Direction::Subscribe)],
        };
        let report = graph_check::check_plan(graph_check::CheckInput {
            mode: graph_check::PlanMode::Deploy,
            participants: &[],
            robot_graph: &graph_check::RobotGraph::default(),
            substitutions: &[substitution],
        });

        assert!(matches!(
            report.problems.as_slice(),
            [graph_check::Problem::SubstitutionNotAllowed {
                mode: graph_check::PlanMode::Deploy,
                component_instance,
                ..
            }] if component_instance == "left_drive"
        ));
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
        remap_simulator_participant_ids(&mut checked, &resolved.robot.identity.id)?;

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
                accepted_substitutions: &[],
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
        let accepted = vec![graph_check::AcceptedSubstitution {
            component_instance: "left_drive".to_string(),
            provider_participant_id: controller_id.clone(),
            provider_artifact_id: "webots-controller".to_string(),
            provider_kind: graph_check::ParticipantKind::Simulator,
            contracts: vec![materialized_motor_command(
                "left_drive",
                graph_check::Direction::Subscribe,
            )],
        }];
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
                accepted_substitutions: &accepted,
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
        let substitutions = contract_substitutions_from_driver_metadata(&checked, &controller_id);
        let sim_participants = sim_checked_participants(&checked);
        let report = graph_check::check_plan(graph_check::CheckInput {
            mode: graph_check::PlanMode::Sim,
            participants: &sim_participants,
            robot_graph: &graph,
            substitutions: &substitutions,
        });
        assert!(report.is_ok(), "{report:?}");

        let plan = build_launch_plan(
            LaunchMode::Sim,
            &[CheckedRobotLaunchInput {
                project_root: temp.path(),
                resolved: &resolved,
                manifest_extras: &extras,
                checked_participants: &sim_participants,
                accepted_substitutions: &report.accepted_substitutions,
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
        let substitutions = contract_substitutions_from_driver_metadata(&checked, &controller_id);
        let sim_participants = sim_checked_participants(&checked);
        let report = graph_check::check_plan(graph_check::CheckInput {
            mode: graph_check::PlanMode::Sim,
            participants: &sim_participants,
            robot_graph: &graph,
            substitutions: &substitutions,
        });
        assert!(report.is_ok(), "{report:?}");

        let plan = build_launch_plan(
            LaunchMode::Sim,
            &[CheckedRobotLaunchInput {
                project_root: temp.path(),
                resolved: &resolved,
                manifest_extras: &extras,
                checked_participants: &sim_participants,
                accepted_substitutions: &report.accepted_substitutions,
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
                accepted_substitutions: &[],
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
                accepted_substitutions: &[],
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
    fn real_sim_plan_with_component_names_missing_simulator_provider() -> Result<()> {
        let temp = tempfile::tempdir()?;
        write_robot_project_with_component(temp.path())?;
        let catalog_path = write_catalog_with_driver(temp.path())?;

        let error = prepare(
            temp.path(),
            SimulateOptions {
                world: "test".to_string(),
                catalog_source: Some(catalog_path.display().to_string()),
                ..SimulateOptions::default()
            },
        )
        .expect_err("no simulator provider should fail the sim check");
        let message = error.to_string();
        assert!(message.contains("NoSimulatorProvider"), "{message}");
        assert!(message.contains("left_drive"), "{message}");
        assert!(
            message.contains("simulator-webots-controller-testbot"),
            "{message}"
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
api_version: y2026_1

identity:
  id: testbot
  namespace: test

structure: structure.urdf

phoxal_artifacts:
  channel: stable
  catalog: catalog.json
phoxal_participants: {}

motion:
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5

components:
  sources: {}
  instances: {}
"#
    }

    fn write_robot_project_with_component(root: &Path) -> Result<()> {
        fs::write(root.join("robot.yaml"), robot_yaml_with_component())?;
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

identity:
  id: testbot
  namespace: test

structure: structure.urdf

phoxal_artifacts:
  channel: stable
  generation: y2026_1
  catalog: catalog.json
phoxal_participants: {}

motion:
  kinematic:
    kind: omnidirectional
    actuators: [left_drive.motor]
    encoders: []

components:
  sources:
    ddsm115:
      path: components/ddsm115
  instances:
    left_drive:
      component: ddsm115
      mount_link: left_wheel
      parameters:
        motor:
          kind: motor
      driver:
        connection: { type: can, bus: 0, node_id: 1 }
"#
    }

    fn robot_yaml_with_custom_component() -> &'static str {
        r#"schema: v0

identity:
  id: testbot
  namespace: test

structure: structure.urdf

phoxal_artifacts:
  channel: stable
  generation: y2026_1
  catalog: catalog.json
phoxal_participants: {}

motion:
  kinematic:
    kind: omnidirectional
    actuators: [left_drive.motor]
    encoders: []

components:
  sources:
    ddsm115:
      path: components/ddsm115
  instances:
    left_drive:
      component: ddsm115
      mount_link: left_wheel
      parameters:
        motor:
          kind: motor
      driver:
        connection: { type: can, bus: 0, node_id: 1 }
"#
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
api_version: y2026_1
identity:
  id: {id}
  namespace: dev
structure: structure.urdf
phoxal_participants: {{}}
motion:
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
components:
  sources: {{}}
  instances: {{}}
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
                framework: "y2026_1".to_string(),
                source_hash: "hash".to_string(),
            });
        }
        for instance in instances {
            resolved.components.push(ResolvedComponent {
                instance: (*instance).to_string(),
                source_name: "ddsm115".to_string(),
                source: ResolvedComponentSource::Path {
                    path: PathBuf::from("components/ddsm115"),
                },
                has_driver: true,
                driver_path_override: None,
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
            artifact_id: format!("service-{name}"),
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
            artifact_id: format!("simulator-{name}"),
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
    /// (`dist/simulator/webots/controllers/<name>/<name>`) for BOTH the
    /// supervisor and the per-robot controller when the simulators are
    /// path-overridden (the live-gate case), by actually running `cargo
    /// build` against fake stand-in crates and copying the result - not just
    /// asserting a path string was computed.
    #[test]
    fn path_overridden_simulators_are_built_and_staged_as_webots_controllers() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let project_root = temp.path().join("project");
        fs::create_dir_all(&project_root)?;

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

        stage_simulator_controller_binaries(&project_root, &resolved, &crate::Ui)?;

        let project = crate::project::Project::new(&project_root)?;
        let supervisor_binary = project.staged_webots_supervisor_binary();
        let controller_binary = project.staged_webots_controller_binary();

        for binary in [&supervisor_binary, &controller_binary] {
            assert!(
                binary.is_file(),
                "expected staged controller binary to exist at {}",
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
        let temp = tempfile::tempdir()?;
        let mut resolved = resolved_with_drive_components(&[], false)?;
        resolved.simulators.clear();
        resolved
            .simulators
            .push(simulator_runtime(SIMULATOR_SUPERVISOR_ARTIFACT_NAME));

        let error = stage_simulator_controller_binaries(temp.path(), &resolved, &crate::Ui)
            .expect_err("a catalog simulator with no cached binary must error, not silently skip");
        let message = format!("{error:#}");
        assert!(
            message.contains("webots-supervisor"),
            "error should name the simulator that failed to provision: {message}"
        );

        Ok(())
    }
}
