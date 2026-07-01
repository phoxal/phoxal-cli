use std::fs;

use phoxal_cli::commands::simulate::{SimulateOptions, prepare};
use serde_yaml::{Mapping, Value};

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
    assert!(compose.contains("joypad"));
    let compose_yaml: Value = serde_yaml::from_str(&compose)?;
    let services = mapping(
        compose_yaml
            .as_mapping()
            .and_then(|root| root.get(key("services")))
            .expect("services"),
        "services",
    )?;
    let drive = mapping(
        services.get(key("drive")).expect("drive service"),
        "drive service",
    )?;
    let environment = mapping(
        drive.get(key("environment")).expect("drive environment"),
        "drive environment",
    )?;
    assert_eq!(environment.get(key("PHOXAL_NAMESPACE")), Some(&key("test")));
    assert_eq!(
        environment.get(key("PHOXAL_ROBOT_ID")),
        Some(&key("testbot"))
    );
    assert_eq!(
        environment.get(key("PHOXAL_BUNDLE_ROOT")),
        Some(&key("/robot"))
    );
    assert_eq!(
        environment.get(key("PHOXAL_CONNECT")),
        Some(&key("tcp/router:7447"))
    );
    assert_eq!(
        environment.get(key("PHOXAL_CLOCK")),
        Some(&key("simulation"))
    );
    assert!(environment.get(key("ROBOT_CONFIG")).is_none());
    assert!(environment.get(key("ROBOT_ID")).is_none());
    assert!(environment.get(key("ROBOT_NAMESPACE")).is_none());
    assert!(environment.get(key("ROBOT_ROUTER_ENDPOINT")).is_none());
    assert!(environment.get(key("ROBOT_SIMULATION")).is_none());
    // Offline dry-run must not embed a fabricated runtime image digest: every
    // platform runtime image is a `repo:version` tag ref, so a later live
    // `simulate` can never try to `docker pull repo@sha256:<fake>`. (The zenoh
    // router image is a real, published digest pin and is intentionally exempt.)
    assert!(compose.contains("ghcr.io/phoxal/service-"));
    assert!(compose.contains("ghcr.io/phoxal/service-drive:y2026_1-stable"));
    for line in compose.lines() {
        if line.contains("ghcr.io/phoxal/service-") {
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

fn mapping<'a>(value: &'a Value, label: &str) -> anyhow::Result<&'a Mapping> {
    value
        .as_mapping()
        .ok_or_else(|| anyhow::anyhow!("{label} is not a mapping"))
}

fn key(value: &str) -> Value {
    Value::String(value.to_string())
}
