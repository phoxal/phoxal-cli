//! Participants responsibilities for check.

use super::{PlatformArtifactRef, tool_env_override};
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use phoxal_cli_core::check::source::SourceParticipant;
use phoxal_cli_core::check::source::ToolParticipant;
use phoxal_cli_core::project::resolver::ResolvedComponent;
use phoxal_cli_core::project::resolver::ResolvedComponentSource;
use phoxal_cli_core::project::resolver::ResolvedPlatformRuntime;
use phoxal_cli_core::project::resolver::ResolvedRobot;
use phoxal_cli_core::project::resolver::tool_emit_apis_id;
use phoxal_cli_core::project::suite::ArtifactKind;
use phoxal_cli_core::project::tooling::resolve_project_path;
use std::path::Path;
use std::path::PathBuf;

pub(crate) fn tool_participants_from_resolved(
    resolved: &ResolvedRobot,
) -> Result<Vec<ToolParticipant>> {
    resolved
        .tools
        .iter()
        .filter(|tool| tool.kind == ArtifactKind::Tool)
        .filter_map(|tool| {
            if tool.path_override.is_some() {
                None
            } else {
                tool_env_override(tool).map(|path| {
                    Ok(ToolParticipant {
                        name: tool.name.clone(),
                        binary_path: path,
                    })
                })
            }
        })
        .collect()
}

pub(crate) fn platform_artifact_refs_from_resolved(
    resolved: &ResolvedRobot,
) -> Vec<PlatformArtifactRef> {
    resolved
        .platform_runtimes
        .iter()
        .filter(|runtime| runtime.source_path().is_none())
        .map(|runtime| PlatformArtifactRef {
            name: runtime.name.clone(),
            kind: runtime.kind,
            artifact_ref: runtime.artifact_ref().to_string(),
            instances: Vec::new(),
        })
        .collect()
}

/// One `PlatformArtifactRef` per distinct Suite-sourced `component_driver`
/// package, `instances` listing every component instance that shares it
/// (`left_drive`/`right_drive` both resolving
/// `phoxal/component-ddsm115`). A Path/Git-sourced driver is a source
/// participant instead (see `source_participants_from_resolved`) and is not
/// included here. Reused by every command that validates the graph like a
/// service (`check`, `run`); `simulate` also fetches through this
/// same function but discards a driver participant from its final launch set
/// after validating its contracts (drivers are sim-substituted, never
/// launched).
pub(crate) fn component_driver_platform_refs_from_resolved(
    resolved: &ResolvedRobot,
) -> Vec<PlatformArtifactRef> {
    struct SuiteDriverRef {
        name: String,
        artifact_ref: String,
        instances: Vec<String>,
    }

    let mut by_package = std::collections::BTreeMap::<String, SuiteDriverRef>::new();
    for component in &resolved.components {
        let Some(driver) = &component.driver else {
            continue;
        };
        if !matches!(driver.source, ResolvedComponentSource::Suite) {
            continue;
        }
        let Some(runtime) = &driver.suite_runtime else {
            continue;
        };
        by_package
            .entry(driver.package.clone())
            .or_insert_with(|| SuiteDriverRef {
                name: runtime.name.clone(),
                artifact_ref: runtime.artifact_ref().to_string(),
                instances: Vec::new(),
            })
            .instances
            .push(component.instance.clone());
    }
    by_package
        .into_values()
        .map(|driver_ref| PlatformArtifactRef {
            name: driver_ref.name,
            kind: ArtifactKind::ComponentDriver,
            artifact_ref: driver_ref.artifact_ref,
            instances: driver_ref.instances,
        })
        .collect()
}

/// Every distinct Suite-sourced component driver's `suite_runtime`, keyed
/// by its `artifact_ref` - the same shape as the `official_by_ref` map every
/// caller already builds from `resolved.platform_runtimes` for the shared
/// `extract_emit_apis_from_staged_runtime` closure. Callers merge this in so
/// one fetch closure resolves services, simulators, AND suite component
/// drivers identically.
pub(crate) fn component_driver_runtimes_by_ref(
    resolved: &ResolvedRobot,
) -> std::collections::BTreeMap<String, &ResolvedPlatformRuntime> {
    resolved
        .components
        .iter()
        .filter_map(|component| component.driver.as_ref())
        .filter(|driver| matches!(driver.source, ResolvedComponentSource::Suite))
        .filter_map(|driver| driver.suite_runtime.as_ref())
        .map(|runtime| (runtime.artifact_ref().to_string(), runtime))
        .collect()
}

pub(crate) fn check_artifact_refs_from_resolved(
    resolved: &ResolvedRobot,
) -> Vec<PlatformArtifactRef> {
    let mut refs = platform_artifact_refs_from_resolved(resolved);
    refs.extend(component_driver_platform_refs_from_resolved(resolved));
    refs.extend(
        resolved
            .tools
            .iter()
            .filter(|tool| tool.kind == ArtifactKind::Tool)
            .filter(|tool| tool.path_override.is_none())
            .filter(|tool| tool_env_override(tool).is_none())
            .map(|tool| PlatformArtifactRef {
                name: tool.name.clone(),
                kind: tool.kind,
                artifact_ref: tool.asset.clone(),
                instances: Vec::new(),
            }),
    );
    refs
}

pub(crate) fn source_participants_from_resolved(
    project_root: &Path,
    resolved: &ResolvedRobot,
    mut locate_component_crate: impl FnMut(&ResolvedComponent, &Path) -> Result<PathBuf>,
) -> Result<Vec<SourceParticipant>> {
    let mut participants = resolved
        .platform_runtimes
        .iter()
        .filter_map(|runtime| {
            runtime.source_path().map(|path| {
                SourceParticipant::official_service(
                    runtime.name.clone(),
                    runtime.name.clone(),
                    path.to_path_buf(),
                )
            })
        })
        .collect::<Vec<_>>();

    participants.extend(resolved.user_runtimes.iter().map(|runtime| {
        SourceParticipant::user_service(
            runtime.name.clone(),
            resolve_project_path(project_root, &runtime.path),
        )
    }));

    // A Suite-sourced driver is a first-class suite artifact, not a
    // build-from-source participant - it becomes a `PlatformArtifactRef`
    // instead (see `component_driver_platform_refs_from_resolved`), fetched
    // and validated like a service. Only a Path/Git (fork/dev-override)
    // driver builds from source here.
    for component in resolved.components.iter().filter(|component| {
        component
            .driver
            .as_ref()
            .is_some_and(|driver| !matches!(driver.source, ResolvedComponentSource::Suite))
    }) {
        let crate_dir = if let Some(path) = component.driver_path_override() {
            path.to_path_buf()
        } else {
            locate_component_crate(component, project_root).with_context(|| {
                format!(
                    "failed to locate component driver {} source",
                    component.instance
                )
            })?
        };
        participants.push(SourceParticipant::component_driver_with_artifact_id(
            component.instance.clone(),
            component.source_name.clone(),
            crate_dir,
        ));
    }

    for tool in resolved
        .tools
        .iter()
        .filter(|tool| tool.kind == ArtifactKind::Tool)
        .filter_map(|tool| {
            tool.path_override.as_ref().map(|path| {
                SourceParticipant::tool(
                    tool.name.clone(),
                    tool_emit_apis_id(&tool.name).to_string(),
                    path.clone(),
                )
            })
        })
    {
        participants.push(tool);
    }

    for simulator in resolved
        .path_overrides
        .iter()
        .filter(|override_| {
            override_.kind
                == phoxal_cli_core::project::resolver::ResolvedPathOverrideKind::Simulator
        })
        .map(|override_| {
            SourceParticipant::simulator(
                override_.artifact_name.clone(),
                override_.artifact_name.clone(),
                override_.path.clone(),
            )
        })
    {
        participants.push(simulator);
    }

    Ok(participants)
}

pub(crate) fn ensure_suite_availability(resolved: &ResolvedRobot) -> Result<()> {
    let unavailable = resolved
        .platform_runtimes
        .iter()
        .filter(|runtime| runtime.source_path().is_none())
        .filter(|runtime| !runtime.published)
        .collect::<Vec<_>>();
    if unavailable.is_empty() {
        return Ok(());
    }

    let mut message = format!(
        "NotYetAvailable: {} is not deployable on {}",
        resolved.robot.robot.id, resolved.target
    );
    message.push_str("\n\nframework train: ");
    message.push_str(&resolved.train);
    message.push_str("\n\nRequired artifacts not released:");
    for runtime in unavailable {
        message.push_str("\n  - ");
        message.push_str(&runtime.package);
        message.push_str(" is missing for ");
        message.push_str(&resolved.target);
        if !runtime.published_triples.is_empty() {
            message.push_str("; published triples: ");
            message.push_str(&runtime.published_triples.join(", "));
        }
    }
    message.push_str(
        "\n\nFix: wait for the listed official artifacts to publish, add a matching Cargo workspace override, or move the train with `cargo update -p phoxal`.",
    );
    bail!("{message}")
}
