//! Robot resolution, suite hydration, and deployment-plan preparation.

use super::{
    DeployOptions, RenderPayloadInput, RenderedPayload, TargetTriples,
    ensure_no_native_c_source_dependencies, official_runtimes_including_component_drivers,
    render_payload,
};
use crate::check::CheckGraphContext;
use crate::check::build_emit_apis_from_source;
use crate::check::check_artifact_refs_from_resolved;
use crate::check::extract_emit_apis_from_staged_runtime;
use crate::check::extract_emit_apis_from_staged_tool;
use crate::check::fetch_emit_apis_from_tool;
use crate::check::run_check_with_context;
use crate::check::source_participants_from_resolved;
use crate::component_driver::component_driver_crate_dir;
use crate::resolver::resolve;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use phoxal_cli_core::check::source::SourceParticipantKind;
use phoxal_cli_core::project::launch_plan::CheckedRobotLaunchInput;
use phoxal_cli_core::project::launch_plan::LaunchMode;
use phoxal_cli_core::project::launch_plan::PlanContext;
use phoxal_cli_core::project::launch_plan::build_launch_plan;
use phoxal_cli_core::project::resolver::ResolveOptions;
use phoxal_cli_core::project::resolver::ResolvedPlatformRuntime;
use phoxal_cli_core::project::resolver::ResolvedRobot;
use phoxal_cli_core::project::resolver::discover_robot_yaml;
use phoxal_cli_core::project::resolver::load_robot;
use phoxal_cli_core::project::suite::ArtifactKind;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

pub(crate) fn prepare_deploy(
    project_start: &Path,
    options: &DeployOptions,
    target: TargetTriples,
    _require_official_binaries: bool,
    remote_user: &str,
    ui: &crate::Ui,
) -> Result<RenderedPayload> {
    let robot_path = discover_robot_yaml(project_start)
        .with_context(|| format!("failed to find robot.yaml from {}", project_start.display()))?;
    let project_root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?;
    let robot = load_robot(&robot_path)?;
    let suite = crate::commands::load_suite_for_robot_from_source(
        options.suite_source.clone(),
        project_root,
    )?;
    let mut resolved = resolve(
        &robot,
        project_root,
        suite.as_ref(),
        ResolveOptions {
            official_target_triple: Some(target.official_triple.clone()),
            tool_target_triple: Some(target.official_triple.clone()),
        },
    )?;
    if let Some(suite) = suite.as_ref() {
        hydrate_suite_blobs(&mut resolved, suite)?;
    }
    if !options.dry_run {
        let mut host_resolved = resolve(
            &robot,
            project_root,
            suite.as_ref(),
            ResolveOptions {
                official_target_triple: Some(crate::resolver::host_target_triple()),
                tool_target_triple: Some(crate::resolver::host_target_triple()),
            },
        )?;
        let suite = suite
            .as_ref()
            .context("deploy requires the pinned suite to recover immutable release URLs")?;
        hydrate_suite_blobs(&mut host_resolved, suite)?;
        ensure_cross_target_release_match(&resolved, &host_resolved)?;
        let descriptors = phoxal_cli_core::artifacts::descriptors_for(&host_resolved, false, true)?;
        crate::native_artifacts::prepare_descriptors_with_preflight(&descriptors, Some(ui))?;
        prepare_deploy_after_host_staging(
            project_root,
            options,
            target,
            remote_user,
            ui,
            &robot,
            robot_path.clone(),
            resolved,
            host_resolved,
        )
    } else {
        prepare_deploy_after_host_staging(
            project_root,
            options,
            target,
            remote_user,
            ui,
            &robot,
            robot_path.clone(),
            resolved.clone(),
            resolved,
        )
    }
}

pub(crate) fn hydrate_suite_blobs(
    resolved: &mut ResolvedRobot,
    suite: &phoxal_cli_core::project::suite::Suite,
) -> Result<()> {
    fn hydrate_runtime(
        runtime: &mut ResolvedPlatformRuntime,
        suite: &phoxal_cli_core::project::suite::Suite,
    ) -> Result<()> {
        if runtime.path_override.is_some() || runtime.version == "source" {
            return Ok(());
        }
        let artifact = suite
            .artifacts
            .iter()
            .find(|artifact| artifact.id == runtime.package && suite.version == runtime.version)
            .with_context(|| {
                format!(
                    "pinned suite is missing {} {}",
                    runtime.package, runtime.version
                )
            })?;
        let blob = if runtime.kind == ArtifactKind::ComponentAssets {
            artifact.assets.as_ref()
        } else {
            runtime
                .target
                .as_ref()
                .and_then(|target| artifact.targets.get(target))
        };
        let Some(blob) = blob else {
            runtime.published = false;
            return Ok(());
        };
        runtime.sha256 = Some(blob.sha256.clone());
        runtime.url = Some(blob.url.clone());
        runtime.size = Some(blob.size);
        runtime.published = true;
        runtime.published_triples = artifact.targets.keys().cloned().collect();
        Ok(())
    }

    for runtime in &mut resolved.platform_runtimes {
        hydrate_runtime(runtime, suite)?;
    }
    for component in &mut resolved.components {
        if let Some(runtime) = component
            .assets
            .as_mut()
            .and_then(|assets| assets.suite_runtime.as_mut())
        {
            hydrate_runtime(runtime, suite)?;
        }
        if let Some(runtime) = component
            .driver
            .as_mut()
            .and_then(|driver| driver.suite_runtime.as_mut())
        {
            hydrate_runtime(runtime, suite)?;
        }
    }
    for tool in &mut resolved.tools {
        if tool.path_override.is_some() || tool.resolved == "source" {
            continue;
        }
        let artifact = suite
            .artifacts
            .iter()
            .find(|artifact| artifact.id == tool.package && suite.version == tool.resolved)
            .with_context(|| {
                format!("pinned suite is missing {} {}", tool.package, tool.resolved)
            })?;
        let Some(blob) = artifact.targets.get(&tool.target) else {
            tool.published = false;
            continue;
        };
        tool.asset = blob.url.clone();
        tool.sha256 = blob.sha256.clone();
        tool.url = Some(blob.url.clone());
        tool.size = Some(blob.size);
        tool.published = true;
    }
    Ok(())
}

pub(crate) fn ensure_cross_target_release_match(
    robot: &ResolvedRobot,
    host: &ResolvedRobot,
) -> Result<()> {
    let robot_versions = official_runtimes_including_component_drivers(robot)
        .into_iter()
        .map(|runtime| (runtime.package.as_str(), runtime.version.as_str()))
        .chain(
            robot
                .tools
                .iter()
                .map(|tool| (tool.package.as_str(), tool.resolved.as_str())),
        )
        .collect::<BTreeMap<_, _>>();
    let host_versions = official_runtimes_including_component_drivers(host)
        .into_iter()
        .map(|runtime| (runtime.package.as_str(), runtime.version.as_str()))
        .chain(
            host.tools
                .iter()
                .map(|tool| (tool.package.as_str(), tool.resolved.as_str())),
        )
        .collect::<BTreeMap<_, _>>();
    for (package, robot_version) in robot_versions {
        let host_version = host_versions
            .get(package)
            .with_context(|| format!("host-target coherence evidence is missing {package}"))?;
        if *host_version != robot_version {
            bail!(
                "CrossTargetReleaseMismatch: robot target resolved {package} {robot_version}, but host-target coherence evidence resolved {host_version}; run `phoxal update` for both target scopes before deploy"
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_deploy_after_host_staging(
    project_root: &Path,
    options: &DeployOptions,
    target: TargetTriples,
    remote_user: &str,
    ui: &crate::Ui,
    robot: &phoxal::model::robot::RobotV0,
    robot_path: PathBuf,
    resolved: ResolvedRobot,
    host_resolved: ResolvedRobot,
) -> Result<RenderedPayload> {
    let all_source_participants =
        source_participants_from_resolved(project_root, &resolved, component_driver_crate_dir)?;
    let checked_source_participants = all_source_participants
        .iter()
        .filter(|participant| participant.kind != SourceParticipantKind::Simulator)
        .cloned()
        .collect::<Vec<_>>();
    let coherence_source_participants = checked_source_participants.clone();
    ensure_no_native_c_source_dependencies(&checked_source_participants)?;
    let platform_refs = check_artifact_refs_from_resolved(&host_resolved);
    let tool_participants = crate::check::tool_participants_from_resolved(&host_resolved)?;
    let mut official_by_ref = host_resolved
        .platform_runtimes
        .iter()
        .map(|runtime| (runtime.artifact_ref().to_string(), runtime))
        .collect::<BTreeMap<_, _>>();
    official_by_ref.extend(crate::check::component_driver_runtimes_by_ref(
        &host_resolved,
    ));
    let tools_by_ref = host_resolved
        .tools
        .iter()
        .map(|tool| (tool.asset.clone(), tool))
        .collect::<BTreeMap<_, _>>();
    let outcome = run_check_with_context(
        &platform_refs,
        &tool_participants,
        &coherence_source_participants,
        CheckGraphContext { robot: Some(robot) },
        |artifact_ref| {
            if let Some(runtime) = official_by_ref.get(artifact_ref) {
                return extract_emit_apis_from_staged_runtime(runtime);
            }
            if let Some(tool) = tools_by_ref.get(artifact_ref) {
                return extract_emit_apis_from_staged_tool(tool);
            }
            Err(anyhow!(
                "resolved official artifact {artifact_ref} is not in the suite"
            ))
        },
        fetch_emit_apis_from_tool,
        build_emit_apis_from_source,
    )?;
    crate::check::ensure_check_outcome_ok(&resolved.train, &outcome)?;

    let plan = build_launch_plan(
        LaunchMode::Deploy,
        &[CheckedRobotLaunchInput {
            project_root,
            resolved: &resolved,
            checked_participants: &outcome.checked_participants,
            substitutions: &[],
            source_participants: &checked_source_participants,
        }],
    )?;
    let coherence_graph =
        crate::check::robot_contract_surfaces(&resolved.robot.robot.id, &outcome.contract_surfaces);
    let coherence = crate::check::coherence_for_launch_plan(&plan, &[coherence_graph])?;
    crate::check::enforce_coherence(crate::check::CoherenceVerb::Deploy, &coherence)?;

    let project_root = project_root.to_path_buf();
    let ctx = PlanContext {
        robot_path,
        project_root,
        resolved,
        source_participants: all_source_participants,
    };

    render_payload(RenderPayloadInput {
        robot,
        ctx: &ctx,
        plan: &plan,
        target,
        health_timeout: options.health_timeout,
        remote_user,
        ui,
    })
}
