use std::fs;

use phoxal::model::robot::RobotV1 as Robot;
use phoxal_cli::catalog::CATALOG;
use phoxal_cli::resolver::{ResolveOptions, resolve};

#[test]
fn resolves_minimal_robot_to_api_channel_platform_set() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml("y2026_1"))?;
    let resolved = resolve(
        &robot,
        std::path::Path::new("."),
        &CATALOG,
        offline_options(),
    )?;

    assert_eq!(resolved.api_version, "y2026_1");
    assert_eq!(resolved.channel.to_string(), "stable");
    assert_eq!(
        resolved
            .platform_runtimes
            .iter()
            .map(|runtime| runtime.name.as_str())
            .collect::<Vec<_>>(),
        vec![
            "asset",
            "battery",
            "drive",
            "explore",
            "follow",
            "frame",
            "joint",
            "localize",
            "map",
            "mission",
            "motion",
            "odometry",
            "perception",
            "plan",
            "power",
            "presence",
            "safety",
            "video"
        ]
    );
    assert!(resolved.platform_runtimes.iter().all(|runtime| {
        runtime.digest_pin().is_none()
            && runtime
                .deploy_ref()
                .ends_with(&format!(":y2026_1-{}", resolved.channel))
    }));
    assert_eq!(
        resolved
            .platform_runtimes
            .iter()
            .find(|runtime| runtime.name == "drive")
            .expect("drive runtime")
            .tag_ref(),
        "ghcr.io/phoxal/service-drive:y2026_1-stable"
    );

    Ok(())
}

#[test]
fn resolves_known_api_to_its_official_set() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml("y2026_1"))?;
    let resolved = resolve(
        &robot,
        std::path::Path::new("."),
        &CATALOG,
        offline_options(),
    )?;

    assert_eq!(
        resolved
            .platform_runtimes
            .iter()
            .map(|runtime| runtime.name.as_str())
            .collect::<Vec<_>>(),
        vec![
            "asset",
            "battery",
            "drive",
            "explore",
            "follow",
            "frame",
            "joint",
            "localize",
            "map",
            "mission",
            "motion",
            "odometry",
            "perception",
            "plan",
            "power",
            "presence",
            "safety",
            "video"
        ]
    );
    assert!(
        resolved
            .platform_runtimes
            .iter()
            .all(|runtime| runtime.tag_ref().ends_with(":y2026_1-stable"))
    );

    Ok(())
}

#[test]
fn image_override_for_official_runtime_is_preserved() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml("y2026_1").replace(
        "phoxal_participants:\n  channel: stable",
        "phoxal_participants:\n  channel: latest\n  images:\n    drive: ghcr.io/phoxal/service-drive:y2026_1-v0.13.0",
    ))?;
    let resolved = resolve(
        &robot,
        std::path::Path::new("."),
        &CATALOG,
        offline_options(),
    )?;

    let drive = resolved
        .platform_runtimes
        .iter()
        .find(|runtime| runtime.name == "drive")
        .expect("drive runtime");
    assert_eq!(
        drive.tag_ref(),
        "ghcr.io/phoxal/service-drive:y2026_1-v0.13.0"
    );
    let map = resolved
        .platform_runtimes
        .iter()
        .find(|runtime| runtime.name == "map")
        .expect("map runtime");
    assert_eq!(map.tag_ref(), "ghcr.io/phoxal/service-map:y2026_1-latest");

    Ok(())
}

#[test]
fn unknown_platform_image_key_fails_for_api() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml("y2026_1").replace(
        "phoxal_participants:\n  channel: stable",
        "phoxal_participants:\n  channel: stable\n  images:\n    diagnostics: ghcr.io/phoxal/service-diagnostics:y2026_1-stable",
    ))?;

    let error = resolve(
        &robot,
        std::path::Path::new("."),
        &CATALOG,
        offline_options(),
    )
    .expect_err("image key outside the API set should fail");
    assert!(error.to_string().contains("not a platform participant"));

    Ok(())
}

#[test]
fn user_runtime_shadowing_platform_fails_for_api() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml("y2026_1").replace(
        "phoxal_participants:\n  channel: stable",
        "phoxal_participants:\n  channel: stable\n\nuser_participants:\n  drive:\n    path: ./runtimes/drive",
    ))?;

    let error = resolve(
        &robot,
        std::path::Path::new("."),
        &CATALOG,
        offline_options(),
    )
    .expect_err("shadowing should fail");
    assert!(error.to_string().contains("shadows a platform participant"));

    Ok(())
}

#[test]
fn user_runtime_match_platform_resolves_to_api_version_hash_and_image() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    write_runtime_source(temp.path(), "runtimes/autonomy", "FROM scratch\n")?;
    let robot = Robot::parse_from_string(&robot_with_user_runtime(
        r#"
  autonomy:
    path: runtimes/autonomy
"#,
    ))?;

    let first = resolve(&robot, temp.path(), &CATALOG, offline_options())?;
    let second = resolve(&robot, temp.path(), &CATALOG, offline_options())?;

    let runtime = first
        .user_runtimes
        .iter()
        .find(|runtime| runtime.name == "autonomy")
        .expect("user service resolved");
    assert_eq!(runtime.path, std::path::PathBuf::from("runtimes/autonomy"));
    assert_eq!(runtime.framework, "y2026_1");
    assert_eq!(runtime.source_hash.len(), 16);
    assert_eq!(
        runtime.image,
        "phoxal.local/testbot/user-service/autonomy:dev"
    );
    assert_eq!(second.user_runtimes[0].source_hash, runtime.source_hash);

    Ok(())
}

#[test]
fn user_runtime_explicit_matching_framework_is_preserved() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    write_runtime_source(temp.path(), "runtimes/autonomy", "FROM scratch\n")?;
    let robot = Robot::parse_from_string(&robot_with_user_runtime(
        r#"
  autonomy:
    path: runtimes/autonomy
    framework: y2026_1
"#,
    ))?;

    let resolved = resolve(&robot, temp.path(), &CATALOG, offline_options())?;

    assert_eq!(resolved.user_runtimes[0].framework, "y2026_1");
    Ok(())
}

#[test]
fn user_runtime_explicit_mismatched_framework_fails() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    write_runtime_source(temp.path(), "runtimes/autonomy", "FROM scratch\n")?;
    let robot = Robot::parse_from_string(&robot_with_user_runtime(
        r#"
  autonomy:
    path: runtimes/autonomy
    framework: y2026_2
"#,
    ))?;

    let error =
        resolve(&robot, temp.path(), &CATALOG, offline_options()).expect_err("stale framework");

    assert_eq!(
        error.to_string(),
        "user service 'autonomy': framework 'y2026_2' must be \"match-platform\" or the graph api_version 'y2026_1'"
    );
    Ok(())
}

#[test]
fn missing_instance_source_fails() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml("y2026_1").replace(
        "  sources:\n    ddsm115:\n      path: ./components/ddsm115",
        "  sources: {}",
    ))?;

    let error = resolve(
        &robot,
        std::path::Path::new("."),
        &CATALOG,
        offline_options(),
    )
    .expect_err("missing source should fail");
    assert!(error.to_string().contains("references missing source"));

    Ok(())
}

#[test]
fn tools_resolve_from_explicit_independent_versions() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml("y2026_1"))?;
    let resolved = resolve(
        &robot,
        std::path::Path::new("."),
        &CATALOG,
        offline_options(),
    )?;

    for tool_name in [
        "simulator_webots_controller",
        "simulator_webots_supervisor",
        "joypad",
    ] {
        let tool = resolved
            .tools
            .iter()
            .find(|tool| tool.name == tool_name)
            .unwrap_or_else(|| panic!("{tool_name} resolved"));
        assert_eq!(tool.repo, "phoxal/framework", "{tool_name} repo");
        assert_eq!(tool.resolved, "0.14.0", "{tool_name} version");
    }

    Ok(())
}

#[test]
fn tool_version_override_is_preserved_for_any_known_tool() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml("y2026_1").replace(
        "phoxal_participants:\n  channel: stable",
        "phoxal_participants:\n  channel: stable\n\ntools:\n  joypad:\n    version: \"0.9.9\"",
    ))?;
    let resolved = resolve(
        &robot,
        std::path::Path::new("."),
        &CATALOG,
        offline_options(),
    )?;
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
        resolve_source_commits: false,
    }
}

fn minimal_robot_yaml(api_version: &str) -> String {
    format!(
        r#"schema: v0
api_version: {api_version}

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
    )
}

fn robot_with_user_runtime(user_runtimes: &str) -> String {
    minimal_robot_yaml("y2026_1").replace(
        "phoxal_participants:\n  channel: stable",
        &format!("phoxal_participants:\n  channel: stable\n\nuser_participants:\n{user_runtimes}"),
    )
}

fn write_runtime_source(root: &std::path::Path, path: &str, contents: &str) -> anyhow::Result<()> {
    let dir = root.join(path);
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("Dockerfile"), contents)?;
    Ok(())
}
