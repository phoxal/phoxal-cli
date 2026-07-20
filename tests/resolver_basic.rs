use std::fs;

use phoxal::model::robot::RobotV0 as Robot;
use phoxal_cli::resolver::{host_target_triple, resolve};
use phoxal_cli_core::project::catalog::{
    Catalog, SelectionChannel as CatalogChannel, fixture_artifact_for_tests,
    fixture_catalog_for_tests, fixture_component_assets_entry_for_tests,
    fixture_component_driver_entry_for_tests, fixture_contract_for_tests,
    fixture_service_entry_for_tests, fixture_simulator_entry_for_tests,
    fixture_tool_entry_for_tests,
};
use phoxal_cli_core::project::resolver::{
    ResolveOptions, ResolvedComponentSource, ResolvedPathOverrideKind, ResolvedRobot,
    load_robot_with_extras, load_robot_with_extras_and_overlays,
};

#[test]
fn resolves_minimal_robot_to_api_channel_platform_set() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml())?;
    let resolved = resolve_with_catalog(&robot, std::path::Path::new("."))?;

    assert_eq!(resolved.channel.to_string(), "stable");
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
        format!("phoxal-service-drive:0.1.0-{}", host_target_triple())
    );

    Ok(())
}

#[test]
fn catalog_component_drivers_do_not_enter_platform_service_set() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml())?;
    let resolved = resolve_with_catalog(&robot, std::path::Path::new("."))?;

    assert!(
        resolved
            .platform_runtimes
            .iter()
            .all(|runtime| runtime.kind == phoxal_cli_core::project::catalog::ArtifactKind::Service)
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
    let resolved = resolve_with_catalog(&robot, std::path::Path::new("."))?;

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
        .expect("ddsm115 assets package resolves from the catalog");
    assert_eq!(assets.package, "phoxal/component-ddsm115");
    assert_eq!(
        assets.source,
        phoxal_cli_core::project::resolver::ResolvedComponentSource::Catalog
    );

    Ok(())
}

#[test]
fn component_with_driver_block_resolves_both_assets_and_driver() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml().replace(
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
    assert_eq!(
        left_drive
            .assets
            .as_ref()
            .expect("ddsm115 assets package resolves from the catalog")
            .package,
        "phoxal/component-ddsm115"
    );
    let driver = left_drive.driver.as_ref().expect("driver package resolved");
    assert_eq!(driver.package, "phoxal/component-ddsm115");

    Ok(())
}

#[test]
fn component_version_pin_resolves_the_full_index_for_assets_and_driver() -> anyhow::Result<()> {
    let yaml = minimal_robot_yaml()
        .replace(
            "      mount_link: left_wheel_mount",
            "      mount_link: left_wheel_mount\n      driver:\n        connection: { type: can, bus: 0, node_id: 1 }",
        )
        .replace(
            "artifacts:\n  channel: stable",
            "artifacts:\n  channel: stable\n  pins:\n    phoxal/component-ddsm115: v0.2.0",
        );
    let robot = Robot::parse_from_string(&yaml)?;
    let target = host_target_triple();
    let catalog = fixture_catalog_for_tests(vec![
        fixture_component_assets_entry_for_tests("ddsm115", "0.1.0", CatalogChannel::Stable),
        fixture_component_driver_entry_for_tests(
            "ddsm115",
            "0.1.0",
            CatalogChannel::Stable,
            &target,
            true,
            Vec::new(),
        ),
        fixture_component_assets_entry_for_tests("ddsm115", "0.2.0", CatalogChannel::Stable),
        fixture_component_driver_entry_for_tests(
            "ddsm115",
            "0.2.0",
            CatalogChannel::Stable,
            &target,
            true,
            Vec::new(),
        ),
    ]);

    let resolved = resolve(
        &robot,
        std::path::Path::new("."),
        Some(&catalog),
        offline_options(),
    )?;
    let component = resolved
        .components
        .iter()
        .find(|component| component.instance == "left_drive")
        .expect("left_drive component resolved");
    assert_eq!(
        component
            .assets
            .as_ref()
            .expect("assets package resolved")
            .catalog_runtime
            .as_ref()
            .expect("pinned assets runtime")
            .version,
        "0.2.0"
    );
    assert_eq!(
        component
            .driver
            .as_ref()
            .and_then(|driver| driver.catalog_runtime.as_ref())
            .expect("pinned driver runtime")
            .version,
        "0.2.0"
    );
    Ok(())
}

#[test]
fn catalog_component_captures_the_release_asset_for_assets_and_driver() -> anyhow::Result<()> {
    // The resolver must capture, for a Catalog-sourced component package,
    // exactly the same shape a service captures: the resolved catalog
    // entry's version, the per-scope `ReleaseAsset`, and the resolved target
    // scope (assets for metadata, the target triple for drivers).
    let robot = Robot::parse_from_string(&minimal_robot_yaml().replace(
        "    left_drive:\n      component: ddsm115\n      mount_link: left_wheel_mount",
        "    left_drive:\n      component: ddsm115\n      mount_link: left_wheel_mount\n      driver:\n        connection: { type: can, bus: 0, node_id: 1 }",
    ))?;
    let target = host_target_triple();
    let mut assets_entry =
        fixture_component_assets_entry_for_tests("ddsm115", "0.1.0", CatalogChannel::Stable);
    assets_entry.as_asset_entry_mut().assets = Some(fixture_artifact_for_tests(
        "phoxal-component-ddsm115-assets-v0.1.0.tar.zst",
        &"a".repeat(64),
    ));
    let mut driver_entry = fixture_component_driver_entry_for_tests(
        "ddsm115",
        "0.1.0",
        CatalogChannel::Stable,
        &target,
        false,
        Vec::new(),
    );
    driver_entry.as_artifact_entry_mut().targets.insert(
        target.clone(),
        fixture_artifact_for_tests(
            &format!("phoxal-component-ddsm115-driver-v0.1.0-{target}.tar.zst"),
            &"b".repeat(64),
        ),
    );
    let catalog = fixture_catalog_for_tests(vec![
        fixture_service_entry_for_tests(
            "drive",
            "0.1.0",
            CatalogChannel::Stable,
            &target,
            false,
            vec![fixture_contract_for_tests("v1::drive::Target", "publish")],
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

    let assets = left_drive.assets.as_ref().expect("assets package resolved");
    assert_eq!(assets.source, ResolvedComponentSource::Catalog);
    let assets_runtime = assets
        .catalog_runtime
        .as_ref()
        .expect("catalog-sourced assets package captures a catalog_runtime");
    assert_eq!(assets_runtime.name, "ddsm115");
    assert_eq!(assets_runtime.version, "0.1.0");
    assert_eq!(
        assets_runtime.sha256.as_deref(),
        Some("a".repeat(64)).as_deref()
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
    let robot = Robot::parse_from_string(&minimal_robot_yaml())?;
    let mut component =
        fixture_component_assets_entry_for_tests("ddsm115", "0.1.0", CatalogChannel::Stable);
    component.as_asset_entry_mut().assets = None;
    let catalog = fixture_catalog_for_tests(vec![component]);
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
    let runtime = left_drive
        .assets
        .as_ref()
        .expect("assets package resolved (the catalog entry exists, just unpublished)")
        .catalog_runtime
        .as_ref()
        .expect("catalog_runtime is populated even with no release asset yet");
    assert!(runtime.sha256.is_none());
    assert!(!runtime.published);

    Ok(())
}

#[test]
fn declared_driver_with_no_target_blob_resolves_as_unpublished() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml().replace(
        "    left_drive:\n      component: ddsm115\n      mount_link: left_wheel_mount",
        "    left_drive:\n      component: ddsm115\n      mount_link: left_wheel_mount\n      driver:\n        connection: { type: can, bus: 0, node_id: 1 }",
    ))?;
    let catalog = fixture_catalog_for_tests(vec![
        fixture_service_entry_for_tests(
            "drive",
            "0.1.0",
            CatalogChannel::Stable,
            &host_target_triple(),
            false,
            vec![fixture_contract_for_tests("v1::drive::Target", "publish")],
        ),
        fixture_component_assets_entry_for_tests("ddsm115", "0.1.0", CatalogChannel::Stable),
    ]);

    let resolved = resolve(
        &robot,
        std::path::Path::new("."),
        Some(&catalog),
        offline_options(),
    )?;
    let driver = resolved.components[0]
        .driver
        .as_ref()
        .and_then(|driver| driver.catalog_runtime.as_ref())
        .expect("driver view resolves from the flattened component entry");
    assert!(!driver.published);
    assert!(driver.sha256.is_none());

    Ok(())
}

#[test]
fn resolves_known_api_to_its_official_set() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml())?;
    let resolved = resolve_with_catalog(&robot, std::path::Path::new("."))?;

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
    let robot = Robot::parse_from_string(&minimal_robot_yaml())?;
    let resolved = resolve_with_catalog(&robot, std::path::Path::new("."))?;

    for (tool_name, package) in [
        ("tool-bus", "phoxal/tool-bus"),
        ("tool-joypad", "phoxal/tool-joypad"),
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
        "phoxal/component-bno085",
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
fn artifacts_generation_field_is_rejected_as_a_dead_field() -> anyhow::Result<()> {
    // D1 (X-tools slice): the artifact catalog no longer carries a per-entry
    // API version, so `artifacts.generation` cannot mean anything against
    // it anymore. `resolve()` must reject it explicitly rather than silently
    // ignore it.
    let robot = Robot::parse_from_string(&minimal_robot_yaml().replace(
        "artifacts:\n  channel: stable",
        "artifacts:\n  channel: stable\n  generation: v1",
    ))?;
    let error = resolve_with_catalog(&robot, std::path::Path::new("."))
        .expect_err("artifacts.generation should be rejected");
    let message = error.to_string();

    assert!(message.contains("artifacts.generation"), "{message}");
    assert!(
        message.contains("no longer meaningful") || message.contains("no longer exists"),
        "{message}"
    );
    Ok(())
}

#[test]
fn frozen_catalog_is_name_driven_not_entry_channel_driven() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml())?;
    let target = "aarch64-unknown-linux-gnu";
    let catalog = fixture_catalog_for_tests(vec![
        fixture_service_entry_for_tests(
            "drive",
            "0.1.0",
            CatalogChannel::Nightly,
            target,
            true,
            vec![fixture_contract_for_tests("v1::drive::Target", "publish")],
        ),
        fixture_component_assets_entry_for_tests("ddsm115", "0.1.0", CatalogChannel::Stable),
    ]);
    let resolved = resolve(
        &robot,
        std::path::Path::new("."),
        Some(&catalog),
        ResolveOptions {
            official_target_triple: Some(target.to_string()),
            resolve_source_commits: false,
            resolve_component_asset_commits: false,
            ..ResolveOptions::default()
        },
    )?;

    assert_eq!(
        resolved.platform_runtimes.len(),
        phoxal_cli_core::project::catalog::OFFICIAL_SERVICES.len()
    );
    Ok(())
}

#[test]
fn official_only_robot_without_catalog_keeps_no_catalog_error() -> anyhow::Result<()> {
    let robot = Robot::parse_from_string(&minimal_robot_yaml())?;
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

    assert!(message.contains("no vendored binaries"), "{message}");
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

    fs::write(&base_path, minimal_robot_yaml())?;
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
        "phoxal/component-ddsm115",
        "../framework/component/ddsm115",
    ))?;
    let resolved = resolve_with_catalog(&robot, temp.path())?;
    let left_drive = resolved
        .components
        .iter()
        .find(|component| component.instance == "left_drive")
        .expect("left_drive component resolved");

    assert_eq!(
        left_drive
            .assets
            .as_ref()
            .expect("assets package resolved")
            .path_override(),
        Some(temp.path().join("../framework/component/ddsm115").as_path())
    );
    assert_eq!(
        resolved
            .path_overrides
            .iter()
            .find(|override_| override_.key == "phoxal/component-ddsm115")
            .map(|override_| override_.kind),
        Some(ResolvedPathOverrideKind::ComponentAssets)
    );
    Ok(())
}

fn resolve_with_catalog(robot: &Robot, root: &std::path::Path) -> anyhow::Result<ResolvedRobot> {
    let catalog = test_catalog();
    resolve(robot, root, Some(&catalog), offline_options())
}

fn test_catalog() -> Catalog {
    let target = host_target_triple();
    let mut entries = service_names()
        .into_iter()
        .map(|name| {
            fixture_service_entry_for_tests(
                name,
                "0.1.0",
                CatalogChannel::Stable,
                &target,
                false,
                vec![fixture_contract_for_tests("v1::drive::Target", "publish")],
            )
        })
        .collect::<Vec<_>>();
    for name in component_names() {
        entries.push(fixture_component_assets_entry_for_tests(
            name,
            "0.1.0",
            CatalogChannel::Stable,
        ));
        entries.push(fixture_component_driver_entry_for_tests(
            name,
            "0.1.0",
            CatalogChannel::Stable,
            &target,
            false,
            vec![fixture_contract_for_tests(
                "v1::component::State",
                "publish",
            )],
        ));
    }
    entries.extend([
        fixture_tool_entry_for_tests(
            "router",
            "0.1.0",
            CatalogChannel::Stable,
            &target,
            false,
            Vec::new(),
        ),
        fixture_tool_entry_for_tests(
            "joypad",
            "0.1.0",
            CatalogChannel::Stable,
            &target,
            false,
            vec![fixture_contract_for_tests("v1::drive::Target", "subscribe")],
        ),
        fixture_simulator_entry_for_tests(
            "webots-supervisor",
            "0.14.0",
            CatalogChannel::Stable,
            &target,
            false,
            Vec::new(),
        ),
        fixture_simulator_entry_for_tests(
            "webots-controller",
            "0.14.0",
            CatalogChannel::Stable,
            &target,
            false,
            vec![fixture_contract_for_tests(
                "v1::component::MotorCommand",
                "publish",
            )],
        ),
    ]);
    fixture_catalog_for_tests(entries)
}

fn service_names() -> Vec<&'static str> {
    vec![
        "asset",
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
artifacts:
  channel: stable
"#
    .to_string()
}

fn robot_with_path_pin(key: &str, path: &str) -> String {
    minimal_robot_yaml().replace(
        "artifacts:\n  channel: stable",
        &format!("artifacts:\n  channel: stable\n  pins:\n    {key}:\n      path: {path}"),
    )
}

fn robot_with_user_service(services: &str) -> String {
    minimal_robot_yaml().replace(
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
