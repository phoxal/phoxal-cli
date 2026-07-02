use std::fs;

use phoxal_cli::commands::simulate::{SimulateOptions, prepare};

#[test]
fn simulate_dry_run_writes_resolved_view_and_state() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    write_robot_project(temp.path())?;

    let plan = prepare(
        temp.path(),
        SimulateOptions {
            world: "test".to_string(),
            joypad: true,
            ..SimulateOptions::default()
        },
    )?;

    assert!(temp.path().join(".phoxal/run/robot.yaml").is_file());
    // There is no lockfile: dry-run resolves live and writes none.
    assert!(!temp.path().join("phoxal.sources.lock").exists());
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
    assert!(plan.state_path.is_file());
    let mut run_files = fs::read_dir(temp.path().join(".phoxal/run"))?
        .map(|entry| {
            entry.map(|entry| {
                entry
                    .path()
                    .file_name()
                    .expect("file name")
                    .to_string_lossy()
                    .to_string()
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    run_files.sort();
    assert_eq!(
        run_files,
        vec!["components", "robot.yaml", "structure.urdf"]
    );
    assert!(
        plan.written_files
            .iter()
            .any(|path| path.ends_with(".phoxal/webots/worlds/default.wbt"))
    );
    let run_robot = fs::read_to_string(temp.path().join(".phoxal/run/robot.yaml"))?;
    assert!(run_robot.contains("api_version: y2026_1"));
    assert!(run_robot.contains("channel: stable"));
    assert_eq!(plan.bus_connect, "tcp/127.0.0.1:7447");
    assert_eq!(
        plan.resolved
            .platform_runtimes
            .iter()
            .find(|runtime| runtime.name == "drive")
            .expect("drive runtime")
            .artifact_ref(),
        "service-drive:y2026_1-stable"
    );

    let state = fs::read_to_string(&plan.state_path)?;
    assert!(state.contains("mode: dry-run"));
    assert!(!state.contains("simulator_webots_controller"));
    assert!(!state.contains("simulator_webots_supervisor"));
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
    r#"schema: v0
api_version: y2026_1

identity:
  id: testbot
  namespace: test

structure: structure.urdf

phoxal_participants:
  channel: stable

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
