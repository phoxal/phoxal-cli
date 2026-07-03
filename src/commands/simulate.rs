use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use phoxal::check as graph_check;
use serde::Serialize;

use crate::AppContext;
use crate::catalog::{ArtifactKind, CatalogEntry, CatalogRevision, Channel as CatalogChannel};
use crate::commands::MessageFormat;
use crate::commands::check::{
    CheckGraphContext, RawArtifact, RawContract, RawEmitApis, SourceParticipant,
    SourceParticipantKind, build_emit_apis_from_source, official_emit_apis_from_catalog_metadata,
    platform_artifact_refs_from_resolved, robot_graph_from_resolved, run_check_with_context,
    source_participants_from_resolved,
};
use crate::component_driver::component_crate_dir;
use crate::launch_plan::{
    CheckedRobotLaunchInput, DEFAULT_ROUTER_CONNECT, LaunchMode, LaunchPlan, SITE_TOOL_JOYPAD,
    SITE_TOOL_ROUTER, SubstitutionRecord, build_launch_plan,
};
use crate::resolver::{
    ResolveOptions, ResolvedComponent, ResolvedRobot, RobotManifestExtras, resolve,
};
use crate::supervisor::{
    BoardBackend, ParticipantKind, ParticipantState, ParticipantStatus, RouterOwnership,
    SupervisorLock, SupervisorOptions, default_connect_endpoint, local_router_reachable,
    router_ownership, start_bus_log_subscriber, supervise_until_shutdown, supervisor_state_path,
};
use crate::world;

pub(crate) const SIMULATOR_PROVIDER_ID: &str = "Simulator";

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
}

struct ResolvedSimulation {
    robot_path: PathBuf,
    project_root: PathBuf,
    world_path: PathBuf,
    resolved: ResolvedRobot,
    manifest_extras: RobotManifestExtras,
    catalog: Option<CatalogRevision>,
}

impl Simulate {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let options = SimulateOptions {
            world: self.world.clone(),
            joypad: self.joypad,
            pull: self.pull,
            catalog_source: app.catalog_source.clone(),
            message_format: self.message_format,
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

            let run_dir = crate::host_paths::run_dir()?;
            let _lock = SupervisorLock::acquire(&run_dir)?;
            let state_file = supervisor_state_path()?;
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

            let outcome = supervise_until_shutdown(
                specs,
                board.clone(),
                SupervisorOptions {
                    state_file: Some(state_file),
                    ..SupervisorOptions::default()
                },
            )
            .await?;

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
    Ok(SimulatePlan {
        robot_path: resolved.robot_path,
        project_root: resolved.project_root,
        world_path: resolved.world_path,
        bus_connect: DEFAULT_ROUTER_CONNECT.to_string(),
        native_tools: native_tool_labels(options),
        resolved: resolved.resolved,
        launch_plan,
    })
}

fn resolve_project(
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
    let loaded = crate::resolver::load_robot_with_extras(&robot_path)?;
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

    // Always resolve live: simulate does not pin tool checksums, but it does
    // resolve git component commits so component drivers can be staged. A
    // path-only / official-only graph needs no network; a git component pinned
    // to a commit SHA resolves offline; a tag/branch ref is resolved live via
    // `git ls-remote` (with an actionable error if the network is unavailable).
    let resolved = resolve(
        &robot,
        &project_root,
        catalog.as_ref(),
        ResolveOptions {
            resolve_external_artifacts: false,
            resolve_source_commits: true,
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

fn build_checked_sim_launch_plan(
    project_root: &Path,
    resolved: &ResolvedRobot,
    manifest_extras: &RobotManifestExtras,
    catalog: Option<&CatalogRevision>,
) -> Result<LaunchPlan> {
    let robot_graph = robot_graph_from_resolved(resolved);
    let source_participants = sim_source_participants(project_root, resolved, catalog)
        .with_context(|| "failed to prepare source participants for simulation metadata")?;
    let platform_refs = platform_artifact_refs_from_resolved(resolved);
    let official_by_ref = resolved
        .platform_runtimes
        .iter()
        .map(|runtime| (runtime.artifact_ref().to_string(), runtime))
        .collect::<BTreeMap<_, _>>();
    let catalog_driver_raws = catalog_driver_raws_by_instance(resolved, catalog);

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
            Ok(official_emit_apis_from_catalog_metadata(runtime))
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

    let substitutions = contract_substitutions_from_driver_metadata(
        &metadata_outcome.checked_participants,
        SIMULATOR_PROVIDER_ID,
    );
    let sim_participants = sim_checked_participants(&metadata_outcome.checked_participants);
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

    for warning in &report.warnings {
        eprintln!("warning: {warning:?}");
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

fn sim_source_participants(
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
) -> BTreeMap<String, RawEmitApis> {
    resolved
        .components
        .iter()
        .filter(|component| component.has_driver)
        .filter_map(|component| {
            catalog_driver_entry(resolved, catalog, component).map(|entry| {
                (
                    component.instance.clone(),
                    RawEmitApis {
                        artifact: RawArtifact {
                            kind: "driver".to_string(),
                            id: component.source_name.clone(),
                        },
                        participant_class: "checked".to_string(),
                        api_version: entry.api_generation.clone(),
                        bus_abi: None,
                        required_contracts: entry
                            .contract_uses
                            .iter()
                            .map(|contract| RawContract {
                                family: contract.family.clone(),
                                topic: contract.topic_template.clone(),
                                direction: contract.direction.clone(),
                                schema_id: contract.schema_id.clone(),
                            })
                            .collect(),
                        config_schema: None,
                    },
                )
            })
        })
        .collect()
}

fn catalog_driver_entry<'a>(
    resolved: &ResolvedRobot,
    catalog: Option<&'a CatalogRevision>,
    component: &ResolvedComponent,
) -> Option<&'a CatalogEntry> {
    let catalog = catalog?;
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
    let substitutions = substitution_lines(&plan.launch_plan);
    let output = SimulateDryRunOutput {
        mode: "dry-run",
        target_generation: plan.resolved.target_generation.clone(),
        channel: plan.resolved.channel.to_string(),
        catalog_revision: plan.resolved.catalog_revision.clone(),
        world_path: plan.world_path.clone(),
        bus_connect: plan.bus_connect.clone(),
        platform_service_count: plan.resolved.platform_runtimes.len(),
        native_tools: plan.native_tools.clone(),
        substitutions: substitutions.clone(),
    };
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
            if !substitutions.is_empty() {
                println!("substitutions:");
                for substitution in &substitutions {
                    println!("  - {substitution}");
                }
            }
            println!("dry-run - no files written and no simulation processes started");
            Ok(())
        },
        message_format,
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{
        ArtifactStatus, Channel as CatalogChannel, fixture_catalog_for_tests,
        fixture_contract_for_tests, fixture_driver_entry_for_tests,
        fixture_service_entry_for_tests,
    };
    use crate::resolver::{
        ResolvedComponent, ResolvedComponentSource, ResolvedPlatformRuntime, ResolvedTool,
        ResolvedUserRuntime, host_target_triple, target_generation_for_robot,
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
            simulator_participant(vec![motor_command(graph_check::Direction::Subscribe)]),
        ];
        let substitutions =
            contract_substitutions_from_driver_metadata(&checked, SIMULATOR_PROVIDER_ID);
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
            vec!["component/left_drive/* : satisfied by Simulator (webots)"]
        );
        assert_eq!(participant_ids(&plan), vec!["drive"]);
        Ok(())
    }

    #[test]
    fn two_identical_instances_get_disjoint_substitution_sets() {
        let graph = component_graph(&["left_drive", "right_drive"]);
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
            simulator_participant(vec![motor_command(graph_check::Direction::Subscribe)]),
        ];
        let substitutions =
            contract_substitutions_from_driver_metadata(&checked, SIMULATOR_PROVIDER_ID);
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
            simulator_participant(vec![materialized_motor_command(
                "left_drive",
                graph_check::Direction::Subscribe,
            )]),
        ];
        let substitutions =
            contract_substitutions_from_driver_metadata(&checked, SIMULATOR_PROVIDER_ID);
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
            provider_participant_id: SIMULATOR_PROVIDER_ID.to_string(),
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
    fn sim_launch_set_matches_checked_robot_participants_without_drivers() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let resolved = resolved_with_drive_components(&["left_drive"], true)?;
        let extras = RobotManifestExtras::default();
        let checked = vec![
            service_participant("drive", Vec::new()),
            service_participant("mission", Vec::new()),
            driver_participant(
                "ddsm115",
                "left_drive",
                vec![motor_command(graph_check::Direction::Subscribe)],
            ),
            simulator_participant(vec![motor_command(graph_check::Direction::Subscribe)]),
        ];
        let accepted = vec![graph_check::AcceptedSubstitution {
            component_instance: "left_drive".to_string(),
            provider_participant_id: SIMULATOR_PROVIDER_ID.to_string(),
            provider_artifact_id: "webots".to_string(),
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

        assert_eq!(participant_ids(&plan), vec!["drive", "mission"]);
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
        assert!(message.contains("Simulator"), "{message}");
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
phoxal_participants: {}

motion:
  kinematic:
    kind: omnidirectional
    actuators: [left_drive.motor]
    encoders: []

components:
  sources:
    ddsm115:
      git: https://github.com/phoxal/framework
      tag: 0123456789012345678901234567890123456789
      directory: component/ddsm115
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
        let path = root.join("catalog.json");
        let catalog = fixture_catalog_for_tests(vec![
            fixture_service_entry_for_tests(
                "drive",
                "y2026_1",
                "0.1.0",
                CatalogChannel::Stable,
                &host_target_triple(),
                ArtifactStatus::Pending,
                vec![fixture_contract_for_tests(
                    "component::MotorCommand",
                    "component/{instance}/motor/{capability}/command",
                    "publish",
                    "0123456789abcdef",
                )],
            ),
            fixture_driver_entry_for_tests(
                "ddsm115",
                "y2026_1",
                "0.1.0",
                CatalogChannel::Stable,
                &host_target_triple(),
                ArtifactStatus::Pending,
                vec![fixture_contract_for_tests(
                    "component::MotorCommand",
                    "component/{instance}/motor/{capability}/command",
                    "subscribe",
                    "0123456789abcdef",
                )],
            ),
        ]);
        fs::write(&path, serde_json::to_string_pretty(&catalog)?)?;
        Ok(path)
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

    fn simulator_participant(
        contracts: Vec<graph_check::Contract>,
    ) -> graph_check::ParticipantApis {
        graph_check::ParticipantApis {
            participant_id: SIMULATOR_PROVIDER_ID.to_string(),
            artifact_id: "webots".to_string(),
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
            user_runtimes: Vec::new(),
            components: Vec::new(),
            tools: Vec::new(),
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
            target_status: Some(ArtifactStatus::Pending),
            per_triple_status: BTreeMap::new(),
            changed_contracts: Vec::new(),
            contract_uses: contracts,
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
        }
    }
}
