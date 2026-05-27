use phoxal_cli::catalog::CATALOG;
use phoxal_cli::resolver::{ResolveOptions, resolve};
use phoxal_utils_robot::Robot;

#[test]
fn resolves_minimal_robot_to_full_platform_set() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml())?;
    let resolved = resolve(&robot, &CATALOG, offline_options())?;

    assert_eq!(resolved.runtime_set_version.to_string(), "0.1.2");
    assert_eq!(
        resolved
            .platform_runtimes
            .iter()
            .map(|runtime| runtime.name.as_str())
            .collect::<Vec<_>>(),
        CATALOG.names().collect::<Vec<_>>()
    );
    assert!(
        resolved
            .platform_runtimes
            .iter()
            .all(|runtime| runtime.image_digest.starts_with("sha256:"))
    );

    Ok(())
}

#[test]
fn unknown_platform_override_fails() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml().replace(
        "phoxal_runtimes:\n  version: \"^0.1\"",
        "phoxal_runtimes:\n  version: \"^0.1\"\n  overrides:\n    nope:\n      version: \"0.1.0\"",
    ))?;

    let error = resolve(&robot, &CATALOG, offline_options()).expect_err("override should fail");
    assert!(error.to_string().contains("not a platform runtime"));

    Ok(())
}

#[test]
fn user_runtime_shadowing_platform_fails() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml().replace(
        "sim:\n  world: sim/worlds/test.wbt",
        "user_runtimes:\n  drive:\n    path: ./runtimes/drive\n\nsim:\n  world: sim/worlds/test.wbt",
    ))?;

    let error = resolve(&robot, &CATALOG, offline_options()).expect_err("shadowing should fail");
    assert!(error.to_string().contains("shadows a platform runtime"));

    Ok(())
}

#[test]
fn missing_instance_source_fails() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml().replace(
        "  sources:\n    ddsm115:\n      path: ./components/ddsm115",
        "  sources: {}",
    ))?;

    let error =
        resolve(&robot, &CATALOG, offline_options()).expect_err("missing source should fail");
    assert!(error.to_string().contains("references missing source"));

    Ok(())
}

fn offline_options() -> ResolveOptions {
    ResolveOptions {
        resolve_external_artifacts: false,
        ..ResolveOptions::default()
    }
}

fn minimal_robot_yaml() -> String {
    r#"version: v1

phoxal:
  cli_min_version: "^0.1"

identity:
  id: testbot
  namespace: test

structure: structure.urdf

phoxal_runtimes:
  version: "^0.1"

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
  sources:
    ddsm115:
      path: ./components/ddsm115
  instances:
    left_drive:
      component: ddsm115
      mount_link: left_wheel_mount
    right_drive:
      component: ddsm115
      mount_link: right_wheel_mount
"#
    .to_string()
}
