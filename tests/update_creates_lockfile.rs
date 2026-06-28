use std::fs;

use phoxal_cli::catalog::{CATALOG, DEFAULT_TOOL_VERSIONS};
use phoxal_cli::commands::update::{UpdateOptions, run};
use phoxal_cli::lockfile::{LOCKFILE_NAME, LOCKFILE_SCHEMA_VERSION, Lockfile};

#[test]
fn update_creates_idempotent_lockfile() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    write_robot_project(temp.path())?;

    let summary = run(
        temp.path(),
        UpdateOptions {
            resolve_external_artifacts: false,
        },
    )?;
    let lock_path = temp.path().join(LOCKFILE_NAME);
    assert_eq!(summary.lockfile_path, lock_path);
    assert!(lock_path.is_file());

    let lockfile = Lockfile::read(&lock_path)?;
    assert_eq!(lockfile.schema_version, LOCKFILE_SCHEMA_VERSION);
    assert_eq!(lockfile.phoxal_runtimes.api_version, "y2026_1");
    assert_eq!(lockfile.phoxal_runtimes.channel, "stable");
    assert_eq!(
        lockfile.phoxal_runtimes.images.len(),
        CATALOG.names_for_api("y2026_1").len()
    );
    // Offline `update` must not write fake digests: every image entry is a
    // tag ref with no fabricated digest pin.
    assert!(
        lockfile
            .phoxal_runtimes
            .images
            .values()
            .all(|image| image.digest.is_none() && image.image_ref.ends_with(":y2026_1-stable"))
    );
    assert!(lockfile.components.is_empty());
    assert_eq!(lockfile.tools.len(), DEFAULT_TOOL_VERSIONS.len());

    let first = fs::read_to_string(&lock_path)?;
    run(
        temp.path(),
        UpdateOptions {
            resolve_external_artifacts: false,
        },
    )?;
    let second = fs::read_to_string(&lock_path)?;
    assert_eq!(second, first);

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
    r#"schema: v0
api_version: y2026_1

identity:
  id: testbot
  namespace: test

structure: structure.urdf

phoxal_runtimes:
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
