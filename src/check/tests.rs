//! Tests for this module.

use super::build::build_participant_report_from_source_with_diagnostics;
use super::participants::{
    component_driver_platform_refs_from_resolved, platform_artifact_refs_from_resolved,
};
use super::*;
use anyhow::{Result, anyhow, bail};
use graph_check::{ParticipantClass, Problem};
use phoxal::model::robot::v0::Robot;
use phoxal_cli_core::check::source::{SourceParticipant, SourceParticipantKind, ToolParticipant};
use phoxal_cli_core::project::catalog::ArtifactKind;
use phoxal_cli_core::project::launch_plan::RunIdentity;
use phoxal_cli_core::project::launch_plan::{
    CheckedRobotLaunchInput, LaunchMode, ROBOT_TOOL_DEVICE, ROBOT_TOOL_JOYPAD, build_launch_plan,
};
use phoxal_cli_core::project::resolver::{
    ResolveOptions, ResolvedComponent, ResolvedComponentPackage, ResolvedComponentSource,
    ResolvedPlatformRuntime, ResolvedRobot, ResolvedTool,
};
use std::path::{Path, PathBuf};

use crate::host_paths::test_support::ScratchPhoxalHome;
use crate::resolver::resolve;

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

#[test]
fn launch_plan_covers_services_services_and_component_instances() -> Result<()> {
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let temp = tempfile::tempdir()?;
    std::fs::create_dir_all(temp.path().join("services/mission/src"))?;
    std::fs::create_dir_all(temp.path().join("components/ddsm115/src"))?;
    std::fs::create_dir_all(temp.path().join("src"))?;
    std::fs::create_dir_all(temp.path().join("train/phoxal/src"))?;
    std::fs::write(
        temp.path().join("services/mission/Cargo.toml"),
        "[package]\nname = \"mission\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    std::fs::write(
        temp.path().join("services/mission/src/main.rs"),
        "fn main() {}",
    )?;
    std::fs::write(
        temp.path().join("components/ddsm115/Cargo.toml"),
        "[package]\nname = \"ddsm115\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[[bin]]\nname = \"ddsm115\"\npath = \"src/main.rs\"\n",
    )?;
    std::fs::write(
        temp.path().join("components/ddsm115/src/main.rs"),
        "fn main() {}",
    )?;
    std::fs::write(
        temp.path().join("components/ddsm115/component.yaml"),
        "schema: component/v0\n",
    )?;
    std::fs::write(
        temp.path().join("Cargo.toml"),
        "[package]\nname = \"robot\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n[workspace]\nmembers = [\".\", \"services/mission\", \"components/ddsm115\"]\nresolver = \"2\"\n\n[dependencies]\nphoxal = { path = \"train/phoxal\" }\n",
    )?;
    std::fs::write(temp.path().join("src/lib.rs"), "")?;
    std::fs::write(
        temp.path().join("train/phoxal/Cargo.toml"),
        "[package]\nname = \"phoxal\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    std::fs::write(temp.path().join("train/phoxal/src/lib.rs"), "")?;
    std::fs::write(
        temp.path().join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"ddsm115\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"mission\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"phoxal\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"robot\"\nversion = \"0.1.0\"\ndependencies = [\"phoxal\"]\n",
    )?;
    let mut robot = Robot::parse_from_string(LAUNCH_PLAN_FIXTURE_ROBOT)?;
    robot
        .services
        .get_mut("mission")
        .expect("mission service")
        .config = Some(serde_json::json!({
        "message": "line\nquoted \"value\"",
    }));
    // `ddsm115` resolves from the `components/` workspace crate above -
    // no network, unlike a registry-resolved component.
    let mut resolved = resolve(&robot, temp.path(), ResolveOptions::default())?;
    add_launch_plan_robot_tools(&mut resolved);
    let source_participants = vec![
        SourceParticipant::user_service("mission", temp.path().join("services/mission")),
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
            robot: Some(&robot),
        },
        |artifact_ref| {
            let participant = platform_refs
                .iter()
                .find(|participant| participant.binary_name == artifact_ref)
                .ok_or_else(|| anyhow!("unexpected platform artifact {artifact_ref}"))?;
            Ok(launch_plan_raw_participant_report(
                participant.kind.wire_kind(),
                &participant.name,
            ))
        },
        |_| bail!("no tools in this check fixture"),
        |source| match source.kind {
            SourceParticipantKind::UserTool => {
                Ok(launch_plan_raw_participant_report("tool", &source.name))
            }
            SourceParticipantKind::UserService => {
                Ok(launch_plan_raw_participant_report("service", &source.name))
            }
            SourceParticipantKind::ComponentDriver => Ok(launch_plan_raw_participant_report(
                "driver",
                &source.expected_artifact_id,
            )),
            SourceParticipantKind::OfficialService => Ok(launch_plan_raw_participant_report(
                "service",
                &source.expected_artifact_id,
            )),
            SourceParticipantKind::Tool => Ok(launch_plan_raw_participant_report(
                "tool",
                &source.expected_artifact_id,
            )),
            SourceParticipantKind::Simulator => Ok(launch_plan_raw_participant_report(
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
            checked_participants: &outcome.checked_participants,
            source_participants: &source_participants,
        }],
        RunIdentity::default(),
    )?;

    assert_eq!(plan.mode, LaunchMode::Run);
    let robot = &plan.robots[0];
    assert_eq!(robot.id, "testbot");
    let participant_ids = robot
        .participants
        .iter()
        .map(|participant| participant.launch.participant_id.as_str())
        .collect::<Vec<_>>();
    for service in resolved
        .platform_runtimes
        .iter()
        .map(|runtime| runtime.name.as_str())
    {
        assert!(
            participant_ids.contains(&service),
            "missing platform service {service}: {participant_ids:?}"
        );
    }
    assert!(participant_ids.contains(&"left_drive"));
    assert!(participant_ids.contains(&"right_drive"));
    assert!(participant_ids.contains(&"tool-bus-testbot"));
    assert!(participant_ids.contains(&"tool-log-testbot"));
    assert!(participant_ids.contains(&"tool-telemetry-testbot"));
    assert!(participant_ids.contains(&"tool-device-testbot"));
    assert!(participant_ids.contains(&"tool-joypad-testbot"));
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

fn launch_plan_raw_participant_report(kind: &str, id: &str) -> RawParticipantReport {
    RawParticipantReport {
        artifact: RawArtifact {
            kind: kind.to_string(),
            id: id.to_string(),
        },
        participant_class: "checked".to_string(),
        config_schema: None,
    }
}

#[test]
fn a_user_tool_is_checked_and_its_config_is_validated() -> Result<()> {
    // A declared user tool is an ordinary checked participant (#950): its
    // embedded metadata must be kind `tool`, and its `tools.<id>.config` is
    // validated against the emitted schema exactly like a user service.
    let robot = Robot::parse_from_string(
        r#"schema: robot/v0
robot:
  id: bot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
  components: {}
tools:
  lidar-viz:
    config:
      port: 9000
"#,
    )?;
    let source = vec![SourceParticipant::user_tool(
        "lidar-viz",
        std::path::PathBuf::from("tools/lidar-viz"),
    )];

    // The tool emits a schema requiring a STRING port; the authored 9000 is an
    // integer, so the check must surface an InvalidConfig problem for it.
    let build = |participant: &SourceParticipant| {
        let mut raw = launch_plan_raw_participant_report("tool", &participant.name);
        raw.config_schema = Some(serde_json::json!({
            "type": "object",
            "properties": {"port": {"type": "string"}},
            "required": ["port"],
        }));
        Ok(raw)
    };
    let outcome = run_check_with_context(
        &[],
        &[],
        &source,
        CheckGraphContext {
            robot: Some(&robot),
        },
        |artifact_ref| bail!("unexpected official artifact {artifact_ref}"),
        |_| bail!("no privileged tools in this fixture"),
        build,
    )?;
    let problems = format!("{outcome:?}");
    assert!(
        problems.contains("lidar-viz") && problems.to_lowercase().contains("config"),
        "user-tool config must be validated: {problems}"
    );

    // The kind gate rejects a user tool whose binary emits a non-tool kind.
    let bad_kind = |participant: &SourceParticipant| {
        Ok(launch_plan_raw_participant_report(
            "service",
            &participant.name,
        ))
    };
    let error = run_check_with_context(
        &[],
        &[],
        &source,
        CheckGraphContext {
            robot: Some(&robot),
        },
        |artifact_ref| bail!("unexpected official artifact {artifact_ref}"),
        |_| bail!("no privileged tools in this fixture"),
        bad_kind,
    )
    .expect_err("a user tool emitting a non-tool kind must fail identity validation");
    assert!(format!("{error:#}").contains("kind"), "{error:#}");
    Ok(())
}

fn add_launch_plan_robot_tools(resolved: &mut ResolvedRobot) {
    resolved.tools.push(launch_plan_tool("tool-bus"));
    resolved.tools.push(launch_plan_tool(ROBOT_TOOL_JOYPAD));
    resolved.tools.push(launch_plan_tool("tool-log"));
    resolved.tools.push(launch_plan_tool("tool-telemetry"));
    resolved.tools.push(launch_plan_tool(ROBOT_TOOL_DEVICE));
}

fn launch_plan_tool(name: &str) -> ResolvedTool {
    ResolvedTool {
        kind: ArtifactKind::Tool,
        name: name.to_string(),
        package: format!("phoxal/{name}"),
        binary_name: name.to_string(),
        path_override: None,
        train: "0.36.0".to_string(),
        target: crate::resolver::host_target_triple(),
    }
}

const LAUNCH_PLAN_FIXTURE_ROBOT: &str = r#"schema: robot/v0
robot:
  id: testbot
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
  mission: {}
"#;

fn robot_with_service_config(service_id: &str, config: Value) -> Result<Robot> {
    let mut robot =
        Robot::parse_from_string(&LAUNCH_PLAN_FIXTURE_ROBOT.replace("mission", service_id))?;
    robot
        .services
        .get_mut(service_id)
        .expect("fixture service")
        .config = Some(config);
    Ok(robot)
}

fn fixture_component_package(
    package: &str,
    kind: ArtifactKind,
    path: &str,
) -> ResolvedComponentPackage {
    ResolvedComponentPackage {
        package: package.to_string(),
        kind,
        source: ResolvedComponentSource::Path {
            path: PathBuf::from(path),
        },
        resolved_dir: Some(PathBuf::from(path)),
        registry_runtime: None,
    }
}

/// A registry-sourced component package with a populated `registry_runtime`,
/// the shape `resolve_components` produces for a package with no matching
/// `components/` workspace crate.
fn fixture_registry_component_package(
    package: &str,
    kind: ArtifactKind,
    component_name: &str,
) -> ResolvedComponentPackage {
    ResolvedComponentPackage {
        package: package.to_string(),
        kind,
        source: ResolvedComponentSource::Registry,
        resolved_dir: (kind == ArtifactKind::ComponentAssets)
            .then(|| PathBuf::from(format!("registry/{component_name}"))),
        registry_runtime: (kind == ArtifactKind::ComponentDriver).then(|| {
            ResolvedPlatformRuntime {
                name: component_name.to_string(),
                package: package.to_string(),
                kind,
                path_override: None,
                train: "0.36.0".to_string(),
                target: Some("aarch64-unknown-linux-gnu".to_string()),
            }
        }),
    }
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
        &[],
        &sources,
        CheckGraphContext { robot: None },
        |image_ref| match image_ref {
            "mission:ok" => Ok(raw("mission")),
            unexpected => bail!("unexpected image {unexpected}"),
        },
        |_| bail!("no tools should be fetched"),
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
        &[],
        &sources,
        CheckGraphContext { robot: None },
        |image_ref| match image_ref {
            "mission:ok" => Ok(raw("mission")),
            unexpected => bail!("unexpected image {unexpected}"),
        },
        |_| bail!("no tools should be fetched"),
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
fn privileged_tools_and_checked_sources_coexist_in_one_graph() -> Result<()> {
    // A privileged tool and a checked source participant appear in the same
    // check run without incident.
    let tools = vec![ToolParticipant {
        name: "joypad".to_string(),
        binary_path: PathBuf::from("/fake/cache/joypad"),
    }];
    let sources = vec![SourceParticipant::user_service(
        "drive".to_string(),
        PathBuf::from("/fake/project/runtimes/drive"),
    )];

    let outcome = run_check_with_context(
        &[],
        &tools,
        &sources,
        CheckGraphContext { robot: None },
        |_| bail!("no platform images should be fetched"),
        |tool| {
            let path = tool.binary_path.as_path();
            if path == Path::new("/fake/cache/joypad") {
                Ok(raw_kind_class("tool", "joypad", "privileged"))
            } else {
                bail!("unexpected tool path {}", path.display())
            }
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
fn privileged_tools_are_exempt_from_topology() -> Result<()> {
    let tools = vec![ToolParticipant {
        name: "joypad".to_string(),
        binary_path: PathBuf::from("/fake/cache/joypad"),
    }];

    let outcome = run_check_with_context(
        &[],
        &tools,
        &[],
        CheckGraphContext { robot: None },
        |_| bail!("no platform images should be fetched"),
        |tool| {
            let path = tool.binary_path.as_path();
            if path == Path::new("/fake/cache/joypad") {
                Ok(raw_kind_class("tool", "joypad", "privileged"))
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
fn source_and_platform_participants_coexist_in_a_healthy_graph() -> Result<()> {
    // A platform participant and a source participant both check clean.
    let images = vec![("mission".to_string(), "mission:ok".to_string())];
    let sources = vec![SourceParticipant::user_service(
        "drive".to_string(),
        PathBuf::from("/fake/project/runtimes/drive"),
    )];

    let outcome = run_check_with_context(
        &platform_refs(&images),
        &[],
        &sources,
        CheckGraphContext { robot: None },
        |image_ref| match image_ref {
            "mission:ok" => Ok(raw("mission")),
            unexpected => bail!("unexpected image {unexpected}"),
        },
        |_| bail!("no tools should be fetched"),
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
        &[],
        &sources,
        CheckGraphContext { robot: None },
        |_| bail!("no platform images should be fetched"),
        |_| bail!("no tools should be fetched"),
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
        &[],
        CheckGraphContext { robot: None },
        |image_ref| match image_ref {
            "drive:swapped" => Ok(raw("mission")),
            unexpected => bail!("unexpected image {unexpected}"),
        },
        |_| bail!("no tools should be fetched"),
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
        &[],
        CheckGraphContext { robot: None },
        |artifact_ref| match artifact_ref {
            "driver-bno085:swapped" => Ok(raw_kind("service", "bno085")),
            unexpected => bail!("unexpected artifact {unexpected}"),
        },
        |_| bail!("no tools should be fetched"),
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
fn tool_artifact_identity_must_match_resolved_tool() {
    let tools = vec![ToolParticipant {
        name: "joypad".to_string(),
        binary_path: PathBuf::from("/fake/cache/joypad"),
    }];

    let error = run_check_with_context(
        &[],
        &tools,
        &[],
        CheckGraphContext { robot: None },
        |_| bail!("no platform images should be fetched"),
        |tool| {
            let path = tool.binary_path.as_path();
            if path == Path::new("/fake/cache/joypad") {
                Ok(raw_kind_class(
                    "tool",
                    "simulator_webots_controller",
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
        message.contains("tool participant report artifact.id 'simulator_webots_controller'")
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

    let outcome = run_check_with_context(
        &[],
        &tools,
        &[],
        CheckGraphContext { robot: None },
        |_| bail!("no platform images should be fetched"),
        |tool| {
            let path = tool.binary_path.as_path();
            if path == Path::new("/fake/cache/joypad") {
                Ok(raw_kind_class("tool", "joypad", "privileged"))
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

    let error = run_check_with_context(
        &[],
        &tools,
        &[],
        CheckGraphContext { robot: None },
        |_| bail!("no platform images should be fetched"),
        |tool| {
            let path = tool.binary_path.as_path();
            if path == Path::new("/fake/cache/joypad") {
                Ok(raw_kind_class("runtime", "joypad", "privileged"))
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
            "tool participant report artifact.kind 'runtime' does not match the expected kind 'tool'"
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

    let outcome = run_check_with_context(
        &[],
        &[],
        &sources,
        CheckGraphContext { robot: None },
        |_| bail!("no platform images should be fetched"),
        |_| bail!("no tools should be fetched"),
        |_| Ok(raw_kind_class("driver", "ddsm115", "checked")),
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
        &[],
        &sources,
        CheckGraphContext { robot: None },
        |_| bail!("no platform images should be fetched"),
        |_| bail!("no tools should be fetched"),
        |_| Ok(raw_kind_class("runtime", "ddsm115", "checked")),
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
fn tool_artifact_kind_garbage_is_rejected() {
    let tools = vec![ToolParticipant {
        name: "joypad".to_string(),
        binary_path: PathBuf::from("/fake/cache/joypad"),
    }];

    let error = run_check_with_context(
        &[],
        &tools,
        &[],
        CheckGraphContext { robot: None },
        |_| bail!("no platform images should be fetched"),
        |tool| {
            let path = tool.binary_path.as_path();
            if path == Path::new("/fake/cache/joypad") {
                Ok(raw_kind_class("nonsense", "joypad", "privileged"))
            } else {
                bail!("unexpected tool path {}", path.display())
            }
        },
        |_| bail!("no source services should be built"),
    )
    .expect_err("tool binary reporting a garbage kind should abort check");

    let message = error.to_string();
    assert!(
        message.contains("tool participant report artifact.kind 'nonsense'")
            && message.contains("expected kind 'tool'"),
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
        &[],
        &sources,
        CheckGraphContext { robot: None },
        |_| bail!("no platform images should be fetched"),
        |_| bail!("no tools should be fetched"),
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
        &[],
        &sources,
        CheckGraphContext { robot: None },
        |_| bail!("no platform images should be fetched"),
        |_| bail!("no tools should be fetched"),
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
        &[],
        &sources,
        CheckGraphContext { robot: None },
        |_| bail!("no platform images should be fetched"),
        |_| bail!("no tools should be fetched"),
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
fn build_participant_report_from_source_never_caches_across_calls() -> Result<()> {
    // Two back-to-back calls for the same crate dir each invoke the fake build
    // closure; source inspection does not retain state between calls.
    let temp = tempfile::tempdir()?;
    let crate_dir = fixture_crate_dir(&temp, "sibling");
    let participant = SourceParticipant::user_service("sibling", crate_dir.clone());

    let mut build_count = 0;
    let first = build_participant_report_from_source_with_diagnostics(
        &participant,
        |_| {
            build_count += 1;
            Ok(raw("sibling"))
        },
        None,
    )?;
    let second = build_participant_report_from_source_with_diagnostics(
        &participant,
        |_| {
            build_count += 1;
            Ok(raw("sibling"))
        },
        None,
    )?;

    assert_eq!(build_count, 2, "every call must rebuild, nothing is cached");
    assert_eq!(first, second);
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
        &[],
        &sources,
        CheckGraphContext { robot: None },
        |image_ref| match image_ref {
            "mission:ok" => Ok(raw("mission")),
            unexpected => bail!("unexpected image {unexpected}"),
        },
        |_| bail!("no tools should be fetched"),
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
        &[],
        &sources,
        CheckGraphContext { robot: None },
        |_| bail!("no platform images should be fetched"),
        |_| bail!("no tools should be fetched"),
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
        &[],
        &sources,
        CheckGraphContext { robot: None },
        |_| bail!("no platform images should be fetched"),
        |_| bail!("no tools should be fetched"),
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
            assets: (fixture_component_package(
                "phoxal/component-ddsm115",
                ArtifactKind::ComponentAssets,
                "components/ddsm115",
            )),
            driver: Some(fixture_component_package(
                "phoxal/component-ddsm115",
                ArtifactKind::ComponentDriver,
                "components/ddsm115",
            )),
            has_driver: true,
        },
        ResolvedComponent {
            instance: "caster".to_string(),
            source_name: "passive_caster".to_string(),
            assets: (fixture_component_package(
                "phoxal/component-passive_caster",
                ArtifactKind::ComponentAssets,
                "components/passive_caster",
            )),
            driver: None,
            has_driver: false,
        },
    ])?;
    // Resolution already settled each package's on-disk directory into
    // `resolved_dir` (`driver_path_override()`), so the locator callback is
    // never reached - only a driverless component (no driver package at all)
    // would ever have nothing to resolve.
    let source_participants =
        source_participants_from_resolved(temp.path(), &resolved, |_component, _project_root| {
            panic!("resolved_dir is always pre-populated; the locator callback is never reached")
        })?;

    assert_eq!(
        source_participants,
        vec![SourceParticipant::component_driver_with_artifact_id(
            "left_drive".to_string(),
            "ddsm115".to_string(),
            PathBuf::from("components/ddsm115")
        )]
    );

    let mut built = Vec::new();
    let outcome = run_check_with_context(
        &[],
        &[],
        &source_participants,
        CheckGraphContext { robot: None },
        |_| bail!("no platform images should be fetched"),
        |_| bail!("no tools should be fetched"),
        |participant| {
            let dir = participant.crate_dir.as_path();
            built.push(dir.to_path_buf());
            Ok(raw_kind("driver", "ddsm115"))
        },
    )?;

    assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
    assert_eq!(built, vec![PathBuf::from("components/ddsm115")]);
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
        assets: (fixture_registry_component_package(
            "phoxal/component-ddsm115",
            ArtifactKind::ComponentAssets,
            "ddsm115",
        )),
        driver: Some(fixture_registry_component_package(
            "phoxal/component-ddsm115",
            ArtifactKind::ComponentDriver,
            "ddsm115",
        )),
        has_driver: true,
    }])?;

    let source_participants =
        source_participants_from_resolved(temp.path(), &resolved, |component, _project_root| {
            panic!(
                "a registry-sourced driver for '{}' must never reach the source-crate locator",
                component.instance
            )
        })?;
    assert!(
        source_participants.is_empty(),
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
            assets: (fixture_registry_component_package(
                "phoxal/component-ddsm115",
                ArtifactKind::ComponentAssets,
                "ddsm115",
            )),
            driver: Some(fixture_registry_component_package(
                "phoxal/component-ddsm115",
                ArtifactKind::ComponentDriver,
                "ddsm115",
            )),
            has_driver: true,
        },
        ResolvedComponent {
            instance: "right_drive".to_string(),
            source_name: "ddsm115".to_string(),
            assets: (fixture_registry_component_package(
                "phoxal/component-ddsm115",
                ArtifactKind::ComponentAssets,
                "ddsm115",
            )),
            driver: Some(fixture_registry_component_package(
                "phoxal/component-ddsm115",
                ArtifactKind::ComponentDriver,
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
            panic!("registry drivers never reach the source locator")
        })?;
    assert!(source_participants.is_empty());

    let mut fetch_calls = 0;
    let outcome = run_check_with_context(
        &platform_refs,
        &[],
        &source_participants,
        CheckGraphContext { robot: None },
        |artifact_ref| {
            fetch_calls += 1;
            assert_eq!(artifact_ref, "phoxal-component-ddsm115");
            Ok(raw_kind("driver", "ddsm115"))
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
fn driverless_registry_component_stages_assets_only_and_is_not_a_check_participant() -> Result<()> {
    // Component assets contribute no contracts and are never a check
    // participant, registry-sourced or not; a driverless instance yields
    // no source participant and no platform ref.
    let temp = tempfile::tempdir()?;
    let resolved = resolved_with_components(vec![ResolvedComponent {
        instance: "caster".to_string(),
        source_name: "passive_caster".to_string(),
        assets: (fixture_registry_component_package(
            "phoxal/component-passive_caster",
            ArtifactKind::ComponentAssets,
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
    let outcome = run_check_with_context(
        &platform_refs,
        &[],
        &source_participants,
        CheckGraphContext { robot: None },
        |_| bail!("path-overridden service should not read registry metadata"),
        |_| bail!("no tools in this fixture"),
        |participant| {
            assert_eq!(participant.kind, SourceParticipantKind::OfficialService);
            Ok(raw_kind("service", "drive"))
        },
    )?;
    assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
    Ok(())
}

#[test]
fn raw_participant_report_unknown_participant_class_defaults_to_checked() -> Result<()> {
    let mut raw = raw("drive");
    raw.participant_class = "future".to_string();
    let participant = graph_check::ParticipantApis::try_from(raw)?;

    assert_eq!(participant.participant_class, ParticipantClass::Checked);
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
        participant_class: "checked".to_string(),
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
            &[],
            &sources,
            CheckGraphContext {
                robot: Some(&robot),
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

    let outcome = run_check_with_context(
        &[],
        &[],
        &sources,
        CheckGraphContext { robot: None },
        |_| bail!("no platform images should be fetched"),
        |_| bail!("no tools should be fetched"),
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
        &[],
        &sources,
        CheckGraphContext { robot: None },
        |_| bail!("no platform images should be fetched"),
        |_| bail!("no tools should be fetched"),
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
        &[],
        &sources,
        CheckGraphContext {
            robot: Some(&robot),
        },
        |_| bail!("no platform images should be fetched"),
        |_| bail!("no tools should be fetched"),
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

/// Writes a marker file so the tempdir hashes distinctly under
/// `hash_tree` (an empty directory always hashes to the SHA-256 of empty
/// input, so two unrelated empty fixture crates would otherwise collide
/// on the same source-tree hash and thus the same cache path).
fn fixture_crate_dir(temp: &tempfile::TempDir, marker: &str) -> PathBuf {
    std::fs::write(temp.path().join("Cargo.toml"), marker).expect("write fixture marker");
    temp.path().to_path_buf()
}

fn raw(id: &str) -> RawParticipantReport {
    raw_kind("service", id)
}

fn raw_kind(kind: &str, id: &str) -> RawParticipantReport {
    raw_kind_class(kind, id, "checked")
}

fn raw_kind_class(kind: &str, id: &str, participant_class: &str) -> RawParticipantReport {
    RawParticipantReport {
        artifact: RawArtifact {
            kind: kind.to_string(),
            id: id.to_string(),
        },
        participant_class: participant_class.to_string(),
        config_schema: None,
    }
}

fn resolved_with_components(components: Vec<ResolvedComponent>) -> Result<ResolvedRobot> {
    Ok(ResolvedRobot {
        robot: Robot::parse_from_string(MINIMAL_ROBOT)?,
        train: "0.36.0".to_string(),
        target: crate::resolver::host_target_triple(),
        platform_runtimes: Vec::new(),
        simulators: Vec::new(),
        user_runtimes: Vec::new(),
        user_tools: Vec::new(),
        undeclared_runtimes: Vec::new(),
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
"#;
