use std::fs;

use phoxal_cli::catalog::{
    ArtifactStatus, Channel as CatalogChannel, fixture_catalog_for_tests,
    fixture_tool_entry_for_tests,
};
use phoxal_cli::commands::simulate::{SimulateOptions, prepare};
use phoxal_cli::resolver::host_target_triple;

#[test]
fn simulate_dry_run_resolves_without_writing_local_launch_directories() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    write_robot_project(temp.path())?;
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
    assert!(!temp.path().join(".phoxal/cache/state.yaml").exists());
    assert!(!temp.path().join("phoxal.sources.lock").exists());
    assert_eq!(
        plan.bus_connect,
        phoxal_cli::launch_plan::DEFAULT_ROUTER_CONNECT
    );
    assert_eq!(plan.world_path, temp.path().join("worlds/test.wbt"));
    assert_eq!(
        plan.native_tools,
        vec![
            phoxal_cli::launch_plan::SITE_TOOL_ROUTER.to_string(),
            phoxal_cli::launch_plan::SITE_TOOL_JOYPAD.to_string(),
            "webots".to_string(),
        ]
    );
    assert!(plan.resolved.platform_runtimes.is_empty());

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
    r#"schema: v0

identity:
  id: testbot
  namespace: test

structure: structure.urdf

phoxal_artifacts:
  channel: stable
  generation: y2026_1
phoxal_participants: {}

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

fn write_catalog(root: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    let path = root.join("catalog.json");
    let catalog = fixture_catalog_for_tests(vec![
        fixture_tool_entry_for_tests(
            "router",
            "y2026_1",
            "0.1.0",
            CatalogChannel::Stable,
            &host_target_triple(),
            ArtifactStatus::Pending,
            Vec::new(),
        ),
        fixture_tool_entry_for_tests(
            "joypad",
            "y2026_1",
            "0.1.0",
            CatalogChannel::Stable,
            &host_target_triple(),
            ArtifactStatus::Pending,
            Vec::new(),
        ),
    ]);
    fs::write(&path, serde_json::to_string_pretty(&catalog)?)?;
    Ok(path)
}
