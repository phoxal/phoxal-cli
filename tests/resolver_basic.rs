use phoxal::model::robot::RobotV1 as Robot;
use phoxal_cli::catalog::CATALOG;
use phoxal_cli::releases::ReleasesSnapshot;
use phoxal_cli::resolver::{ResolveOptions, resolve_with_releases};

#[test]
fn resolves_minimal_robot_to_full_platform_set() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml())?;
    let snapshot = releases_snapshot();
    let resolved = resolve_with_releases(&robot, &CATALOG, offline_options(), &snapshot)?;

    assert_eq!(resolved.runtime_set_version.to_string(), "0.7.0");
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
        !deploy.contains('@') && deploy.ends_with(":0.7.0")
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
        versions: vec!["0.7.0".into()],
    }
}
