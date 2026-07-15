use std::fs;

use anyhow::Context;
use phoxal_cli::catalog::{
    SelectionChannel as CatalogChannel, fixture_catalog_for_tests, fixture_tool_entry_for_tests,
};
use phoxal_cli::commands::simulate::{SimulateOptions, prepare};
use phoxal_cli::resolver::host_target_triple;

#[test]
fn simulate_dry_run_resolves_without_writing_local_launch_directories() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    write_robot_project(temp.path())?;
    write_vendored_fixture_binaries(temp.path())?;
    let catalog_path = write_catalog(temp.path())?;

    let plan = prepare(
        temp.path(),
        SimulateOptions {
            world: "test".to_string(),
            catalog_source: Some(catalog_path.display().to_string()),
            ..SimulateOptions::default()
        },
    )?;

    assert!(!temp.path().join(".phoxal/run").exists());
    assert!(!temp.path().join(".phoxal/webots").exists());
    assert!(!temp.path().join("phoxal.sources.lock").exists());
    // `LaunchMode::Webots` carries the resolved world path directly now
    // (there is no separate `world_path` field to check against).
    match &plan.plan.mode {
        phoxal_cli::launch_plan::LaunchMode::Webots { world } => {
            assert_eq!(*world, temp.path().join("worlds/test.wbt"));
        }
        other => panic!("expected LaunchMode::Webots, got {other:?}"),
    }
    // Router, joypad, and telemetry are all standard, hard-required site
    // tools in every mode including `simulate` (product decision 9) - no
    // opt-in flag, no graceful-degrade path. `fixture_catalog_for_tests`
    // auto-fills every `catalog::OFFICIAL_TOOLS` entry not explicitly listed
    // in `write_catalog` above, which is why joypad and telemetry resolve
    // here despite not being in that fixture list.
    let site_ids = plan
        .plan
        .site
        .iter()
        .map(|site| site.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        site_ids,
        vec![
            phoxal_cli::launch_plan::SITE_TOOL_ROUTER,
            phoxal_cli::launch_plan::SITE_TOOL_JOYPAD,
            phoxal_cli::launch_plan::SITE_TOOL_TELEMETRY,
        ]
    );
    assert_eq!(
        plan.ctx.resolved.platform_runtimes.len(),
        phoxal_cli::catalog::OFFICIAL_SERVICES.len()
    );

    Ok(())
}

fn write_vendored_fixture_binaries(root: &std::path::Path) -> anyhow::Result<()> {
    // `prepare` is called directly rather than through AppContext in this
    // integration test, so point project-local path helpers at its fixture.
    unsafe { std::env::set_var(phoxal_cli::host_paths::PROJECT_ROOT_ENV, root) };
    let source = std::env::current_exe()?;
    let target = host_target_triple();
    for (name, package, binary) in phoxal_cli::catalog::OFFICIAL_SERVICES
        .iter()
        .map(|(name, package)| (*name, *package, format!("phoxal-service-{name}")))
        .chain(
            phoxal_cli::catalog::OFFICIAL_TOOLS
                .iter()
                .map(|(name, package)| (*name, *package, format!("phoxal-tool-{name}"))),
        )
        .chain(
            phoxal_cli::catalog::OFFICIAL_SIMULATORS
                .iter()
                .map(|(name, package)| (*name, *package, format!("phoxal-simulator-{name}"))),
        )
    {
        let _ = name;
        let (provider, package_name) = package
            .split_once('/')
            .context("fixture package must be provider-qualified")?;
        let package_dir = root
            .join(".phoxal/artifacts")
            .join(provider)
            .join(package_name);
        let version_dir = package_dir.join("versions/0.1.0/targets").join(&target);
        fs::create_dir_all(&version_dir)?;
        fs::copy(&source, version_dir.join(binary))?;
        #[cfg(unix)]
        std::os::unix::fs::symlink("versions/0.1.0", package_dir.join("active"))?;
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir("versions/0.1.0", package_dir.join("active"))?;
    }
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
  components: {}

artifacts:
  channel: stable
"#
}

fn write_catalog(root: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    let path = root.join("catalog.json");
    let catalog = fixture_catalog_for_tests(vec![
        fixture_tool_entry_for_tests(
            "router",
            "0.1.0",
            CatalogChannel::Stable,
            &host_target_triple(),
            false,
            Vec::new(),
        ),
        fixture_tool_entry_for_tests(
            "joypad",
            "0.1.0",
            CatalogChannel::Stable,
            &host_target_triple(),
            false,
            Vec::new(),
        ),
    ]);
    fs::write(&path, serde_json::to_string_pretty(&catalog)?)?;
    Ok(path)
}
