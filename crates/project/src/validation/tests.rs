//! Tests for this module.

use super::participants::{
    component_driver_platform_refs_from_resolved, platform_artifact_refs_from_resolved,
};
use super::*;
use crate::check::source::{SourceParticipant, SourceParticipantKind};
use crate::source::resolver::{
    BundlePlan, ResolvedComponent, ResolvedComponentDriver, ResolvedPlatformRuntime,
};
use anyhow::{Result, anyhow, bail};
use graph_check::Problem;
use phoxal_cli_catalog::ArtifactKind;
use phoxal_manifest::source::robot::v0::Manifest as Robot;
use std::path::{Path, PathBuf};

/// Converts a fixture's `(name, artifact_ref)` pairs into the
/// [`PlatformArtifactRef`]s `run_check_with_context` expects, all of kind
/// [`ArtifactKind::Service`] - every caller in this file only ever exercises
/// service-kind platform artifacts.
fn platform_refs(images: &[(String, String)]) -> Vec<PlatformArtifactRef> {
    images
        .iter()
        .map(|(name, binary_name)| PlatformArtifactRef {
            name: name.clone(),
            kind: ArtifactKind::Service,
            binary_name: binary_name.clone(),
            instances: Vec::new(),
        })
        .collect()
}

const LAUNCH_PLAN_FIXTURE_ROBOT: &str = r#"schema: phoxal/robot/v0
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
  components:
    left_drive:
      component: ddsm115
      mount_link: left_wheel
      driver:
        connection: { type: can, bus: 0, node_id: 1 }
    right_drive:
      component: ddsm115
      mount_link: right_wheel
      driver:
        connection: { type: can, bus: 0, node_id: 2 }
services:
  mission: {}
"#;

fn robot_with_service_config(service_id: &str, config: Value) -> Result<Robot> {
    let mut robot = crate::source::resolver::parse_robot_from_string(
        &LAUNCH_PLAN_FIXTURE_ROBOT.replace("mission", service_id),
    )?;
    robot
        .services
        .get_mut(service_id)
        .expect("fixture service")
        .config = Some(config);
    Ok(robot)
}

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

#[test]
fn healthy_graph_passes_with_fake_participant_report() -> Result<()> {
    let images = vec![("mission".to_string(), "mission:ok".to_string())];
    let sources = vec![SourceParticipant::user_service(
        "drive".to_string(),
        PathBuf::from("/fake/project/runtimes/drive"),
    )];

    let outcome = run_check_with_context(
        &platform_refs(&images),
        &sources,
        CheckGraphContext { robot: None },
        |image_ref| match image_ref {
            "mission:ok" => Ok(raw("mission")),
            unexpected => bail!("unexpected image {unexpected}"),
        },
        |participant| {
            let dir = participant.crate_dir.as_path();
            if dir == Path::new("/fake/project/runtimes/drive") {
                Ok(raw("drive"))
            } else {
                bail!("unexpected source dir {}", dir.display())
            }
        },
    )?;

    assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
    Ok(())
}

#[test]
fn healthy_graph_passes_with_platform_and_component_driver_source() -> Result<()> {
    let images = vec![("mission".to_string(), "mission:ok".to_string())];
    let sources = vec![SourceParticipant::component_driver_with_artifact_id(
        "left_drive".to_string(),
        "ddsm115".to_string(),
        PathBuf::from("/fake/project/components/ddsm115"),
    )];

    let outcome = run_check_with_context(
        &platform_refs(&images),
        &sources,
        CheckGraphContext { robot: None },
        |image_ref| match image_ref {
            "mission:ok" => Ok(raw("mission")),
            unexpected => bail!("unexpected image {unexpected}"),
        },
        |participant| {
            let dir = participant.crate_dir.as_path();
            if dir == Path::new("/fake/project/components/ddsm115") {
                Ok(raw_kind("driver", "ddsm115"))
            } else {
                bail!("unexpected source dir {}", dir.display())
            }
        },
    )?;

    assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
    Ok(())
}

#[test]
fn source_and_platform_participants_coexist_in_a_healthy_graph() -> Result<()> {
    // A platform participant and a source participant both check clean.
    let images = vec![("mission".to_string(), "mission:ok".to_string())];
    let sources = vec![SourceParticipant::user_service(
        "drive".to_string(),
        PathBuf::from("/fake/project/runtimes/drive"),
    )];

    let outcome = run_check_with_context(
        &platform_refs(&images),
        &sources,
        CheckGraphContext { robot: None },
        |image_ref| match image_ref {
            "mission:ok" => Ok(raw("mission")),
            unexpected => bail!("unexpected image {unexpected}"),
        },
        |_| Ok(raw("drive")),
    )?;

    assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
    Ok(())
}

#[test]
fn user_service_artifact_id_must_match_manifest_key() {
    let sources = vec![SourceParticipant::user_service(
        "avoid".to_string(),
        PathBuf::from("/fake/project/runtimes/avoid"),
    )];

    let error = run_check_with_context(
        &[],
        &sources,
        CheckGraphContext { robot: None },
        |_| bail!("no platform images should be fetched"),
        |_| Ok(raw("surprise")),
    )
    .expect_err("mismatched user service artifact id should abort check");

    let message = error.to_string();
    assert!(
        message.contains("artifact.id 'surprise'")
            && message.contains("expected artifact id 'avoid'"),
        "{message}"
    );
}

#[test]
fn official_service_artifact_identity_must_match_resolved_name() {
    let images = vec![("drive".to_string(), "drive:swapped".to_string())];

    let error = run_check_with_context(
        &platform_refs(&images),
        &[],
        CheckGraphContext { robot: None },
        |image_ref| match image_ref {
            "drive:swapped" => Ok(raw("mission")),
            unexpected => bail!("unexpected image {unexpected}"),
        },
        |_| bail!("no source services should be built"),
    )
    .expect_err("swapped official service artifact should abort check");

    let message = error.to_string();
    assert!(
        message.contains("official service participant report artifact.id 'mission'")
            && message.contains("expected artifact id 'drive'"),
        "{message}"
    );
}

#[test]
fn official_driver_artifact_identity_uses_driver_label() {
    let artifacts = vec![PlatformArtifactRef {
        name: "bno085".to_string(),
        kind: ArtifactKind::ComponentDriver,
        binary_name: "driver-bno085:swapped".to_string(),
        instances: vec!["imu".to_string()],
    }];

    let error = run_check_with_context(
        &artifacts,
        &[],
        CheckGraphContext { robot: None },
        |artifact_ref| match artifact_ref {
            "driver-bno085:swapped" => Ok(raw_kind("service", "bno085")),
            unexpected => bail!("unexpected artifact {unexpected}"),
        },
        |_| bail!("no source services should be built"),
    )
    .expect_err("wrong official driver kind should abort check");

    let message = error.to_string();
    assert!(
        message.contains("official driver participant report artifact.kind 'service'")
            && message.contains("expected kind 'driver'"),
        "{message}"
    );
}

#[test]
fn component_driver_artifact_kind_true_kind_is_accepted() -> Result<()> {
    let sources = vec![SourceParticipant::component_driver_with_artifact_id(
        "left_motor",
        "ddsm115",
        PathBuf::from("/fake/project/components/ddsm115"),
    )];

    let outcome = run_check_with_context(
        &[],
        &sources,
        CheckGraphContext { robot: None },
        |_| bail!("no platform images should be fetched"),
        |_| Ok(raw_kind("driver", "ddsm115")),
    )?;

    assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
    Ok(())
}

#[test]
fn component_driver_artifact_kind_legacy_runtime_is_rejected() {
    let sources = vec![SourceParticipant::component_driver_with_artifact_id(
        "left_motor",
        "ddsm115",
        PathBuf::from("/fake/project/components/ddsm115"),
    )];

    let error = run_check_with_context(
        &[],
        &sources,
        CheckGraphContext { robot: None },
        |_| bail!("no platform images should be fetched"),
        |_| Ok(raw_kind("runtime", "ddsm115")),
    )
    .expect_err("component driver reporting legacy runtime kind should abort check");

    let message = error.to_string();
    assert!(
            message.contains(
                "component driver participant report artifact.kind 'runtime' does not match the expected kind 'driver'"
            ),
            "{message}"
        );
}

#[test]
fn every_source_participant_always_builds_no_scoping_no_cache() -> Result<()> {
    // Every source participant is rebuilt live on every `run_check_with_context`
    // invocation. This proves
    // `run_check` invokes the build closure for every source participant,
    // not just a named one.
    let sources = vec![
        SourceParticipant::user_service(
            "bad".to_string(),
            PathBuf::from("/fake/project/runtimes/bad"),
        ),
        SourceParticipant::user_service(
            "other".to_string(),
            PathBuf::from("/fake/project/runtimes/other"),
        ),
        SourceParticipant::component_driver_with_artifact_id(
            "left_drive".to_string(),
            "ddsm115".to_string(),
            PathBuf::from("/fake/project/components/ddsm115"),
        ),
    ];

    let mut built = Vec::new();
    let outcome = run_check_with_context(
        &[],
        &sources,
        CheckGraphContext { robot: None },
        |_| bail!("no platform images should be fetched"),
        |participant| {
            let dir = participant.crate_dir.as_path();
            built.push(dir.to_path_buf());
            if dir == Path::new("/fake/project/runtimes/bad") {
                Ok(raw("bad"))
            } else if dir == Path::new("/fake/project/runtimes/other") {
                Ok(raw("other"))
            } else if dir == Path::new("/fake/project/components/ddsm115") {
                Ok(raw_kind("driver", "ddsm115"))
            } else {
                bail!("unexpected source participant: {}", dir.display())
            }
        },
    )?;

    assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
    assert_eq!(
        built,
        vec![
            PathBuf::from("/fake/project/runtimes/bad"),
            PathBuf::from("/fake/project/runtimes/other"),
            PathBuf::from("/fake/project/components/ddsm115"),
        ],
        "every source participant must build, every invocation - no scoping, no cache"
    );
    Ok(())
}

#[test]
fn component_driver_with_no_producer_is_a_legal_graph() -> Result<()> {
    // A component driver subscribing to a contract with no producer in the
    // graph is legal under the relaxed graph check.
    let sources = vec![
        SourceParticipant::user_service(
            "other".to_string(),
            PathBuf::from("/fake/project/runtimes/other"),
        ),
        SourceParticipant::component_driver_with_artifact_id(
            "left_drive".to_string(),
            "ddsm115".to_string(),
            PathBuf::from("/fake/project/components/ddsm115"),
        ),
    ];

    let outcome = run_check_with_context(
        &[],
        &sources,
        CheckGraphContext { robot: None },
        |_| bail!("no platform images should be fetched"),
        |participant| {
            let dir = participant.crate_dir.as_path();
            if dir == Path::new("/fake/project/runtimes/other") {
                Ok(raw("other"))
            } else if dir == Path::new("/fake/project/components/ddsm115") {
                Ok(raw_kind("driver", "ddsm115"))
            } else {
                bail!("unexpected source dir {}", dir.display())
            }
        },
    )?;

    assert!(outcome.report.problems.is_empty());
    Ok(())
}

#[test]
fn user_service_with_no_producer_is_a_legal_graph() -> Result<()> {
    let sources = vec![
        SourceParticipant::user_service(
            "bad".to_string(),
            PathBuf::from("/fake/project/runtimes/bad"),
        ),
        SourceParticipant::user_service(
            "other".to_string(),
            PathBuf::from("/fake/project/runtimes/other"),
        ),
    ];

    let outcome = run_check_with_context(
        &[],
        &sources,
        CheckGraphContext { robot: None },
        |_| bail!("no platform images should be fetched"),
        |participant| {
            let dir = participant.crate_dir.as_path();
            if dir == Path::new("/fake/project/runtimes/bad") {
                Ok(raw("bad"))
            } else if dir == Path::new("/fake/project/runtimes/other") {
                Ok(raw("other"))
            } else {
                bail!("unexpected source dir {}", dir.display())
            }
        },
    )?;

    assert!(outcome.report.problems.is_empty());
    Ok(())
}

#[test]
fn component_driver_and_platform_participants_coexist_in_a_healthy_graph() -> Result<()> {
    // A component driver and a platform publisher both check clean. The
    // driver still appears under its concrete instance id (`left_drive`),
    // not the shared driver artifact (`ddsm115`), so multiple instances
    // of one driver stay distinct.
    let images = vec![("mission".to_string(), "mission:ok".to_string())];
    let sources = vec![SourceParticipant::component_driver_with_artifact_id(
        "left_drive".to_string(),
        "ddsm115".to_string(),
        PathBuf::from("/fake/project/components/ddsm115"),
    )];

    let outcome = run_check_with_context(
        &platform_refs(&images),
        &sources,
        CheckGraphContext { robot: None },
        |image_ref| match image_ref {
            "mission:ok" => Ok(raw("mission")),
            unexpected => bail!("unexpected image {unexpected}"),
        },
        |_| Ok(raw_kind("driver", "ddsm115")),
    )?;

    assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
    let participant_ids = outcome
        .checked_participants
        .iter()
        .map(|participant| participant.participant_id.as_str())
        .collect::<Vec<_>>();
    assert!(participant_ids.contains(&"left_drive"));
    Ok(())
}

#[test]
fn source_build_error_is_a_hard_error() {
    let sources = vec![SourceParticipant::user_service(
        "drive".to_string(),
        PathBuf::from("/fake/project/runtimes/drive"),
    )];

    let error = run_check_with_context(
        &[],
        &sources,
        CheckGraphContext { robot: None },
        |_| bail!("no platform images should be fetched"),
        |_| Err(anyhow!("source build failed")),
    )
    .expect_err("source build failures should abort check");

    let message = format!("{error:#}");
    assert!(
        message.contains("failed to obtain participant report for user service drive"),
        "{message}"
    );
    assert!(message.contains("source build failed"), "{message}");
}

#[test]
fn component_driver_build_error_is_a_hard_error() {
    let sources = vec![SourceParticipant::component_driver_with_artifact_id(
        "left_drive".to_string(),
        "ddsm115".to_string(),
        PathBuf::from("/fake/project/components/ddsm115"),
    )];

    let error = run_check_with_context(
        &[],
        &sources,
        CheckGraphContext { robot: None },
        |_| bail!("no platform images should be fetched"),
        |_| Err(anyhow!("component build failed")),
    )
    .expect_err("component driver build failures should abort check");

    let message = format!("{error:#}");
    assert!(
        message.contains("failed to obtain participant report for component driver left_drive"),
        "{message}"
    );
    assert!(message.contains("component build failed"), "{message}");
}

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
fn n_instances_of_one_registry_driver_fetch_once_and_validate_as_n_graph_participants() -> Result<()>
{
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

    let off =
        check_artifact_refs_from_resolved(&resolved, crate::source::intent::DriverSelection::None);
    assert!(
        off.iter()
            .all(|reference| reference.kind != ArtifactKind::ComponentDriver),
        "--drivers off must not fetch a registry driver during checking"
    );
    let subset = check_artifact_refs_from_resolved(
        &resolved,
        crate::source::intent::DriverSelection::Only(
            ["left_drive".to_string()].into_iter().collect(),
        ),
    );
    let selected = subset
        .iter()
        .find(|reference| reference.kind == ArtifactKind::ComponentDriver)
        .expect("the selected registry driver remains checkable");
    assert_eq!(selected.instances, ["left_drive"]);

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
            assert_eq!(artifact_ref, "phoxal-component-ddsm115");
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
fn driverless_registry_component_stages_assets_only_and_is_not_a_check_participant() -> Result<()> {
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

#[test]
fn user_service_config_is_validated_against_emitted_schema() -> Result<()> {
    let sources = vec![SourceParticipant::user_service(
        "avoid".to_string(),
        PathBuf::from("/fake/project/runtimes/avoid"),
    )];
    let emitted = RawParticipantReport {
        artifact: RawArtifact {
            kind: "service".to_string(),
            id: "avoid".to_string(),
        },
        config_schema: Some(serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "Config",
            "type": "object",
            "properties": { "gain": { "type": "number", "format": "double" } },
            "required": ["gain"]
        })),
    };
    assert_eq!(
        emitted.config_schema,
        Some(serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "Config",
            "type": "object",
            "properties": { "gain": { "type": "number", "format": "double" } },
            "required": ["gain"]
        }))
    );

    let check_config = |config: Value| -> Result<CheckOutcome> {
        let robot = robot_with_service_config("avoid", config)?;

        run_check_with_context(
            &[],
            &sources,
            CheckGraphContext {
                robot: Some(&robot),
            },
            |_| bail!("no platform images should be fetched"),
            |_| Ok(emitted.clone()),
        )
    };

    let missing = check_config(serde_json::json!({}))?;
    assert!(matches!(
        missing
            .report
            .problems
            .iter()
            .find(|problem| matches!(problem, Problem::InvalidConfig { .. })),
        Some(Problem::InvalidConfig { runtime_id, errors })
            if runtime_id == "avoid"
                && errors.iter().any(|error| error.contains("gain"))
    ));

    let mistyped = check_config(serde_json::json!({ "gain": "fast" }))?;
    assert!(matches!(
        mistyped
            .report
            .problems
            .iter()
            .find(|problem| matches!(problem, Problem::InvalidConfig { .. })),
        Some(Problem::InvalidConfig { runtime_id, errors })
            if runtime_id == "avoid"
                && errors.iter().any(|error| error.contains("gain"))
    ));

    let valid = check_config(serde_json::json!({ "gain": 1.5 }))?;
    assert!(
        valid
            .report
            .problems
            .iter()
            .all(|problem| !matches!(problem, Problem::InvalidConfig { .. })),
        "{:?}",
        valid.report.problems
    );
    Ok(())
}

#[test]
fn absent_user_service_config_validates_as_null() -> Result<()> {
    let sources = vec![SourceParticipant::user_service(
        "optional".to_string(),
        PathBuf::from("/fake/project/runtimes/optional"),
    )];

    let outcome = run_check_with_context(
        &[],
        &sources,
        CheckGraphContext { robot: None },
        |_| bail!("no platform images should be fetched"),
        |_| {
            let mut raw = raw("optional");
            raw.config_schema = Some(serde_json::json!({ "type": "null" }));
            Ok(raw)
        },
    )?;

    assert!(
        outcome
            .report
            .problems
            .iter()
            .all(|problem| !matches!(problem, Problem::InvalidConfig { .. })),
        "{:?}",
        outcome.report.problems
    );
    Ok(())
}

#[test]
fn absent_user_service_config_still_fails_required_object_schema() -> Result<()> {
    let sources = vec![SourceParticipant::user_service(
        "required".to_string(),
        PathBuf::from("/fake/project/runtimes/required"),
    )];

    let outcome = run_check_with_context(
        &[],
        &sources,
        CheckGraphContext { robot: None },
        |_| bail!("no platform images should be fetched"),
        |_| {
            let mut raw = raw("required");
            raw.config_schema = Some(serde_json::json!({
                "type": "object",
                "required": ["gain"],
                "properties": {
                    "gain": { "type": "number" }
                },
                "additionalProperties": false
            }));
            Ok(raw)
        },
    )?;

    assert!(matches!(
        outcome
            .report
            .problems
            .iter()
            .find(|problem| matches!(problem, Problem::InvalidConfig { .. })),
        Some(Problem::InvalidConfig { runtime_id, errors })
            if runtime_id == "required"
                && errors.iter().any(|error| error.contains("null"))
    ));
    Ok(())
}

#[test]
fn user_service_config_uses_full_json_schema_keywords() -> Result<()> {
    let sources = vec![SourceParticipant::user_service(
        "avoid".to_string(),
        PathBuf::from("/fake/project/runtimes/avoid"),
    )];
    let robot = robot_with_service_config(
        "avoid",
        serde_json::json!({
            "gains": [0.25, 5.5],
            "mode": "FAST",
            "extra": true
        }),
    )?;

    let outcome = run_check_with_context(
        &[],
        &sources,
        CheckGraphContext {
            robot: Some(&robot),
        },
        |_| bail!("no platform images should be fetched"),
        |_| {
            let mut raw = raw("avoid");
            raw.config_schema = Some(serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "required": ["gains", "mode"],
                "properties": {
                    "gains": {
                        "type": "array",
                        "minItems": 2,
                        "maxItems": 2,
                        "items": { "$ref": "#/$defs/gain" }
                    },
                    "mode": {
                        "type": "string",
                        "pattern": "^[a-z]+$"
                    }
                },
                "additionalProperties": false,
                "$defs": {
                    "gain": {
                        "type": "number",
                        "minimum": 0.0,
                        "maximum": 1.0
                    }
                }
            }));
            Ok(raw)
        },
    )?;

    let [Problem::InvalidConfig { runtime_id, errors }] = outcome.report.problems.as_slice() else {
        panic!(
            "expected one InvalidConfig problem, got {:?}",
            outcome.report.problems
        );
    };
    assert_eq!(runtime_id, "avoid");
    assert!(
        errors.iter().any(|error| error.contains("/gains/1")),
        "{errors:?}"
    );
    assert!(
        errors.iter().any(|error| error.contains("/mode")),
        "{errors:?}"
    );
    assert!(
        errors
            .iter()
            .any(|error| error.to_ascii_lowercase().contains("additional properties")),
        "{errors:?}"
    );
    Ok(())
}

fn raw(id: &str) -> RawParticipantReport {
    raw_kind("service", id)
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

/// The check-engine half of the unit-config gate ().
///
/// `phoxal validate` and the Webots simulation path never open a staged
/// layout, so runtime-bundle verification's identical rule cannot protect
/// them. A root binary whose id and kind are a perfectly good `brain` but
/// which embeds a real config surface must still fail here, before any
/// supervisor startup, and the ordinary unit schema must still pass.
#[test]
fn a_brain_declaring_a_config_surface_fails_the_check_engine() -> Result<()> {
    let sources = vec![fixture_brain_source()];
    let check = |schema: Option<serde_json::Value>| {
        run_check_with_context(
            &[],
            &sources,
            CheckGraphContext { robot: None },
            |_| bail!("no platform artifact reports are fetched here"),
            |_| Ok(raw_brain(schema.clone())),
        )
    };

    let error = check(Some(serde_json::json!({
        "type": "object",
        "properties": {"speed": {"type": "integer"}},
    })))
    .expect_err("a brain embedding a real config surface must be rejected")
    .to_string();
    assert!(error.contains("brain"), "{error}");
    assert!(error.contains("takes no config at all"), "{error}");

    // A missing schema is equally not the unit schema.
    let error = check(None)
        .expect_err("a brain with no embedded config schema must be rejected")
        .to_string();
    assert!(error.contains("takes no config at all"), "{error}");

    // The real `#[phoxal::brain]` record passes and enters the graph.
    let outcome = check(Some(serde_json::json!({"type": "null"})))?;
    assert!(outcome.is_ok(), "{outcome:?}");
    assert_eq!(
        outcome
            .checked_participants
            .iter()
            .map(|participant| participant.participant_id.as_str())
            .collect::<Vec<_>>(),
        ["brain"]
    );
    Ok(())
}

/// The mandatory root brain source record `resolved_with_components`'
/// fixture root package produces ().
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
        train: "0.36.0".to_string(),
        target: crate::resolve::project::host_target_triple(),
        brain: crate::source::resolver::ResolvedBrain {
            crate_dir: std::path::PathBuf::from("/tmp/robot"),
            package: "testbot-robot".to_string(),
            bin_target: "testbot-robot".to_string(),
        },
        platform_runtimes: Vec::new(),
        simulators: Vec::new(),
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
