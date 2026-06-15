use phoxal::model::robot::RobotV1 as Robot;
use phoxal_cli::catalog::CATALOG;
use phoxal_cli::releases::ReleasesSnapshot;
use phoxal_cli::resolver::{ResolveOptions, resolve_with_releases};

#[test]
fn resolves_minimal_robot_to_full_platform_set() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml())?;
    let snapshot = releases_snapshot();
    let resolved = resolve_with_releases(&robot, &CATALOG, offline_options(), &snapshot)?;

    assert_eq!(resolved.runtime_set_version.to_string(), "0.8.0");
    assert_eq!(
        resolved
            .platform_runtimes
            .iter()
            .map(|runtime| runtime.name.as_str())
            .collect::<Vec<_>>(),
        CATALOG.names().collect::<Vec<_>>()
    );
    // Offline resolution must never fabricate an OCI digest: every runtime
    // stays unpinned and deploys by its `repo:version` tag, not `repo@sha256:…`.
    assert!(
        resolved
            .platform_runtimes
            .iter()
            .all(|runtime| runtime.digest_pin().is_none())
    );
    assert!(resolved.platform_runtimes.iter().all(|runtime| {
        let deploy = runtime.deploy_ref();
        !deploy.contains('@') && deploy.ends_with(":0.8.0")
    }));

    Ok(())
}

#[test]
fn unknown_platform_override_fails() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml().replace(
        "phoxal_runtimes:\n  version: \"latest\"",
        "phoxal_runtimes:\n  version: \"latest\"\n  overrides:\n    nope:\n      version: \"0.0.0-dev\"",
    ))?;

    let snapshot = releases_snapshot();
    let error = resolve_with_releases(&robot, &CATALOG, offline_options(), &snapshot)
        .expect_err("override should fail");
    assert!(error.to_string().contains("not a platform runtime"));

    Ok(())
}

#[test]
fn user_runtime_shadowing_platform_fails() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml().replace(
        "phoxal_runtimes:\n  version: \"latest\"",
        "phoxal_runtimes:\n  version: \"latest\"\n\nuser_runtimes:\n  drive:\n    path: ./runtimes/drive",
    ))?;

    let snapshot = releases_snapshot();
    let error = resolve_with_releases(&robot, &CATALOG, offline_options(), &snapshot)
        .expect_err("shadowing should fail");
    assert!(error.to_string().contains("shadows a platform runtime"));

    Ok(())
}

#[test]
fn missing_instance_source_fails() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml().replace(
        "  sources:\n    ddsm115:\n      path: ./components/ddsm115",
        "  sources: {}",
    ))?;

    let snapshot = releases_snapshot();
    let error = resolve_with_releases(&robot, &CATALOG, offline_options(), &snapshot)
        .expect_err("missing source should fail");
    assert!(error.to_string().contains("references missing source"));

    Ok(())
}

#[test]
fn webots_tools_resolve_from_framework_on_the_runtime_train() -> anyhow::Result<()> {
    // #41: the Webots controller/supervisor ship from phoxal/framework in the
    // same release as the runtimes, so they must resolve to the runtime-set
    // version (0.8.0 here), not a hardcoded tool pin.
    let robot = Robot::parse_from_string(&minimal_robot_yaml())?;
    let snapshot = releases_snapshot();
    let resolved = resolve_with_releases(&robot, &CATALOG, offline_options(), &snapshot)?;

    for tool_name in ["simulator_webots_controller", "simulator_webots_supervisor"] {
        let tool = resolved
            .tools
            .iter()
            .find(|tool| tool.name == tool_name)
            .unwrap_or_else(|| panic!("{tool_name} resolved"));
        assert_eq!(tool.repo, "phoxal/framework", "{tool_name} repo");
        assert_eq!(tool.resolved, "0.8.0", "{tool_name} version tracks the train");
        assert!(
            tool.asset.starts_with("phoxal-simulator-0.8.0-"),
            "{tool_name} asset {} should be version-matched",
            tool.asset
        );
    }
    // A non-train tool keeps its own pinned version line.
    let joypad = resolved
        .tools
        .iter()
        .find(|tool| tool.name == "joypad")
        .expect("joypad resolved");
    assert_eq!(joypad.repo, "phoxal/joypad");
    assert_eq!(joypad.resolved, "0.1.0");

    Ok(())
}

#[test]
fn pinning_a_runtime_train_tool_is_rejected() -> anyhow::Result<()> {
    // A per-robot version pin on a train-tracked tool would silently desync the
    // simulator binaries from the runtimes/crate.
    let robot = Robot::parse_from_string(&minimal_robot_yaml().replace(
        "phoxal_runtimes:\n  version: \"latest\"",
        "phoxal_runtimes:\n  version: \"latest\"\n\ntools:\n  simulator_webots_controller:\n    version: \"0.1.0\"",
    ))?;
    let snapshot = releases_snapshot();
    let error = resolve_with_releases(&robot, &CATALOG, offline_options(), &snapshot)
        .expect_err("pinning a train tool should fail");
    assert!(
        error.to_string().contains("tracks the runtime version train"),
        "unexpected error: {error}"
    );

    Ok(())
}

#[test]
fn pinned_tool_version_override_is_preserved() -> anyhow::Result<()> {
    // Non-train tools may still be pinned per robot.
    let robot = Robot::parse_from_string(&minimal_robot_yaml().replace(
        "phoxal_runtimes:\n  version: \"latest\"",
        "phoxal_runtimes:\n  version: \"latest\"\n\ntools:\n  joypad:\n    version: \"0.9.9\"",
    ))?;
    let snapshot = releases_snapshot();
    let resolved = resolve_with_releases(&robot, &CATALOG, offline_options(), &snapshot)?;
    let joypad = resolved
        .tools
        .iter()
        .find(|tool| tool.name == "joypad")
        .expect("joypad resolved");
    assert_eq!(joypad.resolved, "0.9.9");

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

fn releases_snapshot() -> ReleasesSnapshot {
    ReleasesSnapshot {
        fetched_at: std::time::SystemTime::UNIX_EPOCH,
        versions: vec!["0.8.0".into()],
    }
}
