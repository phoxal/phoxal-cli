//! Official runtime discovery and artifact delivery planning.

use super::OfficialArtifactPlan;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::launch_plan::ParticipantExecution;
use phoxal_cli_core::project::resolver::ResolvedComponentSource;
use phoxal_cli_core::project::resolver::ResolvedPlatformRuntime;
use phoxal_cli_core::project::resolver::ResolvedRobot;
use phoxal_cli_core::project::resolver::ResolvedTool;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;

pub(crate) fn official_runtimes_including_component_drivers(
    resolved: &ResolvedRobot,
) -> Vec<&ResolvedPlatformRuntime> {
    let mut seen = BTreeSet::new();
    let mut runtimes = Vec::new();
    for runtime in &resolved.platform_runtimes {
        if seen.insert(runtime.package.clone()) {
            runtimes.push(runtime);
        }
    }
    for driver in resolved
        .components
        .iter()
        .filter_map(|component| component.driver.as_ref())
        .filter(|driver| matches!(driver.source, ResolvedComponentSource::Catalog))
    {
        if let Some(runtime) = &driver.catalog_runtime
            && seen.insert(runtime.package.clone())
        {
            runtimes.push(runtime);
        }
    }
    runtimes
}

/// Find a resolved runtime (service, simulator, or Catalog-sourced component
/// driver) by its participant/launch `artifact_id` - a driver's is the
/// component id (`runtime.name`), matching how a service's is its own name.
pub(crate) fn official_runtime_by_artifact_id<'a>(
    resolved: &'a ResolvedRobot,
    artifact_id: &str,
) -> Option<&'a ResolvedPlatformRuntime> {
    official_runtimes_including_component_drivers(resolved)
        .into_iter()
        .find(|runtime| runtime.name == artifact_id)
}

pub(crate) fn stage_official_artifacts(
    root: &Path,
    resolved: &ResolvedRobot,
    plan: &LaunchPlan,
    require_binaries: bool,
) -> Result<BTreeMap<String, OfficialArtifactPlan>> {
    let mut artifacts = BTreeMap::new();
    for runtime in official_runtimes_including_component_drivers(resolved) {
        if !plan.robots.iter().any(|robot| {
            robot.participants.iter().any(|participant| {
                participant.artifact_id == runtime.name
                    && matches!(
                        participant.execution,
                        ParticipantExecution::OfficialArtifact { .. }
                    )
            })
        }) {
            continue;
        }
        let plan = official_runtime_plan(root, runtime, require_binaries)?;
        if require_binaries && plan.source_path.is_none() {
            bail!(
                "NativePending: official artifact {} is not vendored for {}; run `phoxal update`, set PHOXAL_ARTIFACT_{}_PATH, or set PHOXAL_ARTIFACT_DIR",
                runtime.package,
                resolved.target,
                env_key(&runtime.package)
            );
        }
        artifacts.insert(runtime.package.clone(), plan);
    }
    // Every standard tool (`infrastructure-router`, `tool-joypad`,
    // `tool-telemetry`, `tool-bus`, and `tool-log`) stages the same way: locate its
    // resolved tool entry and, unless it is a local path-pin override, plan
    // its official binary. Generalized from a router-only block now that
    // deploy ships every standard tool, not just the router.
    let requested_tools = plan
        .site
        .iter()
        .map(|site| site.id.as_str())
        .chain(plan.robots.iter().flat_map(|robot| {
            robot.participants.iter().filter_map(|participant| {
                matches!(
                    participant.execution,
                    ParticipantExecution::OfficialTool { .. }
                )
                .then_some(participant.artifact_id.as_str())
            })
        }))
        .collect::<BTreeSet<_>>();
    for tool_name in requested_tools {
        let tool = resolved
            .tools
            .iter()
            .find(|tool| tool.name == tool_name)
            .with_context(|| format!("resolved deploy graph is missing tool {tool_name}"))?;
        if tool.path_override.is_some() {
            continue;
        }
        let plan = official_tool_plan(root, tool)?;
        if require_binaries && plan.source_path.is_none() {
            let key = env_key(&tool.name);
            bail!(
                "NativePending: official artifact {} is not vendored for deploy; run `phoxal update`, set PHOXAL_ARTIFACT_{key}_PATH, set PHOXAL_ARTIFACT_DIR, set PHOXAL_TOOL_{key}_PATH, or set PHOXAL_TOOL_DIR",
                tool.name,
            );
        }
        artifacts.insert(tool.name.clone(), plan);
    }
    Ok(artifacts)
}

/// The filesystem-safe projection of a provider-qualified package id
/// (`phoxal/service-drive` -> `phoxal-service-drive`), used for on-disk
/// binary/install names - a package id's `/` is not a legal path component.
pub(crate) fn filesystem_safe_package_name(package: &str) -> String {
    package.replace('/', "-")
}

pub(crate) fn official_runtime_plan(
    _root: &Path,
    runtime: &ResolvedPlatformRuntime,
    _test_stub_missing: bool,
) -> Result<OfficialArtifactPlan> {
    let descriptor = phoxal_cli_core::artifacts::NativeArtifactDescriptor::from_runtime(runtime)?;
    let install_binary_name = filesystem_safe_package_name(&runtime.package);
    let (url, size, sha256, target, archive_binary_name) = descriptor.map_or_else(
        || {
            (
                String::new(),
                0,
                runtime.sha256.clone().unwrap_or_else(|| "0".repeat(64)),
                runtime
                    .target
                    .clone()
                    .unwrap_or_else(|| "assets".to_string()),
                phoxal_cli_core::project::resolver::official_binary_name(
                    runtime.kind,
                    &runtime.name,
                ),
            )
        },
        |descriptor| {
            (
                descriptor.url,
                descriptor.size,
                descriptor.sha256,
                descriptor.target.unwrap_or_else(|| "assets".to_string()),
                descriptor.binary_name,
            )
        },
    );
    Ok(OfficialArtifactPlan {
        artifact_id: runtime.package.clone(),
        kind: runtime.kind,
        version: runtime.version.clone(),
        sha256,
        url,
        size,
        target,
        archive_binary_name,
        install_binary_name,
        source_path: None,
        missing_label: (!runtime.published).then(|| format!("{} (missing)", runtime.package)),
    })
}

pub(crate) fn official_tool_plan(
    _root: &Path,
    tool: &ResolvedTool,
) -> Result<OfficialArtifactPlan> {
    let descriptor = phoxal_cli_core::artifacts::NativeArtifactDescriptor::from_tool(tool)?;
    let (url, size, sha256, target, archive_binary_name) = descriptor.map_or_else(
        || {
            (
                String::new(),
                0,
                tool.sha256.clone(),
                tool.target.clone(),
                tool.binary_name.clone(),
            )
        },
        |descriptor| {
            (
                descriptor.url,
                descriptor.size,
                descriptor.sha256,
                descriptor.target.unwrap_or_else(|| "assets".to_string()),
                descriptor.binary_name,
            )
        },
    );
    Ok(OfficialArtifactPlan {
        artifact_id: tool.package.clone(),
        kind: tool.kind,
        version: tool.resolved.clone(),
        sha256,
        url,
        size,
        target,
        archive_binary_name,
        install_binary_name: tool.name.clone(),
        source_path: None,
        missing_label: (!tool.published).then(|| format!("{} (missing)", tool.package)),
    })
}

pub(crate) fn env_key(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}
