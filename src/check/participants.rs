//! Participants responsibilities for check.

use super::tool_env_override;
use anyhow::Context;
use anyhow::Result;
use phoxal_cli_core::check::source::SourceParticipant;
use phoxal_cli_core::check::source::ToolParticipant;
use phoxal_cli_core::project::catalog::ArtifactKind;
use phoxal_cli_core::project::resolver::ResolvedComponent;
use phoxal_cli_core::project::resolver::ResolvedComponentSource;
use phoxal_cli_core::project::resolver::ResolvedPlatformRuntime;
use phoxal_cli_core::project::resolver::ResolvedRobot;
use phoxal_cli_core::project::resolver::official_binary_name;
use phoxal_cli_core::project::resolver::tool_participant_id;
use phoxal_cli_core::project::tooling::resolve_project_path;
use std::path::Path;
use std::path::PathBuf;

/// One resolved official artifact `run_check_with_context` needs a
/// participant report for: its canonical `bin/` file name (the key its
/// materialized binary is looked up under, post-`cargo install`) plus the
/// caller-known identity (`name`/`kind`) that the fetched report's own
/// declared id is checked against before its schema is trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformArtifactRef {
    pub name: String,
    pub kind: ArtifactKind,
    pub binary_name: String,
    /// The component instance ids launching this artifact, for a
    /// `ComponentDriver` ref only. Empty for every other kind (a normal
    /// robot runtime participant). A registry-resolved component
    /// driver is fetched once but launched once per instance that declares
    /// it (`left_drive`/`right_drive` sharing one `phoxal/component-<id>`
    /// package) - mirrors how
    /// `SourceParticipant::component_driver_with_artifact_id` keys a
    /// workspace-built driver's graph membership by instance, not by artifact
    /// id. Must not be empty when `kind == ComponentDriver`.
    pub instances: Vec<String>,
}

impl PlatformArtifactRef {
    pub(super) fn kind_label(&self) -> &'static str {
        match self.kind {
            ArtifactKind::Service => "official service",
            ArtifactKind::ComponentAssets => "official component assets",
            ArtifactKind::ComponentDriver => "official driver",
            ArtifactKind::Tool => "official tool",
            ArtifactKind::Simulator => "official simulator",
            ArtifactKind::Infrastructure => "official infrastructure",
        }
    }
}

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
            binary_name: official_binary_name(runtime.kind, &runtime.name),
            instances: Vec::new(),
        })
        .collect()
}

/// One `PlatformArtifactRef` per distinct registry-sourced `component_driver`
/// package, `instances` listing every component instance that shares it
/// (`left_drive`/`right_drive` both resolving
/// `phoxal/component-ddsm115`). A Path/Git-sourced driver is a source
/// participant instead (see `source_participants_from_resolved`) and is not
/// included here. Reused by every path that validates the graph like a
/// service (`build`, `run`); `simulate` also fetches through this
/// same function but discards a driver participant from its final launch set
/// after validating its contracts (drivers are sim-substituted, never
/// launched).
pub(crate) fn component_driver_platform_refs_from_resolved(
    resolved: &ResolvedRobot,
) -> Vec<PlatformArtifactRef> {
    struct RegistryDriverRef {
        name: String,
        binary_name: String,
        instances: Vec<String>,
    }

    let mut by_package = std::collections::BTreeMap::<String, RegistryDriverRef>::new();
    for component in &resolved.components {
        let Some(driver) = &component.driver else {
            continue;
        };
        if !matches!(driver.source, ResolvedComponentSource::Registry) {
            continue;
        }
        let Some(runtime) = &driver.registry_runtime else {
            continue;
        };
        by_package
            .entry(driver.package.clone())
            .or_insert_with(|| RegistryDriverRef {
                name: runtime.name.clone(),
                binary_name: official_binary_name(runtime.kind, &runtime.name),
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
            binary_name: driver_ref.binary_name,
            instances: driver_ref.instances,
        })
        .collect()
}

/// Every distinct registry-sourced component driver's `registry_runtime`,
/// keyed by its canonical `bin/` file name - the same shape as the
/// `official_by_ref` map every caller already builds from
/// `resolved.platform_runtimes` for the shared
/// `extract_participant_report_from_staged_runtime` closure. Callers merge
/// this in so one fetch closure resolves services, simulators, AND registry
/// component drivers identically.
pub(crate) fn component_driver_runtimes_by_ref(
    resolved: &ResolvedRobot,
) -> std::collections::BTreeMap<String, &ResolvedPlatformRuntime> {
    resolved
        .components
        .iter()
        .filter_map(|component| component.driver.as_ref())
        .filter(|driver| matches!(driver.source, ResolvedComponentSource::Registry))
        .filter_map(|driver| driver.registry_runtime.as_ref())
        .map(|runtime| (official_binary_name(runtime.kind, &runtime.name), runtime))
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
                binary_name: tool.binary_name.clone(),
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

    participants.extend(resolved.user_tools.iter().map(|runtime| {
        SourceParticipant::user_tool(
            runtime.name.clone(),
            resolve_project_path(project_root, &runtime.path),
        )
    }));

    // A registry-sourced driver is a first-class official artifact, not a
    // build-from-source participant - it becomes a `PlatformArtifactRef`
    // instead (see `component_driver_platform_refs_from_resolved`),
    // materialized via `cargo install` and validated like a service. Only a
    // Path/Git (fork/dev-override) driver builds from source here.
    for component in resolved.components.iter().filter(|component| {
        component
            .driver
            .as_ref()
            .is_some_and(|driver| !matches!(driver.source, ResolvedComponentSource::Registry))
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
                    tool_participant_id(&tool.name).to_string(),
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
