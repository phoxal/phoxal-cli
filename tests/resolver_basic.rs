use std::fs;

use phoxal::model::robot::RobotV1 as Robot;
use phoxal_cli::catalog::{
    ArtifactStatus, CatalogRevision, Channel as CatalogChannel, fixture_catalog_for_tests,
    fixture_contract_for_tests, fixture_driver_entry_for_tests, fixture_service_entry_for_tests,
};
use phoxal_cli::resolver::{ResolveOptions, ResolvedRobot, host_target_triple, resolve};

#[test]
fn resolves_minimal_robot_to_api_channel_platform_set() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml("y2026_1"))?;
    let resolved = resolve_with_catalog(&robot, std::path::Path::new("."))?;

    assert_eq!(resolved.target_generation, "y2026_1");
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
        runtime
            .artifact_ref()
            .contains(&format!("-y2026_1-{}-", resolved.channel))
    }));
    assert_eq!(
        resolved
            .platform_runtimes
            .iter()
            .find(|runtime| runtime.name == "drive")
            .expect("drive runtime")
            .artifact_ref(),
        format!(
            "service-drive:0.1.0-y2026_1-stable-{}",
            host_target_triple()
        )
    );

    Ok(())
}

#[test]
fn catalog_drivers_do_not_enter_driverless_robot_platform_set() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml("y2026_1"))?;
    let resolved = resolve_with_catalog(&robot, std::path::Path::new("."))?;

    assert!(resolved.platform_runtimes.iter().all(|runtime| {
        runtime.kind == phoxal_cli::catalog::ArtifactKind::Service
            && !runtime.artifact_id.starts_with("driver-")
    }));
    assert!(
        !resolved
            .platform_runtimes
            .iter()
            .any(|runtime| runtime.name == "ddsm115" || runtime.name == "bno085")
    );

    Ok(())
}

#[test]
fn component_driver_still_resolves_from_component_source() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml("y2026_1").replace(
        "    left_drive:\n      component: ddsm115\n      mount_link: left_wheel_mount",
        "    left_drive:\n      component: ddsm115\n      mount_link: left_wheel_mount\n      driver:\n        connection: { type: can, bus: 0, node_id: 1 }",
    ))?;
    let resolved = resolve_with_catalog(&robot, std::path::Path::new("."))?;

    assert!(
        !resolved
            .platform_runtimes
            .iter()
            .any(|runtime| runtime.artifact_id == "driver-ddsm115")
    );
    let left_drive = resolved
        .components
        .iter()
        .find(|component| component.instance == "left_drive")
        .expect("left_drive component resolved");
    assert!(left_drive.has_driver);
    assert_eq!(left_drive.source_name, "ddsm115");
    assert_eq!(
        left_drive.source,
        phoxal_cli::resolver::ResolvedComponentSource::Path {
            path: std::path::PathBuf::from("./components/ddsm115")
        }
    );

    Ok(())
}

#[test]
fn resolves_known_api_to_its_official_set() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml("y2026_1"))?;
    let resolved = resolve_with_catalog(&robot, std::path::Path::new("."))?;

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
            .all(|runtime| runtime.artifact_ref().contains("-y2026_1-stable-"))
    );

    Ok(())
}

#[test]
fn image_override_for_official_runtime_is_rejected() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml("y2026_1").replace(
        "phoxal_participants: {}",
        "phoxal_participants:\n  images:\n    drive: service-drive:y2026_1-v0.13.0",
    ))?;
    let catalog = test_catalog();

    let error = resolve(
        &robot,
        std::path::Path::new("."),
        Some(&catalog),
        offline_options(),
    )
    .expect_err("image overrides should be rejected");
    assert!(
        error
            .to_string()
            .contains("phoxal_participants.images is no longer supported")
    );

    Ok(())
}

#[test]
fn unknown_platform_image_key_fails_for_api() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml("y2026_1").replace(
        "phoxal_participants: {}",
        "phoxal_participants:\n  images:\n    diagnostics: service-diagnostics:y2026_1-stable",
    ))?;
    let catalog = test_catalog();

    let error = resolve(
        &robot,
        std::path::Path::new("."),
        Some(&catalog),
        offline_options(),
    )
    .expect_err("image key outside the API set should fail");
    assert!(error.to_string().contains("not a platform participant"));

    Ok(())
}

#[test]
fn user_runtime_shadowing_platform_fails_for_api() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml("y2026_1").replace(
        "phoxal_participants: {}",
        "phoxal_participants: {}\n\nuser_participants:\n  drive:\n    path: ./runtimes/drive",
    ))?;
    let catalog = test_catalog();

    let error = resolve(
        &robot,
        std::path::Path::new("."),
        Some(&catalog),
        offline_options(),
    )
    .expect_err("shadowing should fail");
    assert!(error.to_string().contains("shadows a platform participant"));

    Ok(())
}

#[test]
fn user_runtime_match_platform_resolves_to_api_version_and_hash() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    write_runtime_source(temp.path(), "runtimes/autonomy", "fn main() {}\n")?;
    let robot = Robot::parse_from_string(&robot_with_user_runtime(
        r#"
  autonomy:
    path: runtimes/autonomy
"#,
    ))?;

    let catalog = test_catalog();
    let first = resolve(&robot, temp.path(), Some(&catalog), offline_options())?;
    let second = resolve(&robot, temp.path(), Some(&catalog), offline_options())?;

    let runtime = first
        .user_runtimes
        .iter()
        .find(|runtime| runtime.name == "autonomy")
        .expect("user service resolved");
    assert_eq!(runtime.path, std::path::PathBuf::from("runtimes/autonomy"));
    assert_eq!(runtime.framework, "y2026_1");
    assert_eq!(runtime.source_hash.len(), 16);
    assert_eq!(second.user_runtimes[0].source_hash, runtime.source_hash);

    Ok(())
}

#[test]
fn user_runtime_explicit_matching_framework_is_preserved() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    write_runtime_source(temp.path(), "runtimes/autonomy", "fn main() {}\n")?;
    let robot = Robot::parse_from_string(&robot_with_user_runtime(
        r#"
  autonomy:
    path: runtimes/autonomy
    framework: y2026_1
"#,
    ))?;

    let catalog = test_catalog();
    let resolved = resolve(&robot, temp.path(), Some(&catalog), offline_options())?;

    assert_eq!(resolved.user_runtimes[0].framework, "y2026_1");
    Ok(())
}

#[test]
fn user_runtime_explicit_mismatched_framework_fails() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    write_runtime_source(temp.path(), "runtimes/autonomy", "fn main() {}\n")?;
    let robot = Robot::parse_from_string(&robot_with_user_runtime(
        r#"
  autonomy:
    path: runtimes/autonomy
    framework: y2026_2
"#,
    ))?;

    let catalog = test_catalog();
    let error = resolve(&robot, temp.path(), Some(&catalog), offline_options())
        .expect_err("stale framework");

    assert_eq!(
        error.to_string(),
        "user service 'autonomy': framework 'y2026_2' must be \"match-platform\" or the target generation 'y2026_1'"
    );
    Ok(())
}

#[test]
fn missing_instance_source_fails() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml("y2026_1").replace(
        "  sources:\n    ddsm115:\n      path: ./components/ddsm115",
        "  sources: {}",
    ))?;

    let error = resolve_with_catalog(&robot, std::path::Path::new("."))
        .expect_err("missing source should fail");
    assert!(error.to_string().contains("references missing source"));

    Ok(())
}

#[test]
fn tools_resolve_from_explicit_independent_versions() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml("y2026_1"))?;
    let resolved = resolve_with_catalog(&robot, std::path::Path::new("."))?;

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
        "phoxal_participants: {}",
        "phoxal_participants: {}\n\ntools:\n  joypad:\n    version: \"0.9.9\"",
    ))?;
    let resolved = resolve_with_catalog(&robot, std::path::Path::new("."))?;
    let joypad = resolved
        .tools
        .iter()
        .find(|tool| tool.name == "joypad")
        .expect("joypad resolved");
    assert_eq!(joypad.resolved, "0.9.9");

    Ok(())
}

fn resolve_with_catalog(robot: &Robot, root: &std::path::Path) -> anyhow::Result<ResolvedRobot> {
    let catalog = test_catalog();
    resolve(robot, root, Some(&catalog), offline_options())
}

fn test_catalog() -> CatalogRevision {
    let target = host_target_triple();
    let mut entries = service_names()
        .into_iter()
        .map(|name| {
            fixture_service_entry_for_tests(
                name,
                "y2026_1",
                "0.1.0",
                CatalogChannel::Stable,
                &target,
                ArtifactStatus::Pending,
                vec![fixture_contract_for_tests(
                    "drive::Target",
                    "drive/target",
                    "publish",
                    "0123456789abcdef",
                )],
            )
        })
        .collect::<Vec<_>>();
    entries.extend(driver_names().into_iter().map(|name| {
        fixture_driver_entry_for_tests(
            name,
            "y2026_1",
            "0.1.0",
            CatalogChannel::Stable,
            &target,
            ArtifactStatus::Pending,
            vec![fixture_contract_for_tests(
                "component::State",
                &format!("component/{name}/state"),
                "publish",
                "fedcba9876543210",
            )],
        )
    }));
    fixture_catalog_for_tests(entries)
}

fn service_names() -> Vec<&'static str> {
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
        "video",
    ]
}

fn driver_names() -> Vec<&'static str> {
    vec!["ddsm115", "bno085"]
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

phoxal_artifacts:
  channel: stable
phoxal_participants: {{}}

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
        "phoxal_participants: {}",
        &format!("phoxal_participants: {{}}\n\nuser_participants:\n{user_runtimes}"),
    )
}

fn write_runtime_source(root: &std::path::Path, path: &str, contents: &str) -> anyhow::Result<()> {
    let dir = root.join(path);
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("main.rs"), contents)?;
    Ok(())
}
