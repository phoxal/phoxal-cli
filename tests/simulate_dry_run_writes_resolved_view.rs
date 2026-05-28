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
            rerun_proxy: true,
            joypad: true,
            resolve_external_artifacts: false,
            ..SimulateOptions::default()
        },
        &snapshot,
    )?;

    assert!(temp.path().join(".phoxal/run/robot.yaml").is_file());
    assert!(temp.path().join(".phoxal/run/structure.urdf").is_file());
    assert!(plan.compose_path.is_file());
    assert!(plan.state_path.is_file());
    assert!(
        plan.written_files
            .iter()
            .any(|path| path.ends_with(".phoxal/run/docker-compose.yml"))
    );

    let compose = fs::read_to_string(&plan.compose_path)?;
    assert!(compose.contains("x-phoxal-native-tools"));
    assert!(compose.contains("rerun_proxy"));
    assert!(compose.contains("joypad"));

    let state = fs::read_to_string(&plan.state_path)?;
    assert!(state.contains("mode: dry-run"));
    assert!(state.contains("simulator_webots_controller"));
    assert!(state.contains("simulator_webots_supervisor"));
    assert!(state.contains("rerun_proxy"));
    assert!(state.contains("joypad"));
    assert!(state.contains("webots"));

    Ok(())
}

fn write_robot_project(root: &std::path::Path) -> anyhow::Result<()> {
    fs::write(root.join("robot.yaml"), minimal_robot_yaml())?;
    fs::write(
        root.join("structure.urdf"),
        r#"<robot name="testbot"><link name="base_link"/></robot>"#,
    )?;
    Ok(())
}

fn minimal_robot_yaml() -> &'static str {
    r#"version: v1

phoxal:
  cli_min_version: "^0.1"

identity:
  id: testbot
  namespace: test

structure: structure.urdf

phoxal_runtimes:
  version: "latest"

sim:
  world: sim/worlds/test.wbt

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
        versions: vec!["0.0.0-dev".into()],
    }
}
