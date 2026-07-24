use phoxal::model::robot::RobotV0 as Robot;
use phoxal_cli::resolver::{host_target_triple, resolve};
use phoxal_cli_core::project::resolver::{ResolveOptions, ResolvedComponentSource, ResolvedRobot};
use phoxal_cli_core::project::suite::{
    Suite, fixture_artifact_for_tests, fixture_component_assets_entry_for_tests,
    fixture_component_driver_entry_for_tests, fixture_contract_for_tests,
    fixture_service_entry_for_tests, fixture_simulator_entry_for_tests, fixture_suite_for_tests,
    fixture_tool_entry_for_tests,
};

#[test]
fn resolves_minimal_robot_to_train_platform_set() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml())?;
    let resolved = resolve_with_suite(&robot, std::path::Path::new("."))?;

    assert_eq!(resolved.train, "0.1.0");
    assert_eq!(
        resolved
            .platform_runtimes
            .iter()
            .map(|runtime| runtime.name.as_str())
            .collect::<Vec<_>>(),
        vec![
            "asset",
            "behavior",
            "drive",
            "frame",
            "joint",
            "localize",
            "map",
            "motion",
            "navigation",
            "odometry",
            "perception",
            "power",
            "safety",
            "video"
        ]
    );
    assert!(
        resolved
            .platform_runtimes
            .iter()
            .all(|runtime| runtime.package.starts_with("phoxal/service-"))
    );
    assert_eq!(
        resolved
            .platform_runtimes
            .iter()
            .find(|runtime| runtime.name == "drive")
            .expect("drive runtime")
            .artifact_ref(),
        format!(
            "https://example.invalid/phoxal/service-drive/{}",
            host_target_triple()
        )
    );

    Ok(())
}

#[test]
fn suite_component_drivers_do_not_enter_platform_service_set() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml())?;
    let resolved = resolve_with_suite(&robot, std::path::Path::new("."))?;

    assert!(
        resolved
            .platform_runtimes
            .iter()
            .all(|runtime| runtime.kind == phoxal_cli_core::project::suite::ArtifactKind::Service)
    );
    assert!(
        !resolved
            .platform_runtimes
            .iter()
            .any(|runtime| runtime.name == "ddsm115" || runtime.name == "bno085")
    );

    Ok(())
}

#[test]
fn driverless_component_resolves_assets_only() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml())?;
    let resolved = resolve_with_suite(&robot, std::path::Path::new("."))?;

    let left_drive = resolved
        .components
        .iter()
        .find(|component| component.instance == "left_drive")
        .expect("left_drive component resolved");
    assert!(!left_drive.has_driver);
    assert!(left_drive.driver.is_none());
    assert_eq!(left_drive.source_name, "ddsm115");
    let assets = left_drive
        .assets
        .as_ref()
        .expect("ddsm115 assets package resolves from the suite");
    assert_eq!(assets.package, "phoxal/component-ddsm115");
    assert_eq!(
        assets.source,
        phoxal_cli_core::project::resolver::ResolvedComponentSource::Suite
    );

    Ok(())
}

#[test]
fn component_with_driver_block_resolves_both_assets_and_driver() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml().replace(
        "    left_drive:\n      component: ddsm115\n      mount_link: left_wheel_mount",
        "    left_drive:\n      component: ddsm115\n      mount_link: left_wheel_mount\n      driver:\n        connection: { type: can, bus: 0, node_id: 1 }",
    ))?;
    let resolved = resolve_with_suite(&robot, std::path::Path::new("."))?;

    let left_drive = resolved
        .components
        .iter()
        .find(|component| component.instance == "left_drive")
        .expect("left_drive component resolved");
    assert!(left_drive.has_driver);
    assert_eq!(left_drive.source_name, "ddsm115");
    assert_eq!(
        left_drive
            .assets
            .as_ref()
            .expect("ddsm115 assets package resolves from the suite")
            .package,
        "phoxal/component-ddsm115"
    );
    let driver = left_drive.driver.as_ref().expect("driver package resolved");
    assert_eq!(driver.package, "phoxal/component-ddsm115");

    Ok(())
}

#[test]
fn component_version_pin_is_rejected_because_the_train_owns_versions() -> anyhow::Result<()> {
    let yaml = format!(
        "{}\nartifacts:\n  pins:\n    phoxal/component-ddsm115: v0.2.0\n",
        minimal_robot_yaml()
    );
    Robot::parse_from_string(&yaml).expect_err("the removed artifacts section must be rejected");
    Ok(())
}

#[test]
fn suite_component_captures_the_release_asset_for_assets_and_driver() -> anyhow::Result<()> {
    // The resolver must capture, for a Suite-sourced component package,
    // exactly the same shape a service captures: the resolved suite
    // entry's version, the per-scope `ReleaseAsset`, and the resolved target
    // scope (assets for metadata, the target triple for drivers).
    let robot = Robot::parse_from_string(&minimal_robot_yaml().replace(
        "    left_drive:\n      component: ddsm115\n      mount_link: left_wheel_mount",
        "    left_drive:\n      component: ddsm115\n      mount_link: left_wheel_mount\n      driver:\n        connection: { type: can, bus: 0, node_id: 1 }",
    ))?;
    let target = host_target_triple();
    let mut assets_entry = fixture_component_assets_entry_for_tests("ddsm115", "0.1.0");
    assets_entry.as_asset_entry_mut().assets = Some(fixture_artifact_for_tests(
        "phoxal-component-ddsm115-assets-v0.1.0.tar.zst",
        &"a".repeat(64),
    ));
    let mut driver_entry =
        fixture_component_driver_entry_for_tests("ddsm115", "0.1.0", &target, false, Vec::new());
    driver_entry.as_artifact_entry_mut().targets.insert(
        target.clone(),
        fixture_artifact_for_tests(
            &format!("phoxal-component-ddsm115-driver-v0.1.0-{target}.tar.zst"),
            &"b".repeat(64),
        ),
    );
    let suite = fixture_suite_for_tests(vec![
        fixture_service_entry_for_tests(
            "drive",
            "0.1.0",
            &target,
            true,
            vec![fixture_contract_for_tests("v0.1::drive::Target", "publish")],
        ),
        assets_entry,
        driver_entry,
    ]);
    let project = locked_project_root()?;

    let resolved = resolve(&robot, project.path(), Some(&suite), offline_options())?;
    let left_drive = resolved
        .components
        .iter()
        .find(|component| component.instance == "left_drive")
        .expect("left_drive component resolved");

    let assets = left_drive.assets.as_ref().expect("assets package resolved");
    assert_eq!(assets.source, ResolvedComponentSource::Suite);
    let assets_runtime = assets
        .suite_runtime
        .as_ref()
        .expect("suite-sourced assets package captures a suite_runtime");
    assert_eq!(assets_runtime.name, "ddsm115");
    assert_eq!(assets_runtime.version, "0.1.0");
    assert_eq!(
        assets_runtime.sha256.as_deref(),
        Some("a".repeat(64)).as_deref()
    );
    assert_eq!(
        assets_runtime.artifact_ref(),
        "https://example.invalid/phoxal-component-ddsm115-assets-v0.1.0.tar.zst"
    );

    let driver = left_drive.driver.as_ref().expect("driver package resolved");
    assert_eq!(driver.source, ResolvedComponentSource::Suite);
    let driver_runtime = driver
        .suite_runtime
        .as_ref()
        .expect("suite-sourced driver package captures a suite_runtime");
    assert_eq!(driver_runtime.name, "ddsm115");
    assert_eq!(
        driver_runtime.sha256.as_deref(),
        Some("b".repeat(64)).as_deref()
    );
    assert_eq!(
        driver_runtime.artifact_ref(),
        format!("https://example.invalid/phoxal-component-ddsm115-driver-v0.1.0-{target}.tar.zst")
    );

    Ok(())
}

#[test]
fn suite_component_with_no_release_asset_yet_still_resolves_with_none_runtime_sha256()
-> anyhow::Result<()> {
    // A metadata-only / not-yet-published suite entry must not silently
    // succeed as if a bundle exists to fetch: resolution succeeds (the
    // package is real and versioned), but `suite_runtime.sha256` stays
    // `None` so a later staging attempt reports a clear diagnostic.
    let robot = Robot::parse_from_string(&minimal_robot_yaml())?;
    let mut component = fixture_component_assets_entry_for_tests("ddsm115", "0.1.0");
    component.as_asset_entry_mut().assets = None;
    let suite = fixture_suite_for_tests(vec![component]);
    let project = locked_project_root()?;
    let resolved = resolve(&robot, project.path(), Some(&suite), offline_options())?;

    let left_drive = resolved
        .components
        .iter()
        .find(|component| component.instance == "left_drive")
        .expect("left_drive component resolved");
    let runtime = left_drive
        .assets
        .as_ref()
        .expect("assets package resolved (the suite entry exists, just unpublished)")
        .suite_runtime
        .as_ref()
        .expect("suite_runtime is populated even with no release asset yet");
    assert!(runtime.sha256.is_none());
    assert!(!runtime.published);

    Ok(())
}

#[test]
fn declared_driver_with_no_target_blob_is_unavailable() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml().replace(
        "    left_drive:\n      component: ddsm115\n      mount_link: left_wheel_mount",
        "    left_drive:\n      component: ddsm115\n      mount_link: left_wheel_mount\n      driver:\n        connection: { type: can, bus: 0, node_id: 1 }",
    ))?;
    let suite = fixture_suite_for_tests(vec![
        fixture_service_entry_for_tests(
            "drive",
            "0.1.0",
            &host_target_triple(),
            true,
            vec![fixture_contract_for_tests("v0.1::drive::Target", "publish")],
        ),
        fixture_component_assets_entry_for_tests("ddsm115", "0.1.0"),
    ]);
    let project = locked_project_root()?;

    let error = resolve(&robot, project.path(), Some(&suite), offline_options())
        .expect_err("a declared driver must have an artifact for the target");
    assert!(
        format!("{error:#}").contains("ComponentDriverUnavailable"),
        "{error:#}"
    );

    Ok(())
}

#[test]
fn resolves_known_api_to_its_official_set() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml())?;
    let resolved = resolve_with_suite(&robot, std::path::Path::new("."))?;

    assert_eq!(
        resolved
            .platform_runtimes
            .iter()
            .map(|runtime| runtime.name.as_str())
            .collect::<Vec<_>>(),
        vec![
            "asset",
            "behavior",
            "drive",
            "frame",
            "joint",
            "localize",
            "map",
            "motion",
            "navigation",
            "odometry",
            "perception",
            "power",
            "safety",
            "video"
        ]
    );
    assert!(
        resolved
            .platform_runtimes
            .iter()
            .all(|runtime| runtime.package.starts_with("phoxal/service-"))
    );

    Ok(())
}

#[test]
fn tools_resolve_from_suite_entries() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml())?;
    let resolved = resolve_with_suite(&robot, std::path::Path::new("."))?;

    for (tool_name, package) in [
        ("tool-bus", "phoxal/tool-bus"),
        ("tool-joypad", "phoxal/tool-joypad"),
        ("tool-log", "phoxal/tool-log"),
        ("tool-telemetry", "phoxal/tool-telemetry"),
        ("infrastructure-router", "phoxal/infrastructure-router"),
    ] {
        let tool = resolved
            .tools
            .iter()
            .find(|tool| tool.name == tool_name)
            .unwrap_or_else(|| panic!("{tool_name} resolved"));
        assert_eq!(tool.package, package);
        assert_eq!(tool.repo, "phoxal/framework", "{tool_name} repo");
        assert_eq!(tool.resolved, "0.1.0", "{tool_name} version");
    }
    assert_eq!(
        resolved
            .simulators
            .iter()
            .map(|simulator| simulator.package.as_str())
            .collect::<Vec<_>>(),
        vec![
            "phoxal/simulator-webots-controller",
            "phoxal/simulator-webots-supervisor"
        ]
    );

    Ok(())
}

fn resolve_with_suite(robot: &Robot, _root: &std::path::Path) -> anyhow::Result<ResolvedRobot> {
    let suite = test_suite();
    let project = locked_project_root()?;
    resolve(robot, project.path(), Some(&suite), offline_options())
}

fn locked_project_root() -> anyhow::Result<tempfile::TempDir> {
    let root = tempfile::tempdir()?;
    fs::create_dir_all(root.path().join("src"))?;
    fs::create_dir_all(root.path().join("train/phoxal/src"))?;
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"robot\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n[dependencies]\nphoxal = { path = \"train/phoxal\" }\n",
    )?;
    fs::write(root.path().join("src/lib.rs"), "")?;
    fs::write(
        root.path().join("train/phoxal/Cargo.toml"),
        "[package]\nname = \"phoxal\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    fs::write(root.path().join("train/phoxal/src/lib.rs"), "")?;
    anyhow::ensure!(
        std::process::Command::new("cargo")
            .arg("generate-lockfile")
            .current_dir(root.path())
            .status()?
            .success(),
        "failed to generate fixture Cargo.lock"
    );
    Ok(root)
}

fn test_suite() -> Suite {
    let target = host_target_triple();
    let mut entries = service_names()
        .into_iter()
        .map(|name| {
            fixture_service_entry_for_tests(
                name,
                "0.1.0",
                &target,
                true,
                vec![fixture_contract_for_tests("v0.1::drive::Target", "publish")],
            )
        })
        .collect::<Vec<_>>();
    for name in component_names() {
        entries.push(fixture_component_assets_entry_for_tests(name, "0.1.0"));
        entries.push(fixture_component_driver_entry_for_tests(
            name,
            "0.1.0",
            &target,
            true,
            vec![fixture_contract_for_tests(
                "v0.1::component::State",
                "publish",
            )],
        ));
    }
    entries.extend([
        fixture_tool_entry_for_tests("bus", "0.1.0", &target, true, Vec::new()),
        fixture_tool_entry_for_tests("router", "0.1.0", &target, true, Vec::new()),
        fixture_tool_entry_for_tests(
            "joypad",
            "0.1.0",
            &target,
            true,
            vec![fixture_contract_for_tests(
                "v0.1::drive::Target",
                "subscribe",
            )],
        ),
        fixture_tool_entry_for_tests("log", "0.1.0", &target, true, Vec::new()),
        fixture_tool_entry_for_tests("telemetry", "0.1.0", &target, true, Vec::new()),
        fixture_simulator_entry_for_tests(
            "webots-controller",
            "0.14.0",
            &target,
            true,
            vec![fixture_contract_for_tests(
                "v0.1::component::MotorCommand",
                "publish",
            )],
        ),
        fixture_simulator_entry_for_tests("webots-supervisor", "0.14.0", &target, true, Vec::new()),
    ]);
    fixture_suite_for_tests(entries)
}

fn service_names() -> Vec<&'static str> {
    vec![
        "asset",
        "behavior",
        "drive",
        "frame",
        "joint",
        "localize",
        "map",
        "motion",
        "navigation",
        "odometry",
        "perception",
        "power",
        "safety",
        "video",
    ]
}

fn component_names() -> Vec<&'static str> {
    vec!["ddsm115", "bno085"]
}

fn offline_options() -> ResolveOptions {
    ResolveOptions {
        ..ResolveOptions::default()
    }
}

fn minimal_robot_yaml() -> String {
    r#"schema: robot/v0
robot:
  id: testbot
  namespace: test
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  structure: structure.urdf
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
  components:
    left_drive:
      component: ddsm115
      mount_link: left_wheel_mount
    right_drive:
      component: ddsm115
      mount_link: right_wheel_mount
"#
    .to_string()
}
use std::fs;
