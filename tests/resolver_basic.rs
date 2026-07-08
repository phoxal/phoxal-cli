use std::fs;

use phoxal::model::robot::RobotV0 as Robot;
use phoxal_cli::catalog::{
    CatalogRevision, Channel as CatalogChannel, fixture_artifact_for_tests,
    fixture_catalog_for_tests, fixture_component_assets_entry_for_tests,
    fixture_component_driver_entry_for_tests, fixture_contract_for_tests,
    fixture_service_entry_for_tests, fixture_simulator_entry_for_tests,
    fixture_tool_entry_for_tests,
};
use phoxal_cli::resolver::{
    ResolveOptions, ResolvedComponentSource, ResolvedPathOverrideKind, ResolvedRobot,
    host_target_triple, load_robot_with_extras, load_robot_with_extras_and_overlays, resolve,
};

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
            "phoxal-service-drive:0.1.0-y2026_1-stable-{}",
            host_target_triple()
        )
    );

    Ok(())
}

#[test]
fn catalog_component_drivers_do_not_enter_platform_service_set() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml("y2026_1"))?;
    let resolved = resolve_with_catalog(&robot, std::path::Path::new("."))?;

    assert!(
        resolved
            .platform_runtimes
            .iter()
            .all(|runtime| runtime.kind == phoxal_cli::catalog::ArtifactKind::Service)
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
    let robot = Robot::parse_from_string(&minimal_robot_yaml("y2026_1"))?;
    let resolved = resolve_with_catalog(&robot, std::path::Path::new("."))?;

    let left_drive = resolved
        .components
        .iter()
        .find(|component| component.instance == "left_drive")
        .expect("left_drive component resolved");
    assert!(!left_drive.has_driver);
    assert!(left_drive.driver.is_none());
    assert_eq!(left_drive.source_name, "ddsm115");
    assert_eq!(left_drive.assets.package, "phoxal/component-ddsm115-assets");
    assert_eq!(
        left_drive.assets.source,
        phoxal_cli::resolver::ResolvedComponentSource::Catalog
    );

    Ok(())
}

#[test]
fn component_with_driver_block_resolves_both_assets_and_driver() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml("y2026_1").replace(
        "    left_drive:\n      component: ddsm115\n      mount_link: left_wheel_mount",
        "    left_drive:\n      component: ddsm115\n      mount_link: left_wheel_mount\n      driver:\n        connection: { type: can, bus: 0, node_id: 1 }",
    ))?;
    let resolved = resolve_with_catalog(&robot, std::path::Path::new("."))?;

    let left_drive = resolved
        .components
        .iter()
        .find(|component| component.instance == "left_drive")
        .expect("left_drive component resolved");
    assert!(left_drive.has_driver);
    assert_eq!(left_drive.source_name, "ddsm115");
    assert_eq!(left_drive.assets.package, "phoxal/component-ddsm115-assets");
    let driver = left_drive.driver.as_ref().expect("driver package resolved");
    assert_eq!(driver.package, "phoxal/component-ddsm115-driver");

    Ok(())
}

#[test]
fn catalog_component_captures_the_release_asset_for_assets_and_driver() -> anyhow::Result<()> {
    // The resolver must capture, for a Catalog-sourced component package,
    // exactly the same shape a service captures: the resolved catalog
    // entry's version, the per-scope `ReleaseAsset`, and the resolved target
    // scope (target-independent for assets, the target triple for drivers).
    let robot = Robot::parse_from_string(&minimal_robot_yaml("y2026_1").replace(
        "    left_drive:\n      component: ddsm115\n      mount_link: left_wheel_mount",
        "    left_drive:\n      component: ddsm115\n      mount_link: left_wheel_mount\n      driver:\n        connection: { type: can, bus: 0, node_id: 1 }",
    ))?;
    let target = host_target_triple();
    let mut assets_entry = fixture_component_assets_entry_for_tests(
        "ddsm115",
        "y2026_1",
        "0.1.0",
        CatalogChannel::Stable,
    );
    assets_entry.as_asset_entry_mut().artifacts.insert(
        phoxal_cli::catalog::TARGET_INDEPENDENT_SCOPE.to_string(),
        fixture_artifact_for_tests(
            "phoxal-component-ddsm115-assets-v0.1.0.tar.zst",
            &"a".repeat(64),
        ),
    );
    let mut driver_entry = fixture_component_driver_entry_for_tests(
        "ddsm115",
        "y2026_1",
        "0.1.0",
        CatalogChannel::Stable,
        &target,
        false,
        Vec::new(),
    );
    driver_entry.as_artifact_entry_mut().artifacts.insert(
        target.clone(),
        fixture_artifact_for_tests(
            &format!("phoxal-component-ddsm115-driver-v0.1.0-{target}.tar.zst"),
            &"b".repeat(64),
        ),
    );
    let catalog = fixture_catalog_for_tests(vec![
        fixture_service_entry_for_tests(
            "drive",
            "y2026_1",
            "0.1.0",
            CatalogChannel::Stable,
            &target,
            false,
            vec![fixture_contract_for_tests(
                "drive::Target",
                "0123456789abcdef",
            )],
        ),
        assets_entry,
        driver_entry,
    ]);

    let resolved = resolve(
        &robot,
        std::path::Path::new("."),
        Some(&catalog),
        offline_options(),
    )?;
    let left_drive = resolved
        .components
        .iter()
        .find(|component| component.instance == "left_drive")
        .expect("left_drive component resolved");

    assert_eq!(left_drive.assets.source, ResolvedComponentSource::Catalog);
    let assets_runtime = left_drive
        .assets
        .catalog_runtime
        .as_ref()
        .expect("catalog-sourced assets package captures a catalog_runtime");
    assert_eq!(assets_runtime.name, "ddsm115");
    assert_eq!(assets_runtime.version, "0.1.0");
    assert_eq!(
        assets_runtime.sha256.as_deref(),
        Some("a".repeat(64)).as_deref()
    );
    assert!(
        assets_runtime.bus_abi.is_none(),
        "component_assets carries no bus_abi/contracts (no runtime binary to describe)"
    );
    assert_eq!(
        assets_runtime.artifact_ref(),
        "phoxal-component-ddsm115-assets-v0.1.0.tar.zst"
    );

    let driver = left_drive.driver.as_ref().expect("driver package resolved");
    assert_eq!(driver.source, ResolvedComponentSource::Catalog);
    let driver_runtime = driver
        .catalog_runtime
        .as_ref()
        .expect("catalog-sourced driver package captures a catalog_runtime");
    assert_eq!(driver_runtime.name, "ddsm115");
    assert_eq!(
        driver_runtime.sha256.as_deref(),
        Some("b".repeat(64)).as_deref()
    );
    assert!(
        driver_runtime.bus_abi.is_some(),
        "component_driver carries inlined bus_abi metadata"
    );
    assert_eq!(
        driver_runtime.artifact_ref(),
        format!("phoxal-component-ddsm115-driver-v0.1.0-{target}.tar.zst")
    );

    Ok(())
}

#[test]
fn catalog_component_with_no_release_asset_yet_still_resolves_with_none_runtime_sha256()
-> anyhow::Result<()> {
    // A metadata-only / not-yet-published catalog entry must not silently
    // succeed as if a bundle exists to fetch: resolution succeeds (the
    // package is real and versioned), but `catalog_runtime.sha256` stays
    // `None` so a later staging attempt reports a clear diagnostic.
    let robot = Robot::parse_from_string(&minimal_robot_yaml("y2026_1"))?;
    let resolved = resolve_with_catalog(&robot, std::path::Path::new("."))?;

    let left_drive = resolved
        .components
        .iter()
        .find(|component| component.instance == "left_drive")
        .expect("left_drive component resolved");
    let runtime = left_drive
        .assets
        .catalog_runtime
        .as_ref()
        .expect("catalog_runtime is populated even with no release asset yet");
    assert!(runtime.sha256.is_none());
    assert!(!runtime.published);

    Ok(())
}

#[test]
fn declared_driver_with_no_catalog_driver_package_is_a_named_diagnostic() -> anyhow::Result<()> {
    // Only the assets package is in this catalog; the instance declares
    // `driver:` but no matching `phoxal/component-ddsm115-driver` package
    // exists in the resolved graph.
    let robot = Robot::parse_from_string(&minimal_robot_yaml("y2026_1").replace(
        "    left_drive:\n      component: ddsm115\n      mount_link: left_wheel_mount",
        "    left_drive:\n      component: ddsm115\n      mount_link: left_wheel_mount\n      driver:\n        connection: { type: can, bus: 0, node_id: 1 }",
    ))?;
    let catalog = fixture_catalog_for_tests(vec![
        fixture_service_entry_for_tests(
            "drive",
            "y2026_1",
            "0.1.0",
            CatalogChannel::Stable,
            &host_target_triple(),
            false,
            vec![fixture_contract_for_tests(
                "drive::Target",
                "0123456789abcdef",
            )],
        ),
        fixture_component_assets_entry_for_tests(
            "ddsm115",
            "y2026_1",
            "0.1.0",
            CatalogChannel::Stable,
        ),
    ]);

    let error = resolve(
        &robot,
        std::path::Path::new("."),
        Some(&catalog),
        offline_options(),
    )
    .expect_err("declared driver with no resolvable driver package should fail");
    let message = error.to_string();
    assert!(message.contains("ComponentDriverUnavailable"), "{message}");
    assert!(message.contains("left_drive"), "{message}");
    assert!(message.contains("ddsm115"), "{message}");

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
fn user_service_resolves_source_hash() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    write_runtime_source(temp.path(), "runtimes/autonomy", "fn main() {}\n")?;
    let robot = Robot::parse_from_string(&robot_with_user_service(
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
    assert_eq!(runtime.source_hash.len(), 16);
    assert_eq!(second.user_runtimes[0].source_hash, runtime.source_hash);

    Ok(())
}

#[test]
fn missing_user_service_source_dir_fails() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let robot = Robot::parse_from_string(&robot_with_user_service(
        r#"
  autonomy:
    path: runtimes/autonomy
"#,
    ))?;

    let catalog = test_catalog();
    let error = resolve(&robot, temp.path(), Some(&catalog), offline_options())
        .expect_err("missing source dir should fail");

    assert!(error.to_string().contains("does not exist"), "{error:#}");
    Ok(())
}

#[test]
fn tools_resolve_from_catalog_entries() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml("y2026_1"))?;
    let resolved = resolve_with_catalog(&robot, std::path::Path::new("."))?;

    for tool_name in ["tool-router", "tool-joypad"] {
        let tool = resolved
            .tools
            .iter()
            .find(|tool| tool.name == tool_name)
            .unwrap_or_else(|| panic!("{tool_name} resolved"));
        assert_eq!(tool.package, format!("phoxal/{tool_name}"));
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

#[test]
fn path_pin_with_unqualified_key_is_rejected() -> anyhow::Result<()> {
    // Pin keys must be provider-qualified `<provider>/<name>` package ids; an
    // unqualified key is rejected during resolution/validation.
    let robot = Robot::parse_from_string(&robot_with_path_pin(
        "runtime-drive",
        "./framework/service/drive",
    ))?;
    let error = resolve_with_catalog(&robot, std::path::Path::new("."))
        .expect_err("unqualified path pin key should fail");

    assert!(
        error.to_string().contains("provider-qualified"),
        "{error:#}"
    );
    Ok(())
}

#[test]
fn unused_provider_qualified_path_pin_is_rejected() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&robot_with_path_pin(
        "phoxal/component-bno085-driver",
        "./framework/component/bno085",
    ))?;
    let error = resolve_with_catalog(&robot, std::path::Path::new("."))
        .expect_err("unused path pin key should fail");

    assert!(
        error.to_string().contains("unused artifact path pin"),
        "{error:#}"
    );
    Ok(())
}

#[test]
fn loaded_catalog_without_target_channel_gets_not_yet_available_error() -> anyhow::Result<()> {
    // The lean catalog schema has no separate "declared target triple" list
    // to mismatch against (only `artifacts`, which is legitimately empty in a
    // metadata-only catalog and must not by itself fail resolution - see
    // `catalog::newest_generation_on_channel`). What remains a real,
    // catalog-wide mismatch a robot can hit is the *channel*: a catalog that
    // only publishes `preview` has nothing for a robot pinned to `stable`.
    let robot = Robot::parse_from_string(&minimal_robot_yaml_without_generation())?;
    let target = "aarch64-unknown-linux-gnu";
    let catalog = fixture_catalog_for_tests(vec![fixture_service_entry_for_tests(
        "drive",
        "y2026_1",
        "0.1.0",
        CatalogChannel::Preview,
        target,
        true,
        vec![fixture_contract_for_tests(
            "drive::Target",
            "0123456789abcdef",
        )],
    )]);
    let error = resolve(
        &robot,
        std::path::Path::new("."),
        Some(&catalog),
        ResolveOptions {
            official_target_triple: Some(target.to_string()),
            resolve_source_commits: false,
            resolve_component_asset_commits: false,
            ..ResolveOptions::default()
        },
    )
    .expect_err("catalog with nothing on the robot's channel should fail explicitly");
    let message = error.to_string();

    assert!(message.contains("NotYetAvailable"), "{message}");
    assert!(message.contains(&catalog.revision), "{message}");
    assert!(message.contains(target), "{message}");
    assert!(message.contains("stable"), "{message}");
    assert!(
        message.contains("no released generation assets"),
        "{message}"
    );
    assert!(
        message.contains("Pass a catalog that includes released assets"),
        "{message}"
    );
    Ok(())
}

#[test]
fn official_only_robot_without_catalog_keeps_no_catalog_error() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml_without_generation())?;
    let error = resolve(
        &robot,
        std::path::Path::new("."),
        None,
        ResolveOptions {
            resolve_source_commits: false,
            resolve_component_asset_commits: false,
            ..ResolveOptions::default()
        },
    )
    .expect_err("no catalog should keep the catalog-unavailable diagnostic");
    let message = error.to_string();

    assert!(
        message.contains("no artifact catalog revision is available"),
        "{message}"
    );
    assert!(!message.contains("NotYetAvailable"), "{message}");
    Ok(())
}

#[test]
fn path_pins_are_overlay_only() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let base_path = temp.path().join("robot.yaml");
    fs::write(
        &base_path,
        robot_with_path_pin("phoxal/service-drive", "../framework/service/drive"),
    )?;
    let error = load_robot_with_extras(&base_path).expect_err("base path pin should fail");
    assert!(error.to_string().contains("dev-overlay only"), "{error:#}");

    fs::write(&base_path, minimal_robot_yaml("y2026_1"))?;
    fs::write(
        temp.path().join("robot.dev.yaml"),
        "artifacts:\n  pins:\n    phoxal/service-drive:\n      path: ../framework/service/drive\n",
    )?;
    let loaded = load_robot_with_extras_and_overlays(&base_path, &["dev".to_string()])?;
    assert!(
        loaded
            .robot
            .artifacts
            .pins
            .contains_key("phoxal/service-drive")
    );
    Ok(())
}

#[test]
fn simulator_path_pin_replaces_the_supervisor_or_controller_artifact() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let robot = Robot::parse_from_string(&robot_with_path_pin(
        "phoxal/simulator-webots-controller",
        "../framework/simulator/webots-controller",
    ))?;
    let resolved = resolve_with_catalog(&robot, temp.path())?;
    let controller = resolved
        .simulators
        .iter()
        .find(|simulator| simulator.package == "phoxal/simulator-webots-controller")
        .expect("webots-controller simulator resolved");

    assert_eq!(
        controller.source_path(),
        Some(
            temp.path()
                .join("../framework/simulator/webots-controller")
                .as_path()
        )
    );
    assert!(controller.artifact_ref().starts_with("path:"));
    assert_eq!(
        resolved
            .path_overrides
            .iter()
            .find(|override_| override_.key == "phoxal/simulator-webots-controller")
            .map(|override_| override_.kind),
        Some(ResolvedPathOverrideKind::Simulator)
    );

    // The supervisor entry is untouched: only the pinned package resolves to
    // a path override.
    let supervisor = resolved
        .simulators
        .iter()
        .find(|simulator| simulator.package == "phoxal/simulator-webots-supervisor")
        .expect("webots-supervisor simulator resolved");
    assert!(supervisor.source_path().is_none());

    Ok(())
}

#[test]
fn service_path_pin_replaces_catalog_artifact() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let robot = Robot::parse_from_string(&robot_with_path_pin(
        "phoxal/service-drive",
        "../framework/service/drive",
    ))?;
    let resolved = resolve_with_catalog(&robot, temp.path())?;
    let drive = resolved
        .platform_runtimes
        .iter()
        .find(|runtime| runtime.name == "drive")
        .expect("drive runtime");

    assert_eq!(
        drive.source_path(),
        Some(temp.path().join("../framework/service/drive").as_path())
    );
    assert!(drive.artifact_ref().starts_with("path:"));
    assert_eq!(
        resolved
            .path_overrides
            .iter()
            .find(|override_| override_.key == "phoxal/service-drive")
            .map(|override_| override_.kind),
        Some(ResolvedPathOverrideKind::Service)
    );
    Ok(())
}

#[test]
fn component_asset_path_pin_forks_the_assets_package() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let robot = Robot::parse_from_string(&robot_with_path_pin(
        "phoxal/component-ddsm115-assets",
        "../framework/component/ddsm115",
    ))?;
    let resolved = resolve_with_catalog(&robot, temp.path())?;
    let left_drive = resolved
        .components
        .iter()
        .find(|component| component.instance == "left_drive")
        .expect("left_drive component resolved");

    assert_eq!(
        left_drive.assets.path_override(),
        Some(temp.path().join("../framework/component/ddsm115").as_path())
    );
    assert_eq!(
        resolved
            .path_overrides
            .iter()
            .find(|override_| override_.key == "phoxal/component-ddsm115-assets")
            .map(|override_| override_.kind),
        Some(ResolvedPathOverrideKind::ComponentAssets)
    );
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
                false,
                vec![fixture_contract_for_tests(
                    "drive::Target",
                    "0123456789abcdef",
                )],
            )
        })
        .collect::<Vec<_>>();
    for name in component_names() {
        entries.push(fixture_component_assets_entry_for_tests(
            name,
            "y2026_1",
            "0.1.0",
            CatalogChannel::Stable,
        ));
        entries.push(fixture_component_driver_entry_for_tests(
            name,
            "y2026_1",
            "0.1.0",
            CatalogChannel::Stable,
            &target,
            false,
            vec![fixture_contract_for_tests(
                "component::State",
                "fedcba9876543210",
            )],
        ));
    }
    entries.extend([
        fixture_tool_entry_for_tests(
            "router",
            "y2026_1",
            "0.1.0",
            CatalogChannel::Stable,
            &target,
            false,
            Vec::new(),
        ),
        fixture_tool_entry_for_tests(
            "joypad",
            "y2026_1",
            "0.1.0",
            CatalogChannel::Stable,
            &target,
            false,
            vec![fixture_contract_for_tests(
                "drive::Target",
                "0123456789abcdef",
            )],
        ),
        fixture_simulator_entry_for_tests(
            "webots-supervisor",
            "y2026_1",
            "0.14.0",
            CatalogChannel::Stable,
            &target,
            false,
            Vec::new(),
        ),
        fixture_simulator_entry_for_tests(
            "webots-controller",
            "y2026_1",
            "0.14.0",
            CatalogChannel::Stable,
            &target,
            false,
            vec![fixture_contract_for_tests(
                "component::MotorCommand",
                "fedcba9876543210",
            )],
        ),
    ]);
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

fn component_names() -> Vec<&'static str> {
    vec!["ddsm115", "bno085"]
}

fn offline_options() -> ResolveOptions {
    ResolveOptions {
        resolve_source_commits: false,
        resolve_component_asset_commits: false,
        ..ResolveOptions::default()
    }
}

fn minimal_robot_yaml(generation: &str) -> String {
    format!(
        r#"schema: robot/v0
robot:
  id: testbot
  namespace: test
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
artifacts:
  channel: stable
  generation: {generation}
"#
    )
}

fn minimal_robot_yaml_without_generation() -> String {
    minimal_robot_yaml("y2026_1").replace("  generation: y2026_1\n", "")
}

fn robot_with_path_pin(key: &str, path: &str) -> String {
    minimal_robot_yaml("y2026_1").replace(
        "artifacts:\n  channel: stable",
        &format!("artifacts:\n  channel: stable\n  pins:\n    {key}:\n      path: {path}"),
    )
}

fn robot_with_user_service(services: &str) -> String {
    minimal_robot_yaml("y2026_1").replace(
        "artifacts:\n  channel: stable",
        &format!("services:\n{services}\nartifacts:\n  channel: stable"),
    )
}

fn write_runtime_source(root: &std::path::Path, path: &str, contents: &str) -> anyhow::Result<()> {
    let dir = root.join(path);
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("main.rs"), contents)?;
    Ok(())
}
