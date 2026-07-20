//! Tests for this module.

use super::build::build_emit_apis_from_source_with_diagnostics;
use super::participants::{
    component_driver_platform_refs_from_resolved, platform_artifact_refs_from_resolved,
};
use super::*;
use anyhow::{Result, anyhow, bail};
use graph_check::{ParticipantClass, Problem};
use phoxal::model::robot::v0::Robot;
use phoxal_cli_core::check::source::SourceParticipantKind;
use phoxal_cli_core::project::catalog::{
    ArtifactKind, OFFICIAL_SERVICES, SelectionChannel as CatalogChannel, fixture_catalog_for_tests,
    fixture_component_assets_entry_for_tests, fixture_component_driver_entry_for_tests,
    fixture_contract_for_tests, fixture_service_entry_for_tests,
};
use phoxal_cli_core::project::launch_plan::{
    CheckedRobotLaunchInput, LaunchMode, ROBOT_TOOL_BUS, ROBOT_TOOL_LOG, SITE_TOOL_JOYPAD,
    SITE_TOOL_TELEMETRY, SubstitutionRecord, build_launch_plan,
};
use phoxal_cli_core::project::resolver::{
    ResolveOptions, ResolvedComponent, ResolvedComponentPackage, ResolvedComponentSource,
    ResolvedPlatformRuntime, ResolvedRobot, ResolvedTool, UserRuntimeManifestExtras,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::host_paths::test_support::ScratchPhoxalHome;
use crate::resolver::resolve;

fn coherence_surface(
    participant_id: &str,
    contracts: &[(&str, &str, &str)],
) -> graph_check::ParticipantContractSurface {
    graph_check::ParticipantContractSurface {
        participant_id: participant_id.to_string(),
        contracts: contracts
            .iter()
            .map(
                |(role, version, contract)| participant_metadata::ParticipantMetaContract {
                    role: (*role).to_string(),
                    version: (*version).to_string(),
                    contract: (*contract).to_string(),
                    external: false,
                },
            )
            .collect(),
    }
}

fn assert_severity_matrix(diagnostics: &[RobotCoherenceDiagnostic], coherent: bool) {
    let check = if coherent {
        CoherenceDisposition::Pass
    } else {
        CoherenceDisposition::Warning
    };
    let hard = if coherent {
        CoherenceDisposition::Pass
    } else {
        CoherenceDisposition::Failure
    };
    assert_eq!(
        coherence_disposition(CoherenceVerb::Check, false, diagnostics),
        check
    );
    assert_eq!(
        coherence_disposition(CoherenceVerb::Check, true, diagnostics),
        hard
    );
    for verb in [
        CoherenceVerb::Deploy,
        CoherenceVerb::Run,
        CoherenceVerb::Simulate,
    ] {
        assert_eq!(coherence_disposition(verb, false, diagnostics), hard);
    }
}

#[test]
fn launch_plan_covers_site_singletons_services_and_component_instances() -> Result<()> {
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let temp = tempfile::tempdir()?;
    std::fs::create_dir_all(temp.path().join("runtimes/mission"))?;
    std::fs::write(
        temp.path().join("runtimes/mission/Cargo.toml"),
        "[package]\nname = \"mission\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    std::fs::write(temp.path().join("runtimes/mission/src.txt"), "source")?;
    let robot = Robot::parse_from_string(LAUNCH_PLAN_FIXTURE_ROBOT)?;
    let catalog = fixture_catalog_for_tests(vec![
        fixture_service_entry_for_tests(
            "drive",
            "0.1.0",
            CatalogChannel::Stable,
            &crate::resolver::host_target_triple(),
            true,
            vec![fixture_contract_for_tests("v1::drive::Target", "publish")],
        ),
        fixture_component_assets_entry_for_tests("ddsm115", "0.1.0", CatalogChannel::Stable),
        fixture_component_driver_entry_for_tests(
            "ddsm115",
            "0.1.0",
            CatalogChannel::Stable,
            &crate::resolver::host_target_triple(),
            true,
            Vec::new(),
        ),
    ]);
    let mut resolved = resolve(
        &robot,
        temp.path(),
        Some(&catalog),
        ResolveOptions {
            resolve_source_commits: false,
            resolve_component_asset_commits: false,
            ..ResolveOptions::default()
        },
    )?;
    add_launch_plan_site_tools(&mut resolved);
    let mut extras = RobotManifestExtras::default();
    extras.user_runtimes.insert(
        "mission".to_string(),
        UserRuntimeManifestExtras {
            config: Some(serde_json::json!({
                "message": "line\nquoted \"value\"",
            })),
        },
    );
    let source_participants = vec![
        SourceParticipant::user_service("mission", temp.path().join("runtimes/mission")),
        SourceParticipant::component_driver_with_artifact_id(
            "left_drive",
            "ddsm115",
            temp.path().join("components/ddsm115"),
        ),
        SourceParticipant::component_driver_with_artifact_id(
            "right_drive",
            "ddsm115",
            temp.path().join("components/ddsm115"),
        ),
    ];
    let platform_refs = platform_artifact_refs_from_resolved(&resolved);
    let outcome = run_check_with_context(
        &platform_refs,
        &[],
        &source_participants,
        CheckGraphContext {
            manifest_extras: &extras,
        },
        |artifact_ref| {
            let participant = platform_refs
                .iter()
                .find(|participant| participant.artifact_ref == artifact_ref)
                .ok_or_else(|| anyhow!("unexpected platform artifact {artifact_ref}"))?;
            Ok(launch_plan_raw_emit_apis(
                participant.kind.emit_apis_kind(),
                &participant.name,
            ))
        },
        |_| bail!("no tools in this check fixture"),
        |source| match source.kind {
            SourceParticipantKind::UserService => {
                Ok(launch_plan_raw_emit_apis("service", &source.name))
            }
            SourceParticipantKind::ComponentDriver => Ok(launch_plan_raw_emit_apis(
                "driver",
                &source.expected_artifact_id,
            )),
            SourceParticipantKind::OfficialService => Ok(launch_plan_raw_emit_apis(
                "service",
                &source.expected_artifact_id,
            )),
            SourceParticipantKind::Tool => Ok(launch_plan_raw_emit_apis(
                "tool",
                &source.expected_artifact_id,
            )),
            SourceParticipantKind::Simulator => Ok(launch_plan_raw_emit_apis(
                "simulator",
                &source.expected_artifact_id,
            )),
        },
    )?;
    assert!(outcome.is_ok(), "fixture check should pass: {outcome:?}");
    let plan = build_launch_plan(
        LaunchMode::Run,
        &[CheckedRobotLaunchInput {
            project_root: temp.path(),
            resolved: &resolved,
            manifest_extras: &extras,
            checked_participants: &outcome.checked_participants,
            substitutions: &[],
            source_participants: &source_participants,
        }],
    )?;

    assert_eq!(plan.mode, LaunchMode::Run);
    assert_eq!(plan.site[0].id, SITE_TOOL_JOYPAD);
    assert_eq!(plan.site[0].phoxal_config, Value::Null);
    assert_eq!(plan.site[1].id, SITE_TOOL_TELEMETRY);
    let robot = &plan.robots[0];
    assert_eq!(robot.id, "robot_v1");
    assert_eq!(robot.substitutions, Vec::<SubstitutionRecord>::new());
    let participant_ids = robot
        .participants
        .iter()
        .map(|participant| participant.launch.participant_id.as_str())
        .collect::<Vec<_>>();
    for (service, _) in OFFICIAL_SERVICES {
        assert!(
            participant_ids.contains(service),
            "missing platform service {service}: {participant_ids:?}"
        );
    }
    assert!(participant_ids.contains(&"left_drive"));
    assert!(participant_ids.contains(&"right_drive"));
    assert!(participant_ids.contains(&"tool-bus-robot_v1"));
    assert!(participant_ids.contains(&"tool-log-robot_v1"));
    assert_eq!(
        participant_ids
            .iter()
            .filter(|id| **id == "mission")
            .count(),
        1,
        "only the explicitly authored user mission remains"
    );
    let left_drive = robot
        .participants
        .iter()
        .find(|participant| participant.launch.participant_id == "left_drive")
        .expect("left_drive participant");
    assert_eq!(left_drive.artifact_id, "ddsm115");
    assert_eq!(
        left_drive.launch.component_instance.as_deref(),
        Some("left_drive")
    );
    let mission = robot
        .participants
        .iter()
        .find(|participant| participant.launch.participant_id == "mission")
        .expect("mission participant");
    assert_eq!(
        mission.launch.config,
        Some(serde_json::json!({"message": "line\nquoted \"value\""}))
    );
    let encoded = phoxal_cli_core::session::launch_env::encode_participant_env(&mission.launch)?;
    assert_eq!(
        encoded
            .variables()
            .get(phoxal::participant::launch::env::CONFIG)
            .map(String::as_str),
        Some(r#"{"message":"line\nquoted \"value\""}"#)
    );
    Ok(())
}

fn launch_plan_raw_emit_apis(kind: &str, id: &str) -> RawEmitApis {
    RawEmitApis {
        artifact: RawArtifact {
            kind: kind.to_string(),
            id: id.to_string(),
        },
        participant_class: "checked".to_string(),
        api_version: "v1".to_string(),
        required_contracts: Vec::new(),
        config_schema: None,
    }
}

fn add_launch_plan_site_tools(resolved: &mut ResolvedRobot) {
    resolved.tools.push(launch_plan_tool(ROBOT_TOOL_BUS));
    resolved.tools.push(launch_plan_tool(SITE_TOOL_JOYPAD));
    resolved.tools.push(launch_plan_tool(ROBOT_TOOL_LOG));
    resolved.tools.push(launch_plan_tool(SITE_TOOL_TELEMETRY));
}

fn launch_plan_tool(name: &str) -> ResolvedTool {
    ResolvedTool {
        kind: ArtifactKind::Tool,
        name: name.to_string(),
        package: format!("phoxal/{name}"),
        requested: "0.1.0".to_string(),
        resolved: "0.1.0".to_string(),
        repo: "phoxal/framework".to_string(),
        asset: format!(
            "{name}-0.1.0-{}.tar.gz",
            crate::resolver::host_target_triple()
        ),
        binary_name: name.to_string(),
        sha256: "0".repeat(64),
        url: None,
        size: None,
        published: false,
        path_override: None,
        channel: CatalogChannel::Stable,
        target: crate::resolver::host_target_triple(),
    }
}

const LAUNCH_PLAN_FIXTURE_ROBOT: &str = r#"schema: robot/v0
robot:
  id: robot_v1
  namespace: dev
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
  mission:
    path: runtimes/mission
"#;

#[test]
fn coherent_contract_set_passes_every_verb() {
    let surfaces = vec![
        coherence_surface("producer", &[("publish", "v1", "drive::Target")]),
        coherence_surface("consumer", &[("subscribe", "v1", "drive::Target")]),
        coherence_surface("server", &[("serve", "v1", "map::Get")]),
        coherence_surface("client", &[("ask", "v1", "map::Get")]),
    ];
    let diagnostics = vec![evaluate_robot_coherence("robot-a", &surfaces)];
    assert_severity_matrix(&diagnostics, true);
}

#[test]
fn pub_sub_disjoint_warns_for_check_and_fails_strict_and_launch_verbs() {
    let surfaces = vec![
        coherence_surface("producer", &[("publish", "v1", "drive::Target")]),
        coherence_surface("consumer", &[("subscribe", "v2", "drive::Target")]),
    ];
    let diagnostics = vec![evaluate_robot_coherence("robot-a", &surfaces)];
    assert!(matches!(
        diagnostics[0].mismatches.as_slice(),
        [CoherenceMismatchDiagnostic::PubSubDisjoint { .. }]
    ));
    assert_severity_matrix(&diagnostics, false);
}

#[test]
fn unserved_ask_warns_for_check_and_fails_strict_and_launch_verbs() {
    let surfaces = vec![
        coherence_surface("server", &[("serve", "v1", "map::Get")]),
        coherence_surface("client", &[("ask", "v2", "map::Get")]),
    ];
    let diagnostics = vec![evaluate_robot_coherence("robot-a", &surfaces)];
    assert!(matches!(
        diagnostics[0].mismatches.as_slice(),
        [CoherenceMismatchDiagnostic::UnservedAsk { .. }]
    ));
    assert_severity_matrix(&diagnostics, false);
}

#[test]
fn robot_graphs_are_checked_independently_not_pooled() {
    let robot_a = vec![
        coherence_surface("a-producer", &[("publish", "v1", "drive::Target")]),
        coherence_surface("a-consumer", &[("subscribe", "v2", "drive::Target")]),
    ];
    let robot_b = vec![coherence_surface(
        "b-producer",
        &[("publish", "v2", "drive::Target")],
    )];

    let diagnostics = [
        evaluate_robot_coherence("robot-a", &robot_a),
        evaluate_robot_coherence("robot-b", &robot_b),
    ];
    assert_eq!(diagnostics[0].mismatches.len(), 1);
    assert!(diagnostics[1].mismatches.is_empty());

    let pooled = robot_a
        .into_iter()
        .chain(robot_b)
        .collect::<Vec<graph_check::ParticipantContractSurface>>();
    assert!(
        evaluate_robot_coherence("incorrect-pool", &pooled)
            .mismatches
            .is_empty()
    );
}

fn fixture_component_package(
    package: &str,
    kind: phoxal_cli_core::project::catalog::ArtifactKind,
    path: &str,
) -> ResolvedComponentPackage {
    ResolvedComponentPackage {
        package: package.to_string(),
        kind,
        source: ResolvedComponentSource::Path {
            path: PathBuf::from(path),
        },
        path_override: None,
        catalog_runtime: None,
    }
}

/// A Catalog-sourced component package with a populated `catalog_runtime`,
/// the shape `resolve_component_package` produces once a matching release
/// asset exists.
fn fixture_catalog_component_package(
    package: &str,
    kind: phoxal_cli_core::project::catalog::ArtifactKind,
    component_name: &str,
) -> ResolvedComponentPackage {
    ResolvedComponentPackage {
        package: package.to_string(),
        kind,
        source: ResolvedComponentSource::Catalog,
        path_override: None,
        catalog_runtime: Some(ResolvedPlatformRuntime {
            name: component_name.to_string(),
            package: package.to_string(),
            kind,
            version: "0.1.0".to_string(),
            artifact_ref: format!("{}-driver-v0.1.0.tar.zst", component_name),
            sha256: Some("a".repeat(64)),
            url: Some("https://example.invalid/component.tar.zst".to_string()),
            size: Some(1),
            published: true,
            published_triples: Vec::new(),
            path_override: None,
            channel: phoxal_cli_core::project::catalog::SelectionChannel::Stable,
            target: Some("aarch64-unknown-linux-gnu".to_string()),
        }),
    }
}

#[test]
fn healthy_graph_passes_with_fake_emit_apis() -> Result<()> {
    let images = vec![("mission".to_string(), "mission:ok".to_string())];
    let sources = vec![SourceParticipant::user_service(
        "drive".to_string(),
        PathBuf::from("/fake/project/runtimes/drive"),
    )];

    let outcome = run_check(
        &images,
        &[],
        &sources,
        |image_ref| match image_ref {
            "mission:ok" => Ok(raw("mission", "v1", &["drive::Target"])),
            unexpected => bail!("unexpected image {unexpected}"),
        },
        |_| bail!("no tools should be fetched"),
        |participant| {
            let dir = participant.crate_dir.as_path();
            if dir == Path::new("/fake/project/runtimes/drive") {
                Ok(raw("drive", "v1", &["drive::Target"]))
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

    let outcome = run_check(
        &images,
        &[],
        &sources,
        |image_ref| match image_ref {
            "mission:ok" => Ok(raw("mission", "v1", &["drive::Target"])),
            unexpected => bail!("unexpected image {unexpected}"),
        },
        |_| bail!("no tools should be fetched"),
        |participant| {
            let dir = participant.crate_dir.as_path();
            if dir == Path::new("/fake/project/components/ddsm115") {
                Ok(raw_kind("driver", "ddsm115", "v1", &["drive::Target"]))
            } else {
                bail!("unexpected source dir {}", dir.display())
            }
        },
    )?;

    assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
    Ok(())
}

#[test]
fn privileged_tools_share_a_contract_family_with_no_agreement_gate() -> Result<()> {
    // D1: a privileged tool and a checked source participant reporting
    // different roles for the same `family` is not a mismatch - there is
    // no `schema_id` axis left to disagree on; name identity alone
    // decides compatibility.
    let tools = vec![ToolParticipant {
        name: "joypad".to_string(),
        binary_path: PathBuf::from("/fake/cache/joypad"),
    }];
    let sources = vec![SourceParticipant::user_service(
        "drive".to_string(),
        PathBuf::from("/fake/project/runtimes/drive"),
    )];

    let outcome = run_check(
        &[],
        &tools,
        &sources,
        |_| bail!("no platform images should be fetched"),
        |tool| {
            let path = tool.binary_path.as_path();
            if path == Path::new("/fake/cache/joypad") {
                Ok(raw_kind_with_role(
                    "tool",
                    "joypad",
                    "v1",
                    &[("drive::Target", "subscribe")],
                    "privileged",
                ))
            } else {
                bail!("unexpected tool path {}", path.display())
            }
        },
        |participant| {
            let dir = participant.crate_dir.as_path();
            if dir == Path::new("/fake/project/runtimes/drive") {
                Ok(raw_with_role(
                    "drive",
                    "v1",
                    &[("drive::Target", "publish")],
                ))
            } else {
                bail!("unexpected source dir {}", dir.display())
            }
        },
    )?;

    assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
    Ok(())
}

#[test]
fn privileged_tools_are_exempt_from_topology() -> Result<()> {
    let tools = vec![ToolParticipant {
        name: "joypad".to_string(),
        binary_path: PathBuf::from("/fake/cache/joypad"),
    }];

    let outcome = run_check(
        &[],
        &tools,
        &[],
        |_| bail!("no platform images should be fetched"),
        |tool| {
            let path = tool.binary_path.as_path();
            if path == Path::new("/fake/cache/joypad") {
                Ok(raw_kind_class(
                    "tool",
                    "joypad",
                    "v1",
                    &["drive::Target", "odometry::State"],
                    "privileged",
                ))
            } else {
                bail!("unexpected tool path {}", path.display())
            }
        },
        |_| bail!("no source services should be built"),
    )?;

    assert!(outcome.report.problems.is_empty());
    Ok(())
}

#[test]
fn deployed_user_service_images_are_checked_from_image_refs() -> Result<()> {
    let user_images = vec![UserServiceImageParticipant {
        name: "avoid".to_string(),
        image_ref: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            .to_string(),
    }];
    let sources = vec![SourceParticipant::component_driver_with_artifact_id(
        "left_drive".to_string(),
        "ddsm115".to_string(),
        PathBuf::from("/fake/project/components/ddsm115"),
    )];
    let extras = RobotManifestExtras::default();

    let mut fetched_images = Vec::new();
    let mut built_sources = Vec::new();
    let outcome = run_check_with_deployed_user_service_images(
        CheckParticipants {
            platform_artifact_refs: &[],
            user_service_images: &user_images,
            tool_participants: &[],
            source_participants: &sources,
        },
        CheckGraphContext {
            manifest_extras: &extras,
        },
        |image_ref| {
            fetched_images.push(image_ref.to_string());
            Ok(raw("avoid", "v1", &[]))
        },
        |_| bail!("no tools should be fetched"),
        |participant| {
            let dir = participant.crate_dir.as_path();
            built_sources.push(dir.to_path_buf());
            Ok(raw_kind("driver", "ddsm115", "v1", &[]))
        },
    )?;

    assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
    assert_eq!(
        fetched_images,
        vec!["sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
    );
    assert_eq!(
        built_sources,
        vec![PathBuf::from("/fake/project/components/ddsm115")]
    );
    Ok(())
}

#[test]
fn source_and_platform_sharing_a_contract_family_is_a_healthy_graph() -> Result<()> {
    // D1: a platform publisher and a source subscriber sharing
    // `drive::Target` is healthy regardless of role - name identity
    // alone decides compatibility, there is no wire-shape agreement axis
    // left to gate on.
    let images = vec![("mission".to_string(), "mission:ok".to_string())];
    let sources = vec![SourceParticipant::user_service(
        "drive".to_string(),
        PathBuf::from("/fake/project/runtimes/drive"),
    )];

    let outcome = run_check(
        &images,
        &[],
        &sources,
        |image_ref| match image_ref {
            "mission:ok" => Ok(raw_with_role(
                "mission",
                "v1",
                &[("drive::Target", "publish")],
            )),
            unexpected => bail!("unexpected image {unexpected}"),
        },
        |_| bail!("no tools should be fetched"),
        |_| {
            Ok(raw_with_role(
                "drive",
                "v1",
                &[("drive::Target", "subscribe")],
            ))
        },
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

    let error = run_check(
        &[],
        &[],
        &sources,
        |_| bail!("no platform images should be fetched"),
        |_| bail!("no tools should be fetched"),
        |_| Ok(raw("surprise", "v1", &[])),
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

    let error = run_check(
        &images,
        &[],
        &[],
        |image_ref| match image_ref {
            "drive:swapped" => Ok(raw("mission", "v1", &[])),
            unexpected => bail!("unexpected image {unexpected}"),
        },
        |_| bail!("no tools should be fetched"),
        |_| bail!("no source services should be built"),
    )
    .expect_err("swapped official service artifact should abort check");

    let message = error.to_string();
    assert!(
        message.contains("official service emit-apis artifact.id 'mission'")
            && message.contains("expected artifact id 'drive'"),
        "{message}"
    );
}

#[test]
fn official_driver_artifact_identity_uses_driver_label() {
    let artifacts = vec![PlatformArtifactRef {
        name: "bno085".to_string(),
        kind: ArtifactKind::ComponentDriver,
        artifact_ref: "driver-bno085:swapped".to_string(),
        instances: vec!["imu".to_string()],
    }];
    let extras = RobotManifestExtras::default();

    let error = run_check_with_context(
        &artifacts,
        &[],
        &[],
        CheckGraphContext {
            manifest_extras: &extras,
        },
        |artifact_ref| match artifact_ref {
            "driver-bno085:swapped" => Ok(raw_kind("service", "bno085", "v1", &[])),
            unexpected => bail!("unexpected artifact {unexpected}"),
        },
        |_| bail!("no tools should be fetched"),
        |_| bail!("no source services should be built"),
    )
    .expect_err("wrong official driver kind should abort check");

    let message = error.to_string();
    assert!(
        message.contains("official driver emit-apis artifact.kind 'service'")
            && message.contains("expected kind 'driver'"),
        "{message}"
    );
}

#[test]
fn tool_artifact_identity_must_match_resolved_tool() {
    let tools = vec![ToolParticipant {
        name: "joypad".to_string(),
        binary_path: PathBuf::from("/fake/cache/joypad"),
    }];

    let error = run_check(
        &[],
        &tools,
        &[],
        |_| bail!("no platform images should be fetched"),
        |tool| {
            let path = tool.binary_path.as_path();
            if path == Path::new("/fake/cache/joypad") {
                Ok(raw_kind_class(
                    "tool",
                    "simulator_webots_controller",
                    "v1",
                    &[],
                    "privileged",
                ))
            } else {
                bail!("unexpected tool path {}", path.display())
            }
        },
        |_| bail!("no source services should be built"),
    )
    .expect_err("swapped tool binary should abort check");

    let message = error.to_string();
    assert!(
        message.contains("tool emit-apis artifact.id 'simulator_webots_controller'")
            && message.contains("expected artifact id 'joypad'"),
        "{message}"
    );
}

#[test]
fn tool_artifact_kind_true_kind_is_accepted() -> Result<()> {
    let tools = vec![ToolParticipant {
        name: "joypad".to_string(),
        binary_path: PathBuf::from("/fake/cache/joypad"),
    }];

    let outcome = run_check(
        &[],
        &tools,
        &[],
        |_| bail!("no platform images should be fetched"),
        |tool| {
            let path = tool.binary_path.as_path();
            if path == Path::new("/fake/cache/joypad") {
                Ok(raw_kind_class("tool", "joypad", "v1", &[], "privileged"))
            } else {
                bail!("unexpected tool path {}", path.display())
            }
        },
        |_| bail!("no source services should be built"),
    )?;

    assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
    Ok(())
}

#[test]
fn tool_artifact_kind_legacy_runtime_is_rejected() {
    let tools = vec![ToolParticipant {
        name: "joypad".to_string(),
        binary_path: PathBuf::from("/fake/cache/joypad"),
    }];

    let error = run_check(
        &[],
        &tools,
        &[],
        |_| bail!("no platform images should be fetched"),
        |tool| {
            let path = tool.binary_path.as_path();
            if path == Path::new("/fake/cache/joypad") {
                Ok(raw_kind_class("runtime", "joypad", "v1", &[], "privileged"))
            } else {
                bail!("unexpected tool path {}", path.display())
            }
        },
        |_| bail!("no source services should be built"),
    )
    .expect_err("tool binary reporting legacy runtime kind should abort check");

    let message = error.to_string();
    assert!(
        message.contains(
            "tool emit-apis artifact.kind 'runtime' does not match the expected kind 'tool'"
        ),
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

    let outcome = run_check(
        &[],
        &[],
        &sources,
        |_| bail!("no platform images should be fetched"),
        |_| bail!("no tools should be fetched"),
        |_| Ok(raw_kind_class("driver", "ddsm115", "v1", &[], "checked")),
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

    let error = run_check(
        &[],
        &[],
        &sources,
        |_| bail!("no platform images should be fetched"),
        |_| bail!("no tools should be fetched"),
        |_| Ok(raw_kind_class("runtime", "ddsm115", "v1", &[], "checked")),
    )
    .expect_err("component driver reporting legacy runtime kind should abort check");

    let message = error.to_string();
    assert!(
            message.contains(
                "component driver emit-apis artifact.kind 'runtime' does not match the expected kind 'driver'"
            ),
            "{message}"
        );
}

#[test]
fn tool_artifact_kind_garbage_is_rejected() {
    let tools = vec![ToolParticipant {
        name: "joypad".to_string(),
        binary_path: PathBuf::from("/fake/cache/joypad"),
    }];

    let error = run_check(
        &[],
        &tools,
        &[],
        |_| bail!("no platform images should be fetched"),
        |tool| {
            let path = tool.binary_path.as_path();
            if path == Path::new("/fake/cache/joypad") {
                Ok(raw_kind_class(
                    "nonsense",
                    "joypad",
                    "v1",
                    &[],
                    "privileged",
                ))
            } else {
                bail!("unexpected tool path {}", path.display())
            }
        },
        |_| bail!("no source services should be built"),
    )
    .expect_err("tool binary reporting a garbage kind should abort check");

    let message = error.to_string();
    assert!(
        message.contains("tool emit-apis artifact.kind 'nonsense'")
            && message.contains("expected kind 'tool'"),
        "{message}"
    );
}

#[test]
fn every_source_participant_always_builds_no_scoping_no_cache() -> Result<()> {
    // The old `check --service <name>` build-scoping ("UseCached" siblings
    // served from a disk cache) is gone: every source participant is
    // rebuilt live on every `check` invocation, scoped or not. This proves
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
    let outcome = run_check(
        &[],
        &[],
        &sources,
        |_| bail!("no platform images should be fetched"),
        |_| bail!("no tools should be fetched"),
        |participant| {
            let dir = participant.crate_dir.as_path();
            built.push(dir.to_path_buf());
            if dir == Path::new("/fake/project/runtimes/bad") {
                Ok(raw("bad", "v1", &[]))
            } else if dir == Path::new("/fake/project/runtimes/other") {
                Ok(raw("other", "v1", &[]))
            } else if dir == Path::new("/fake/project/components/ddsm115") {
                Ok(raw_kind("driver", "ddsm115", "v1", &[]))
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

    let outcome = run_check(
        &[],
        &[],
        &sources,
        |_| bail!("no platform images should be fetched"),
        |_| bail!("no tools should be fetched"),
        |participant| {
            let dir = participant.crate_dir.as_path();
            if dir == Path::new("/fake/project/runtimes/other") {
                Ok(raw("other", "v1", &[]))
            } else if dir == Path::new("/fake/project/components/ddsm115") {
                Ok(raw_kind("driver", "ddsm115", "v1", &["drive::Target"]))
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

    let outcome = run_check(
        &[],
        &[],
        &sources,
        |_| bail!("no platform images should be fetched"),
        |_| bail!("no tools should be fetched"),
        |participant| {
            let dir = participant.crate_dir.as_path();
            if dir == Path::new("/fake/project/runtimes/bad") {
                Ok(raw("bad", "v1", &["drive::Target"]))
            } else if dir == Path::new("/fake/project/runtimes/other") {
                Ok(raw("other", "v1", &[]))
            } else {
                bail!("unexpected source dir {}", dir.display())
            }
        },
    )?;

    assert!(outcome.report.problems.is_empty());
    Ok(())
}

#[test]
fn build_emit_apis_from_source_never_caches_across_calls() -> Result<()> {
    // The old `cache/emit-apis/` disk cache is gone: two back-to-back calls
    // for the SAME crate dir each invoke the (fake) build closure - nothing
    // is remembered between calls.
    let temp = tempfile::tempdir()?;
    let crate_dir = fixture_crate_dir(&temp, "sibling");
    let participant = SourceParticipant::user_service("sibling", crate_dir.clone());

    let mut build_count = 0;
    let first = build_emit_apis_from_source_with_diagnostics(
        &participant,
        |_| {
            build_count += 1;
            Ok(raw("sibling", "v1", &[]))
        },
        None,
    )?;
    let second = build_emit_apis_from_source_with_diagnostics(
        &participant,
        |_| {
            build_count += 1;
            Ok(raw("sibling", "v1", &[]))
        },
        None,
    )?;

    assert_eq!(build_count, 2, "every call must rebuild, nothing is cached");
    assert_eq!(first, second);
    Ok(())
}

#[test]
fn component_driver_and_platform_sharing_a_contract_family_is_a_healthy_graph() -> Result<()> {
    // D1: a component driver (subscribing `drive::Target`) and a platform
    // publisher sharing the family is healthy regardless of role. The
    // driver still appears under its concrete instance id (`left_drive`),
    // not the shared driver artifact (`ddsm115`), so multiple instances
    // of one driver stay distinct.
    let images = vec![("mission".to_string(), "mission:ok".to_string())];
    let sources = vec![SourceParticipant::component_driver_with_artifact_id(
        "left_drive".to_string(),
        "ddsm115".to_string(),
        PathBuf::from("/fake/project/components/ddsm115"),
    )];

    let outcome = run_check(
        &images,
        &[],
        &sources,
        |image_ref| match image_ref {
            "mission:ok" => Ok(raw_with_role(
                "mission",
                "v1",
                &[("drive::Target", "publish")],
            )),
            unexpected => bail!("unexpected image {unexpected}"),
        },
        |_| bail!("no tools should be fetched"),
        |_| {
            Ok(raw_kind_with_role(
                "driver",
                "ddsm115",
                "v1",
                &[("drive::Target", "subscribe")],
                "checked",
            ))
        },
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

    let error = run_check(
        &[],
        &[],
        &sources,
        |_| bail!("no platform images should be fetched"),
        |_| bail!("no tools should be fetched"),
        |_| Err(MissingImageError::new(anyhow!("source build failed")).into()),
    )
    .expect_err("source build failures should abort check");

    let message = format!("{error:#}");
    assert!(
        message.contains("failed to obtain emit-apis for user service drive"),
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

    let error = run_check(
        &[],
        &[],
        &sources,
        |_| bail!("no platform images should be fetched"),
        |_| bail!("no tools should be fetched"),
        |_| Err(anyhow!("component build failed")),
    )
    .expect_err("component driver build failures should abort check");

    let message = format!("{error:#}");
    assert!(
        message.contains("failed to obtain emit-apis for component driver left_drive"),
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
            assets: Some(fixture_component_package(
                "phoxal/component-ddsm115",
                phoxal_cli_core::project::catalog::ArtifactKind::ComponentAssets,
                "components/ddsm115",
            )),
            driver: Some(fixture_component_package(
                "phoxal/component-ddsm115",
                phoxal_cli_core::project::catalog::ArtifactKind::ComponentDriver,
                "components/ddsm115",
            )),
            has_driver: true,
        },
        ResolvedComponent {
            instance: "caster".to_string(),
            source_name: "passive_caster".to_string(),
            assets: Some(fixture_component_package(
                "phoxal/component-passive_caster",
                phoxal_cli_core::project::catalog::ArtifactKind::ComponentAssets,
                "components/passive_caster",
            )),
            driver: None,
            has_driver: false,
        },
    ])?;
    let mut located = Vec::new();
    let source_participants =
        source_participants_from_resolved(temp.path(), &resolved, |component, project_root| {
            located.push(component.instance.clone());
            Ok(project_root
                .join("component-crates")
                .join(&component.instance))
        })?;

    assert_eq!(located, vec!["left_drive"]);
    assert_eq!(
        source_participants,
        vec![SourceParticipant::component_driver_with_artifact_id(
            "left_drive".to_string(),
            "ddsm115".to_string(),
            temp.path().join("component-crates/left_drive")
        )]
    );

    let mut built = Vec::new();
    let outcome = run_check(
        &[],
        &[],
        &source_participants,
        |_| bail!("no platform images should be fetched"),
        |_| bail!("no tools should be fetched"),
        |participant| {
            let dir = participant.crate_dir.as_path();
            built.push(dir.to_path_buf());
            Ok(raw_kind("driver", "ddsm115", "v1", &[]))
        },
    )?;

    assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
    assert_eq!(built, vec![temp.path().join("component-crates/left_drive")]);
    Ok(())
}

#[test]
fn catalog_component_driver_becomes_a_platform_ref_not_a_source_participant() -> Result<()> {
    // A Catalog-sourced driver is a first-class catalog artifact - it
    // must NOT enter `source_participants_from_resolved` (which would
    // later bail trying to build/locate a nonexistent local crate dir).
    let temp = tempfile::tempdir()?;
    let resolved = resolved_with_components(vec![ResolvedComponent {
        instance: "left_drive".to_string(),
        source_name: "ddsm115".to_string(),
        assets: Some(fixture_catalog_component_package(
            "phoxal/component-ddsm115",
            phoxal_cli_core::project::catalog::ArtifactKind::ComponentAssets,
            "ddsm115",
        )),
        driver: Some(fixture_catalog_component_package(
            "phoxal/component-ddsm115",
            phoxal_cli_core::project::catalog::ArtifactKind::ComponentDriver,
            "ddsm115",
        )),
        has_driver: true,
    }])?;

    let source_participants =
        source_participants_from_resolved(temp.path(), &resolved, |component, _project_root| {
            panic!(
                "a Catalog-sourced driver for '{}' must never reach the source-crate locator",
                component.instance
            )
        })?;
    assert!(
        source_participants.is_empty(),
        "catalog driver must not become a source participant: {source_participants:?}"
    );

    let platform_refs = component_driver_platform_refs_from_resolved(&resolved);
    assert_eq!(platform_refs.len(), 1);
    assert_eq!(
        platform_refs[0].kind,
        phoxal_cli_core::project::catalog::ArtifactKind::ComponentDriver
    );
    assert_eq!(platform_refs[0].name, "ddsm115");
    assert_eq!(platform_refs[0].instances, vec!["left_drive".to_string()]);

    Ok(())
}

#[test]
fn n_instances_of_one_catalog_driver_fetch_once_and_validate_as_n_graph_participants() -> Result<()>
{
    // Two instances (`left_drive`/`right_drive`) share one catalog
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
            assets: Some(fixture_catalog_component_package(
                "phoxal/component-ddsm115",
                phoxal_cli_core::project::catalog::ArtifactKind::ComponentAssets,
                "ddsm115",
            )),
            driver: Some(fixture_catalog_component_package(
                "phoxal/component-ddsm115",
                phoxal_cli_core::project::catalog::ArtifactKind::ComponentDriver,
                "ddsm115",
            )),
            has_driver: true,
        },
        ResolvedComponent {
            instance: "right_drive".to_string(),
            source_name: "ddsm115".to_string(),
            assets: Some(fixture_catalog_component_package(
                "phoxal/component-ddsm115",
                phoxal_cli_core::project::catalog::ArtifactKind::ComponentAssets,
                "ddsm115",
            )),
            driver: Some(fixture_catalog_component_package(
                "phoxal/component-ddsm115",
                phoxal_cli_core::project::catalog::ArtifactKind::ComponentDriver,
                "ddsm115",
            )),
            has_driver: true,
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

    let source_participants =
        source_participants_from_resolved(temp.path(), &resolved, |_component, _project_root| {
            panic!("catalog drivers never reach the source locator")
        })?;
    assert!(source_participants.is_empty());

    let mut fetch_calls = 0;
    let outcome = run_check_with_context(
        &platform_refs,
        &[],
        &source_participants,
        CheckGraphContext {
            manifest_extras: &RobotManifestExtras::default(),
        },
        |artifact_ref| {
            fetch_calls += 1;
            assert_eq!(artifact_ref, "ddsm115-driver-v0.1.0.tar.zst");
            Ok(raw_kind("driver", "ddsm115", "v1", &[]))
        },
        |_| bail!("no tools should be fetched"),
        |_| bail!("no source participants should be built"),
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
        vec!["left_drive".to_string(), "right_drive".to_string()],
        "each instance must be a distinct graph node keyed by its own instance id"
    );
    for participant in &outcome.checked_participants {
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
fn driverless_catalog_component_stages_assets_only_and_is_not_a_check_participant() -> Result<()> {
    // Component assets contribute no contracts and are never a check
    // participant, catalog-sourced or not; a driverless instance yields
    // no source participant and no platform ref.
    let temp = tempfile::tempdir()?;
    let resolved = resolved_with_components(vec![ResolvedComponent {
        instance: "caster".to_string(),
        source_name: "passive_caster".to_string(),
        assets: Some(fixture_catalog_component_package(
            "phoxal/component-passive_caster",
            phoxal_cli_core::project::catalog::ArtifactKind::ComponentAssets,
            "passive_caster",
        )),
        driver: None,
        has_driver: false,
    }])?;

    let source_participants =
        source_participants_from_resolved(temp.path(), &resolved, |_component, _project_root| {
            panic!("a driverless component has no driver to locate")
        })?;
    assert!(source_participants.is_empty());
    assert!(component_driver_platform_refs_from_resolved(&resolved).is_empty());

    Ok(())
}

#[test]
fn path_overridden_service_enters_check_through_source_emit_apis() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut resolved = resolved_with_components(Vec::new())?;
    resolved.platform_runtimes.push(ResolvedPlatformRuntime {
        name: "drive".to_string(),
        package: "phoxal/service-drive".to_string(),
        kind: phoxal_cli_core::project::catalog::ArtifactKind::Service,
        version: "0.1.0".to_string(),
        artifact_ref: "path:framework/service/drive".to_string(),
        sha256: None,
        url: None,
        size: None,
        published: true,
        published_triples: Vec::new(),
        path_override: Some(temp.path().join("framework/service/drive")),
        channel: phoxal_cli_core::project::catalog::SelectionChannel::Stable,
        target: Some("aarch64-unknown-linux-gnu".to_string()),
    });

    let platform_refs = platform_artifact_refs_from_resolved(&resolved);
    assert!(platform_refs.is_empty());

    let source_participants =
        source_participants_from_resolved(temp.path(), &resolved, |_component, _root| {
            bail!("no components in this fixture")
        })?;
    assert_eq!(
        source_participants,
        vec![SourceParticipant::official_service(
            "drive",
            "drive",
            temp.path().join("framework/service/drive"),
        )]
    );

    let extras = RobotManifestExtras::default();
    let outcome = run_check_with_context(
        &platform_refs,
        &[],
        &source_participants,
        CheckGraphContext {
            manifest_extras: &extras,
        },
        |_| bail!("path-overridden service should not read catalog metadata"),
        |_| bail!("no tools in this fixture"),
        |participant| {
            assert_eq!(participant.kind, SourceParticipantKind::OfficialService);
            Ok(raw_kind("service", "drive", "v1", &[]))
        },
    )?;
    assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
    Ok(())
}

#[test]
fn missing_image_is_reported_after_other_images_are_checked() -> Result<()> {
    let images = vec![
        ("mission".to_string(), "mission:ok".to_string()),
        ("drive".to_string(), "service-drive:v1-stable".to_string()),
    ];

    let outcome = run_check(
        &images,
        &[],
        &[],
        |image_ref| match image_ref {
            "mission:ok" => Ok(raw("mission", "v1", &[])),
            "service-drive:v1-stable" => Err(MissingImageError::new(anyhow!("not found")).into()),
            unexpected => bail!("unexpected image {unexpected}"),
        },
        |_| bail!("no tools should be fetched"),
        |_| bail!("no source services should be built"),
    )?;

    assert_eq!(
        outcome.missing_images,
        vec!["service-drive:v1-stable".to_string()]
    );
    assert!(!outcome.is_ok());
    Ok(())
}

#[test]
fn raw_emit_apis_accepts_required_contracts_json() -> Result<()> {
    let parsed: RawEmitApis = serde_json::from_str(
        r#"{
                "artifact": { "kind": "service", "id": "drive", "ignored": true },
                "api_version": "v1",
                "required_contracts": [
                    {
                        "role": "publish",
                        "version": "v1",
                        "contract": "drive::Target",
                        "external": false
                    }
                ],
                "config_schema": { "type": "object" }
            }"#,
    )?;
    let participant = graph_check::ParticipantApis::try_from(parsed)?;

    assert_eq!(participant.artifact_id, "drive");
    assert_eq!(participant.participant_class, ParticipantClass::Checked);
    assert_eq!(participant.api_version, "v1");
    assert_eq!(
        participant
            .config_schema
            .as_ref()
            .and_then(|schema| schema.get("type"))
            .and_then(Value::as_str),
        Some("object")
    );
    assert_eq!(participant.contracts[0].family, "v1::drive::Target");
    Ok(())
}

#[test]
fn raw_emit_apis_threads_privileged_participant_class() -> Result<()> {
    let parsed: RawEmitApis = serde_json::from_str(
        r#"{
                "artifact": { "kind": "tool", "id": "joypad" },
                "participant_class": "privileged",
                "api_version": "v1",
                "required_contracts": []
            }"#,
    )?;
    let participant = graph_check::ParticipantApis::try_from(parsed)?;

    assert_eq!(participant.participant_class, ParticipantClass::Privileged);
    Ok(())
}

#[test]
fn raw_emit_apis_unknown_participant_class_defaults_to_checked() -> Result<()> {
    let mut raw = raw("drive", "v1", &[]);
    raw.participant_class = "future".to_string();
    let participant = graph_check::ParticipantApis::try_from(raw)?;

    assert_eq!(participant.participant_class, ParticipantClass::Checked);
    Ok(())
}

#[test]
fn user_service_config_is_validated_against_emitted_schema() -> Result<()> {
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("api-fixture");
    let sources = vec![SourceParticipant::user_service(
        "avoid".to_string(),
        fixture_dir,
    )];
    let emitted = build_emit_apis_by_building(&sources[0])?;
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
        let extras = RobotManifestExtras {
            user_runtimes: BTreeMap::from([(
                "avoid".to_string(),
                phoxal_cli_core::project::resolver::UserRuntimeManifestExtras {
                    config: Some(config),
                },
            )]),
            ..RobotManifestExtras::default()
        };

        run_check_with_context(
            &[],
            &[],
            &sources,
            CheckGraphContext {
                manifest_extras: &extras,
            },
            |_| bail!("no platform images should be fetched"),
            |_| bail!("no tools should be fetched"),
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
    let extras = RobotManifestExtras::default();

    let outcome = run_check_with_context(
        &[],
        &[],
        &sources,
        CheckGraphContext {
            manifest_extras: &extras,
        },
        |_| bail!("no platform images should be fetched"),
        |_| bail!("no tools should be fetched"),
        |_| {
            let mut raw = raw("optional", "v1", &[]);
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
    let extras = RobotManifestExtras::default();

    let outcome = run_check_with_context(
        &[],
        &[],
        &sources,
        CheckGraphContext {
            manifest_extras: &extras,
        },
        |_| bail!("no platform images should be fetched"),
        |_| bail!("no tools should be fetched"),
        |_| {
            let mut raw = raw("required", "v1", &[]);
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
    let extras = RobotManifestExtras {
        user_runtimes: BTreeMap::from([(
            "avoid".to_string(),
            phoxal_cli_core::project::resolver::UserRuntimeManifestExtras {
                config: Some(serde_json::json!({
                    "gains": [0.25, 5.5],
                    "mode": "FAST",
                    "extra": true
                })),
            },
        )]),
        ..RobotManifestExtras::default()
    };

    let outcome = run_check_with_context(
        &[],
        &[],
        &sources,
        CheckGraphContext {
            manifest_extras: &extras,
        },
        |_| bail!("no platform images should be fetched"),
        |_| bail!("no tools should be fetched"),
        |_| {
            let mut raw = raw("avoid", "v1", &[]);
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

/// Writes a marker file so the tempdir hashes distinctly under
/// `hash_tree` (an empty directory always hashes to the SHA-256 of empty
/// input, so two unrelated empty fixture crates would otherwise collide
/// on the same source-tree hash and thus the same cache path).
fn fixture_crate_dir(temp: &tempfile::TempDir, marker: &str) -> PathBuf {
    std::fs::write(temp.path().join("Cargo.toml"), marker).expect("write fixture marker");
    temp.path().to_path_buf()
}

fn raw(id: &str, api_version: &str, contracts: &[&str]) -> RawEmitApis {
    raw_kind("service", id, api_version, contracts)
}

/// Like `raw`, but each contract carries an explicit `role` (D1: the
/// wire-shape-agreement axis `schema_id` used to gate is gone - two
/// participants naming the same `family` are compatible by construction
/// regardless of role, so this only exists for tests that want to spell
/// out a specific publish/subscribe/serve/ask role).
fn raw_with_role(id: &str, api_version: &str, contracts: &[(&str, &str)]) -> RawEmitApis {
    raw_kind_with_role("service", id, api_version, contracts, "checked")
}

fn raw_kind_with_role(
    kind: &str,
    id: &str,
    api_version: &str,
    contracts: &[(&str, &str)],
    participant_class: &str,
) -> RawEmitApis {
    RawEmitApis {
        artifact: RawArtifact {
            kind: kind.to_string(),
            id: id.to_string(),
        },
        participant_class: participant_class.to_string(),
        api_version: api_version.to_string(),
        required_contracts: contracts
            .iter()
            .map(
                |(family, role)| participant_metadata::ParticipantMetaContract {
                    role: (*role).to_string(),
                    version: family
                        .split_once("::")
                        .map_or(api_version, |(version, _)| version)
                        .to_string(),
                    contract: family
                        .split_once("::")
                        .map_or(*family, |(_, contract)| contract)
                        .to_string(),
                    external: false,
                },
            )
            .collect(),
        config_schema: None,
    }
}

fn raw_kind(kind: &str, id: &str, api_version: &str, contracts: &[&str]) -> RawEmitApis {
    raw_kind_class(kind, id, api_version, contracts, "checked")
}

fn raw_kind_class(
    kind: &str,
    id: &str,
    api_version: &str,
    contracts: &[&str],
    participant_class: &str,
) -> RawEmitApis {
    RawEmitApis {
        artifact: RawArtifact {
            kind: kind.to_string(),
            id: id.to_string(),
        },
        participant_class: participant_class.to_string(),
        api_version: api_version.to_string(),
        required_contracts: contracts
            .iter()
            .map(|family| participant_metadata::ParticipantMetaContract {
                // A single default role: nothing in these fixtures cares
                // about role identity (D1: only `family` decides
                // compatibility), so every contract shares one.
                role: "publish".to_string(),
                version: family
                    .split_once("::")
                    .map_or(api_version, |(version, _)| version)
                    .to_string(),
                contract: family
                    .split_once("::")
                    .map_or(*family, |(_, contract)| contract)
                    .to_string(),
                external: false,
            })
            .collect(),
        config_schema: None,
    }
}

fn resolved_with_components(components: Vec<ResolvedComponent>) -> Result<ResolvedRobot> {
    Ok(ResolvedRobot {
        robot: Robot::parse_from_string(MINIMAL_ROBOT)?,
        channel: phoxal_cli_core::project::catalog::SelectionChannel::Stable,
        target: crate::resolver::host_target_triple(),
        catalog_snapshot: None,
        platform_runtimes: Vec::new(),
        simulators: Vec::new(),
        user_runtimes: Vec::new(),
        components,
        tools: Vec::new(),
        path_overrides: Vec::new(),
    })
}

const MINIMAL_ROBOT: &str = r#"schema: robot/v0
robot:
  id: testbot
  namespace: test
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
artifacts:
  channel: stable
"#;
