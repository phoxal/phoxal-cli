//! Participants responsibilities for check.

use anyhow::Result;
use phoxal_cli_core::check::source::SourceParticipant;
use phoxal_cli_core::project::catalog::ArtifactKind;
use phoxal_cli_core::project::resolver::BundlePlan;
use phoxal_cli_core::project::resolver::ResolvedComponentDriver;
use phoxal_cli_core::project::resolver::ResolvedPlatformRuntime;
use phoxal_cli_core::project::resolver::official_binary_name;
use phoxal_cli_core::project::tooling::resolve_project_path;
use std::path::Path;

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
            ArtifactKind::ComponentDriver => "official driver",
            ArtifactKind::Simulator => "official simulator",
        }
    }
}

pub(crate) fn platform_artifact_refs_from_resolved(
    resolved: &BundlePlan,
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
/// included here. Reused by native paths that validate the graph like a
/// service (`build`, `run`). Simulation filters these refs before preparation
/// because physical drivers are substituted out there and must not be fetched
/// merely to discard them afterward.
pub(crate) fn component_driver_platform_refs_from_resolved(
    resolved: &BundlePlan,
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
        let Some(runtime) = driver.registry_runtime() else {
            continue;
        };
        by_package
            .entry(runtime.package.clone())
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
    resolved: &BundlePlan,
) -> std::collections::BTreeMap<String, &ResolvedPlatformRuntime> {
    resolved
        .components
        .iter()
        .filter_map(|component| component.driver.as_ref())
        .filter_map(ResolvedComponentDriver::registry_runtime)
        .map(|runtime| (official_binary_name(runtime.kind, &runtime.name), runtime))
        .collect()
}

pub(crate) fn check_artifact_refs_from_resolved(
    resolved: &BundlePlan,
    drivers: phoxal_cli_core::project::layout::DriverSelection,
) -> Vec<PlatformArtifactRef> {
    let mut refs = platform_artifact_refs_from_resolved(resolved);
    refs.extend(
        component_driver_platform_refs_from_resolved(resolved)
            .into_iter()
            .filter_map(|mut reference| {
                reference
                    .instances
                    .retain(|instance| drivers.includes_instance(instance));
                (!reference.instances.is_empty()).then_some(reference)
            }),
    );
    refs
}

pub(crate) fn source_participants_from_resolved(
    project_root: &Path,
    resolved: &BundlePlan,
) -> Result<Vec<SourceParticipant>> {
    source_participants_from_resolved_with_drivers(project_root, resolved, true)
}

pub(crate) fn source_participants_from_resolved_with_drivers(
    project_root: &Path,
    resolved: &BundlePlan,
    include_component_drivers: bool,
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

    // A registry-sourced driver is a first-class official artifact, not a
    // build-from-source participant - it becomes a `PlatformArtifactRef`
    // instead (see `component_driver_platform_refs_from_resolved`),
    // materialized via `cargo install` and validated like a service. Only a
    // Path/Git (fork/dev-override) driver builds from source here.
    for (component, crate_dir) in resolved.components.iter().filter_map(|component| {
        include_component_drivers
            .then_some(component)
            .and_then(|component| match component.driver.as_ref() {
                Some(ResolvedComponentDriver::Local { crate_dir }) => Some((component, crate_dir)),
                Some(ResolvedComponentDriver::Registry(_)) | None => None,
            })
    }) {
        participants.push(SourceParticipant::component_driver_with_artifact_id(
            component.instance.clone(),
            component.source_name.clone(),
            crate_dir.clone(),
        ));
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
