//! Participants responsibilities for check.

use crate::check::source::SourceParticipant;
use crate::source::resolver::BundlePlan;
use crate::source::resolver::ResolvedComponentDriver;
use crate::source::resolver::ResolvedPlatformRuntime;
use crate::source::tooling::resolve_project_path;
use anyhow::Result;
use phoxal_cli_catalog::ArtifactKind;
use std::path::Path;

/// One resolved official artifact `run_check_with_context` needs a
/// participant report for: its staged `bin/` file name - the id it is launched
/// under, which is what staging renamed the installed package to - plus the
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
            binary_name: phoxal_cli_catalog::bundle_binary_name(&runtime.name),
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
                binary_name: phoxal_cli_catalog::bundle_binary_name(&runtime.name),
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
        .map(|runtime| {
            (
                phoxal_cli_catalog::bundle_binary_name(&runtime.name),
                runtime,
            )
        })
        .collect()
}

pub(crate) fn check_artifact_refs_from_resolved(resolved: &BundlePlan) -> Vec<PlatformArtifactRef> {
    let mut refs = platform_artifact_refs_from_resolved(resolved);
    refs.extend(component_driver_platform_refs_from_resolved(resolved));
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
    // The mandatory root brain enters every selected source build first: it is
    // built, inspected, and staged on the same path as every other source
    // participant, with its Cargo package/bin target kept separate from its
    // canonical `brain` identity.
    let mut participants = vec![SourceParticipant::brain(
        resolved.brain.crate_dir.clone(),
        resolved.brain.bin_target.clone(),
    )];
    participants.extend(resolved.platform_runtimes.iter().filter_map(|runtime| {
        runtime.source_path().map(|path| {
            SourceParticipant::official_service(
                runtime.name.clone(),
                runtime.name.clone(),
                path.to_path_buf(),
            )
        })
    }));

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

    Ok(participants)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check as graph_check;
    use crate::check::source::SourceParticipantKind;
    use crate::source::resolver::ResolvedComponent;
    use crate::validation::{
        CheckGraphContext, RawArtifact, RawParticipantReport, run_check_with_context,
    };
    use anyhow::{Result, bail};
    use std::path::PathBuf;

    fn fixture_local_driver(path: &str) -> ResolvedComponentDriver {
        ResolvedComponentDriver::Local {
            crate_dir: PathBuf::from(path),
        }
    }

    /// A registry-sourced component package with a populated `registry_runtime`,
    /// the shape `resolve_components` produces for a package with no matching
    /// `components/` workspace crate.
    fn fixture_registry_driver(package: &str, component_name: &str) -> ResolvedComponentDriver {
        ResolvedComponentDriver::Registry(ResolvedPlatformRuntime {
            name: component_name.to_string(),
            package: package.to_string(),
            kind: ArtifactKind::ComponentDriver,
            path_override: None,
            train: "0.36.0".to_string(),
            target: Some("aarch64-unknown-linux-gnu".to_string()),
        })
    }

    /// The report a real `#[phoxal::brain]` binary emits: fixed identity, brain
    /// kind, and the unit config schema its `Config = ()` produces.
    fn raw_brain_unit() -> RawParticipantReport {
        raw_brain(Some(serde_json::json!({"type": "null"})))
    }

    /// A brain report carrying `schema` as its embedded config schema, so the
    /// check engine can be driven against a root binary that is not a valid brain.
    fn raw_brain(schema: Option<serde_json::Value>) -> RawParticipantReport {
        RawParticipantReport {
            artifact: RawArtifact {
                kind: "brain".to_string(),
                id: "brain".to_string(),
            },
            config_schema: schema,
        }
    }

    /// The mandatory root brain source record `resolved_with_components`'
    /// fixture root package produces the unit configuration.
    fn fixture_brain_source() -> SourceParticipant {
        SourceParticipant::brain(std::path::PathBuf::from("/tmp/robot"), "testbot-robot")
    }

    fn raw_kind(kind: &str, id: &str) -> RawParticipantReport {
        RawParticipantReport {
            artifact: RawArtifact {
                kind: kind.to_string(),
                id: id.to_string(),
            },
            config_schema: None,
        }
    }

    fn resolved_with_components(components: Vec<ResolvedComponent>) -> Result<BundlePlan> {
        Ok(BundlePlan {
            source_manifest: crate::source::resolver::parse_robot_from_string(MINIMAL_ROBOT)?,
            compiled: crate::stage::compile_test_bundle(
                &crate::source::resolver::parse_robot_from_string(MINIMAL_ROBOT)?,
            )?,
            train: crate::stage::test_project_train(),
            target: crate::resolve::project::host_target_triple(),
            brain: crate::source::resolver::ResolvedBrain {
                crate_dir: std::path::PathBuf::from("/tmp/robot"),
                package: "testbot-robot".to_string(),
                bin_target: "testbot-robot".to_string(),
            },
            platform_runtimes: Vec::new(),
            user_runtimes: Vec::new(),
            undeclared_runtimes: Vec::new(),
            components,
            path_overrides: Vec::new(),
        })
    }

    const MINIMAL_ROBOT: &str = r#"schema: phoxal/robot/v0
robot:
  id: testbot
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
  components: {}
"#;

    #[test]
    fn components_without_drivers_are_not_built() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let resolved = resolved_with_components(vec![
            ResolvedComponent {
                instance: "left_drive".to_string(),
                source_name: "ddsm115".to_string(),
                assets_root: PathBuf::from("components/ddsm115"),
                driver: Some(fixture_local_driver("components/ddsm115")),
            },
            ResolvedComponent {
                instance: "caster".to_string(),
                source_name: "passive_caster".to_string(),
                assets_root: PathBuf::from("components/passive_caster"),
                driver: None,
            },
        ])?;
        // Resolution retains the local driver's settled source directory; source
        // participant construction never rediscoveries a component package.
        let source_participants = source_participants_from_resolved(temp.path(), &resolved)?;

        assert_eq!(
            source_participants,
            vec![
                fixture_brain_source(),
                SourceParticipant::component_driver_with_artifact_id(
                    "left_drive".to_string(),
                    "ddsm115".to_string(),
                    PathBuf::from("components/ddsm115")
                )
            ]
        );

        let mut built = Vec::new();
        let outcome = run_check_with_context(
            &[],
            &source_participants,
            CheckGraphContext { robot: None },
            |_| bail!("no platform images should be fetched"),
            |participant| {
                let dir = participant.crate_dir.as_path();
                built.push(dir.to_path_buf());
                match participant.kind {
                    SourceParticipantKind::Brain => Ok(raw_brain_unit()),
                    _ => Ok(raw_kind("driver", "ddsm115")),
                }
            },
        )?;

        assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
        assert_eq!(
            built,
            vec![
                PathBuf::from("/tmp/robot"),
                PathBuf::from("components/ddsm115")
            ]
        );

        // Simulation uses the same resolved plan but deliberately substitutes
        // physical components. The selector must not merely filter an already
        // built list.
        // The mandatory brain still runs in simulation; only the physical
        // component drivers are substituted out.
        let simulation_sources =
            source_participants_from_resolved_with_drivers(temp.path(), &resolved, false)?;
        assert_eq!(
            simulation_sources,
            vec![fixture_brain_source()],
            "{simulation_sources:?}"
        );
        Ok(())
    }

    #[test]
    fn registry_component_driver_becomes_a_platform_ref_not_a_source_participant() -> Result<()> {
        // A registry-sourced driver is a first-class official artifact - it
        // must NOT enter `source_participants_from_resolved` (which would
        // later bail trying to build/locate a nonexistent local crate dir).
        let temp = tempfile::tempdir()?;
        let resolved = resolved_with_components(vec![ResolvedComponent {
            instance: "left_drive".to_string(),
            source_name: "ddsm115".to_string(),
            assets_root: PathBuf::from("registry/ddsm115"),
            driver: Some(fixture_registry_driver(
                "phoxal/component-ddsm115",
                "ddsm115",
            )),
        }])?;

        let source_participants = source_participants_from_resolved(temp.path(), &resolved)?;
        assert_eq!(
            source_participants,
            vec![fixture_brain_source()],
            "registry driver must not become a source participant: {source_participants:?}"
        );

        let platform_refs = component_driver_platform_refs_from_resolved(&resolved);
        assert_eq!(platform_refs.len(), 1);
        assert_eq!(platform_refs[0].kind, ArtifactKind::ComponentDriver);
        assert_eq!(platform_refs[0].name, "ddsm115");
        assert_eq!(platform_refs[0].instances, vec!["left_drive".to_string()]);

        Ok(())
    }

    #[test]
    fn n_instances_of_one_registry_driver_fetch_once_and_validate_as_n_graph_participants()
    -> Result<()> {
        // Two instances (`left_drive`/`right_drive`) share one registry-sourced
        // driver package: the fetch closure must be called exactly once
        // (proving the driver is fetched once, not per instance), yet both
        // instances must appear as distinct, correctly-scoped graph
        // participants - exactly like two Path/Git-overridden driver
        // instances already do.
        let temp = tempfile::tempdir()?;
        let resolved = resolved_with_components(vec![
            ResolvedComponent {
                instance: "left_drive".to_string(),
                source_name: "ddsm115".to_string(),
                assets_root: PathBuf::from("registry/ddsm115"),
                driver: Some(fixture_registry_driver(
                    "phoxal/component-ddsm115",
                    "ddsm115",
                )),
            },
            ResolvedComponent {
                instance: "right_drive".to_string(),
                source_name: "ddsm115".to_string(),
                assets_root: PathBuf::from("registry/ddsm115"),
                driver: Some(fixture_registry_driver(
                    "phoxal/component-ddsm115",
                    "ddsm115",
                )),
            },
        ])?;

        let platform_refs = component_driver_platform_refs_from_resolved(&resolved);
        assert_eq!(
            platform_refs.len(),
            1,
            "one shared package must yield one platform ref, not one per instance"
        );
        let mut instances = platform_refs[0].instances.clone();
        instances.sort();
        assert_eq!(
            instances,
            vec!["left_drive".to_string(), "right_drive".to_string()]
        );

        // One bundle serves every mode, so the driver is always resolved and
        // always checked: whether it is started is the launcher's decision.
        let refs = check_artifact_refs_from_resolved(&resolved);
        let checked = refs
            .iter()
            .find(|reference| reference.kind == ArtifactKind::ComponentDriver)
            .expect("a declared registry driver is always checkable");
        assert_eq!(checked.instances, ["left_drive", "right_drive"]);

        // Only the mandatory brain, exactly once: the registry driver stays a
        // platform ref.
        let source_participants = source_participants_from_resolved(temp.path(), &resolved)?;
        assert_eq!(source_participants, vec![fixture_brain_source()]);

        let mut fetch_calls = 0;
        let outcome = run_check_with_context(
            &platform_refs,
            &source_participants,
            CheckGraphContext { robot: None },
            |artifact_ref| {
                fetch_calls += 1;
                assert_eq!(artifact_ref, "ddsm115");
                Ok(raw_kind("driver", "ddsm115"))
            },
            |participant| {
                assert_eq!(participant.kind, SourceParticipantKind::Brain);
                Ok(raw_brain_unit())
            },
        )?;

        assert_eq!(
            fetch_calls, 1,
            "the shared driver must be fetched exactly once"
        );
        assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
        let mut participant_ids = outcome
            .checked_participants
            .iter()
            .map(|participant| participant.participant_id.clone())
            .collect::<Vec<_>>();
        participant_ids.sort();
        assert_eq!(
            participant_ids,
            vec![
                "brain".to_string(),
                "left_drive".to_string(),
                "right_drive".to_string()
            ],
            "each instance must be a distinct graph node keyed by its own instance id"
        );
        for participant in outcome
            .checked_participants
            .iter()
            .filter(|participant| participant.participant_id != "brain")
        {
            assert_eq!(participant.artifact_id, "ddsm115");
            assert!(matches!(
                &participant.scope,
                graph_check::ParticipantScope::ComponentInstance(instance)
                    if *instance == participant.participant_id
            ));
        }

        Ok(())
    }

    #[test]
    fn driverless_registry_component_stages_assets_only_and_is_not_a_check_participant()
    -> Result<()> {
        // Component assets contribute no contracts and are never a check
        // participant, registry-sourced or not; a driverless instance yields
        // no source participant and no platform ref.
        let temp = tempfile::tempdir()?;
        let resolved = resolved_with_components(vec![ResolvedComponent {
            instance: "caster".to_string(),
            source_name: "passive_caster".to_string(),
            assets_root: PathBuf::from("registry/passive_caster"),
            driver: None,
        }])?;

        let source_participants = source_participants_from_resolved(temp.path(), &resolved)?;
        assert_eq!(source_participants, vec![fixture_brain_source()]);
        assert!(component_driver_platform_refs_from_resolved(&resolved).is_empty());

        Ok(())
    }

    #[test]
    fn path_overridden_service_enters_check_through_source_participant_report() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut resolved = resolved_with_components(Vec::new())?;
        resolved.platform_runtimes.push(ResolvedPlatformRuntime {
            name: "drive".to_string(),
            package: "phoxal/service-drive".to_string(),
            kind: ArtifactKind::Service,
            path_override: Some(temp.path().join("framework/service/drive")),
            train: "0.36.0".to_string(),
            target: Some("aarch64-unknown-linux-gnu".to_string()),
        });

        let platform_refs = platform_artifact_refs_from_resolved(&resolved);
        assert!(platform_refs.is_empty());

        let source_participants = source_participants_from_resolved(temp.path(), &resolved)?;
        assert_eq!(
            source_participants,
            vec![
                fixture_brain_source(),
                SourceParticipant::official_service(
                    "drive",
                    "drive",
                    temp.path().join("framework/service/drive"),
                )
            ]
        );
        let outcome = run_check_with_context(
            &platform_refs,
            &source_participants,
            CheckGraphContext { robot: None },
            |_| bail!("path-overridden service should not read registry metadata"),
            |participant| match participant.kind {
                SourceParticipantKind::Brain => Ok(raw_brain_unit()),
                SourceParticipantKind::OfficialService => Ok(raw_kind("service", "drive")),
                other => bail!("unexpected source participant kind {other:?}"),
            },
        )?;
        assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
        Ok(())
    }
}
