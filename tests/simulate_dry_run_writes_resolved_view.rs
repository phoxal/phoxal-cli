use std::fs;

use phoxal_cli::commands::simulate::{SimulateOptions, prepare_with_releases};
use phoxal_cli::releases::ReleasesSnapshot;

#[test]
fn simulate_dry_run_writes_resolved_view_and_state() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    write_robot_project(temp.path())?;

    let snapshot = releases_snapshot();
    let plan = prepare_with_releases(
        temp.path(),
        SimulateOptions {
            world: "test".to_string(),
            rerun_proxy: true,
            joypad: true,
            resolve_external_artifacts: false,
            ..SimulateOptions::default()
        },
        &snapshot,
    )?;

    assert!(temp.path().join(".phoxal/run/robot.yaml").is_file());
    assert!(temp.path().join(".phoxal/run/structure.urdf").is_file());
    assert!(
        temp.path()
            .join(".phoxal/webots/protos/Testbot.proto")
            .is_file()
    );
    assert!(
        temp.path()
            .join(".phoxal/webots/protos/components")
            .is_dir()
    );
    assert!(
        temp.path()
            .join(".phoxal/webots/worlds/default.wbt")
            .is_file()
    );
    assert!(
        !temp
            .path()
            .join(".phoxal/webots/controllers/phoxal-simulator-webots-controller/phoxal-simulator-webots-controller")
            .is_file()
    );
    assert!(plan.compose_path.is_file());
    assert!(plan.state_path.is_file());
    assert!(
        plan.written_files
            .iter()
            .any(|path| path.ends_with(".phoxal/run/docker-compose.yml"))
    );
    assert!(
        plan.written_files
            .iter()
            .any(|path| path.ends_with(".phoxal/webots/worlds/default.wbt"))
    );

    let compose = fs::read_to_string(&plan.compose_path)?;
    assert!(compose.starts_with("name: testbot\n"));
    assert!(compose.contains("x-phoxal-native-tools"));
    assert!(compose.contains("rerun_proxy"));
    assert!(compose.contains("joypad"));
    // Offline dry-run must not embed a fabricated runtime image digest: every
    // platform runtime image is a `repo:version` tag ref, so a later live
    // `simulate` can never try to `docker pull repo@sha256:<fake>`. (The zenoh
    // router image is a real, published digest pin and is intentionally exempt.)
    assert!(compose.contains("ghcr.io/phoxal/runtime-"));
    for line in compose.lines() {
        if line.contains("ghcr.io/phoxal/runtime-") {
            assert!(
                !line.contains("@sha256:"),
                "platform runtime image must be a tag ref during offline dry-run, got: {line}"
            );
        }
    }

    let state = fs::read_to_string(&plan.state_path)?;
    assert!(state.contains("mode: dry-run"));
    assert!(!state.contains("simulator_webots_controller"));
    assert!(!state.contains("simulator_webots_supervisor"));
    assert!(state.contains("rerun_proxy"));
    assert!(state.contains("joypad"));
    assert!(state.contains("webots"));

    let world = fs::read_to_string(temp.path().join(".phoxal/webots/worlds/default.wbt"))?;
    assert!(world.contains("controller \"phoxal-simulator-webots-controller\""));
    assert!(world.contains("controller \"phoxal-simulator-webots-supervisor\""));
    assert!(world.contains("controllerArgs"));

    Ok(())
}

fn write_robot_project(root: &std::path::Path) -> anyhow::Result<()> {
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

fn minimal_robot_yaml() -> &'static str {
    r#"version: v1

identity:
  id: testbot
  namespace: test

structure: structure.urdf

phoxal_runtimes:
  version: "latest"

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

fn releases_snapshot() -> ReleasesSnapshot {
    ReleasesSnapshot {
        fetched_at: std::time::SystemTime::UNIX_EPOCH,
        versions: vec!["0.8.0".into()],
    }
}
