use super::*;
use crate::paths::host::test_support::ScratchPhoxalHome;

/// A minimal robot workspace whose locked train resolves to `0.1.0` via a
/// local path dependency on a stub `phoxal` crate - no registry, no network.
/// `resolve()` always calls `resolve_locked_project`, so every test needs
/// this fixture even when it never touches a component.
fn locked_project_root() -> anyhow::Result<tempfile::TempDir> {
    let root = tempfile::tempdir()?;
    std::fs::create_dir_all(root.path().join("src"))?;
    std::fs::create_dir_all(root.path().join("train/phoxal/src"))?;
    std::fs::create_dir_all(root.path().join("components/fixture/src"))?;
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"robot\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n[workspace]\nmembers = [\".\", \"components/fixture\"]\nresolver = \"2\"\n\n[dependencies]\nphoxal = { path = \"train/phoxal\" }\n",
    )?;
    std::fs::write(root.path().join("src/lib.rs"), "")?;
    std::fs::write(
        root.path().join("train/phoxal/Cargo.toml"),
        "[package]\nname = \"phoxal\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    std::fs::write(root.path().join("train/phoxal/src/lib.rs"), "")?;
    std::fs::write(
        root.path().join("components/fixture/Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\npath = \"src/lib.rs\"\n",
    )?;
    std::fs::write(root.path().join("components/fixture/src/lib.rs"), "")?;
    std::fs::write(
        root.path().join("components/fixture/component.yaml"),
        "schema: component/v0\ncapabilities:\n  motor:\n    kind: motor\n    command: velocity\n    target:\n      kind: joint\n      id: wheel_joint\n",
    )?;
    std::fs::write(
        root.path().join("components/fixture/structure.urdf"),
        r#"<robot name="component"><link name="base"/><link name="wheel"/><joint name="wheel_joint" type="continuous"><parent link="base"/><child link="wheel"/></joint></robot>"#,
    )?;
    write_lock(root.path(), &[])?;
    Ok(root)
}

/// (Re)write `Cargo.lock` for `root`, covering `phoxal`/`robot` plus one
/// entry per name in `extra_packages`. `resolve_locked_project` runs `cargo
/// metadata --locked`, which fails if the lock does not already cover every
/// workspace member, so this must be called after the member crates exist
/// (and, for a workspace member, after [`declare_workspace_member`]) and
/// before `resolve()`.
fn write_lock(root: &std::path::Path, extra_packages: &[&str]) -> anyhow::Result<()> {
    let mut lock = String::from(
        "version = 4\n\n[[package]]\nname = \"fixture\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"phoxal\"\nversion = \"0.1.0\"\n\n",
    );
    for name in extra_packages {
        lock.push_str(&format!(
            "[[package]]\nname = \"{name}\"\nversion = \"0.1.0\"\n\n"
        ));
    }
    lock.push_str(
        "[[package]]\nname = \"robot\"\nversion = \"0.1.0\"\ndependencies = [\"phoxal\"]\n",
    );
    std::fs::write(root.join("Cargo.lock"), lock)?;
    Ok(())
}

/// Turn `root`'s plain train-anchor package into a real Cargo workspace
/// listing itself plus `member` (a `services/`, `tools/`, or `components/`
/// crate a test just created). `locked_project_root` deliberately declares no
/// `[workspace]` table - a glob member errors when a test's temp dir has no
/// matching crate yet - so a test that adds one calls this with the exact
/// relative path instead.
fn declare_workspace_member(root: &std::path::Path, member: &str) -> anyhow::Result<()> {
    std::fs::write(
        root.join("Cargo.toml"),
        format!(
            "[package]\nname = \"robot\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n[workspace]\nmembers = [\".\", \"components/fixture\", \"{member}\"]\nresolver = \"2\"\n\n[dependencies]\nphoxal = {{ path = \"train/phoxal\" }}\n"
        ),
    )?;
    Ok(())
}

fn minimal_robot(extra: &str) -> anyhow::Result<Robot> {
    minimal_robot_with_components("{}", extra)
}

fn minimal_robot_with_components(components: &str, extra: &str) -> anyhow::Result<Robot> {
    let (components, actuators) = if components.trim() == "{}" {
        (
            "\n    drive:\n      component: fixture\n      mount_link: base",
            "[drive.motor]",
        )
    } else {
        (components, "[left_drive.motor]")
    };
    phoxal_manifest::source::robot::parse_from_string(&format!(
        r#"schema: robot/v0
robot:
  id: testbot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: omnidirectional
    actuators: {actuators}
    encoders: []
  components: {components}
{extra}"#
    ))
}

/// Persist the real compiler inputs that production resolution consumes.
///
/// These tests used to pass only an already-parsed manifest into a test-only
/// resolution fork. Keeping the source tree explicit exercises the same
/// single compiler path as `check`, `run`, `simulate`, and `build`.
fn write_compiler_sources(root: &std::path::Path, robot: &Robot) -> anyhow::Result<()> {
    phoxal_manifest::source::robot::write_to_dir(robot, root)?;
    let structure = root.join(&robot.robot.structure);
    if let Some(parent) = structure.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        structure,
        r#"<robot name="fixture"><link name="base_footprint"/><link name="base_link"/><link name="base"/><joint name="root" type="fixed"><parent link="base_footprint"/><child link="base_link"/></joint><joint name="base_mount" type="fixed"><parent link="base_link"/><child link="base"/></joint></robot>"#,
    )?;
    for component_type in robot.used_component_types() {
        let component_root = root.join("components").join(component_type);
        std::fs::create_dir_all(&component_root)?;
        std::fs::write(
            component_root.join("component.yaml"),
            "schema: component/v0\ncapabilities:\n  motor:\n    kind: motor\n    command: velocity\n    target:\n      kind: joint\n      id: wheel_joint\n",
        )?;
        std::fs::write(
            component_root.join("structure.urdf"),
            r#"<robot name="component"><link name="base"/><link name="wheel"/><joint name="wheel_joint" type="continuous"><parent link="base"/><child link="wheel"/></joint></robot>"#,
        )?;
    }
    Ok(())
}

fn resolve_fixture(
    robot: &Robot,
    root: &std::path::Path,
    options: ResolveOptions,
) -> anyhow::Result<BundlePlan> {
    write_compiler_sources(root, robot)?;
    resolve(robot, root, options)
}

#[test]
fn an_invalid_declaration_fails_before_locked_project_resolution() -> anyhow::Result<()> {
    // The declaration validator is the first operation in `resolve` (#950):
    // an official identity in a map must fail with the declaration error even
    // when there is no Cargo project at all (which would otherwise be the
    // first failure) - proving the ordering, not just the presence, of the
    // check.
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let robot = minimal_robot("tools:\n  drive: {}\n")?;
    let error = resolve(&robot, Path::new("/nonexistent"), ResolveOptions::default())
        .expect_err("an official identity in tools: must fail resolution");
    let message = format!("{error:#}");
    assert!(
        message.contains("official service"),
        "the declaration error must win over the missing-project error: {message}"
    );
    Ok(())
}

#[test]
fn platform_runtimes_resolve_from_the_catalog_at_the_locked_train() -> anyhow::Result<()> {
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let robot = minimal_robot("")?;
    let project = locked_project_root()?;

    let resolved = resolve_fixture(&robot, project.path(), ResolveOptions::default())?;

    assert_eq!(resolved.train, "0.1.0");
    let drive = resolved
        .platform_runtimes
        .iter()
        .find(|runtime| runtime.package == "phoxal/service-drive")
        .expect("drive is a catalog service");
    assert_eq!(drive.name, "drive");
    assert_eq!(drive.train, "0.1.0");
    assert_eq!(drive.target.as_deref(), Some(host_target_triple().as_str()));
    assert!(drive.path_override.is_none());

    // Every catalog service is present; the official set is CLI-internal, not
    // subject to any per-robot pruning.
    assert_eq!(
        resolved.platform_runtimes.len(),
        catalog::NATIVE
            .iter()
            .filter(|official| official.kind == ArtifactKind::Service)
            .count()
    );

    let bus = resolved
        .tools
        .iter()
        .find(|tool| tool.package == "phoxal/tool-bus")
        .expect("bus is a catalog tool");
    assert_eq!(bus.binary_name, "phoxal-tool-bus");
    let router = resolved
        .tools
        .iter()
        .find(|tool| tool.package == "phoxal/infrastructure-router")
        .expect("the infrastructure router is always resolved");
    assert_eq!(router.binary_name, "phoxal-infrastructure-router");
    Ok(())
}

#[test]
fn include_simulators_toggles_the_webots_controller_only() -> anyhow::Result<()> {
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let robot = minimal_robot("")?;
    let project = locked_project_root()?;

    let with_simulators = resolve_fixture(&robot, project.path(), ResolveOptions::default())?;
    assert!(
        with_simulators
            .simulators
            .iter()
            .any(|runtime| runtime.package == "phoxal/simulator-webots-controller")
    );

    let without_simulators = resolve_fixture(
        &robot,
        project.path(),
        ResolveOptions {
            include_simulators: false,
            ..ResolveOptions::default()
        },
    )?;
    assert!(
        without_simulators.simulators.is_empty(),
        "a Native bundle must carry no simulator-only runtimes"
    );
    Ok(())
}

#[test]
fn a_matching_workspace_service_crate_overrides_the_official_binary_without_declaration()
-> anyhow::Result<()> {
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let robot = minimal_robot("")?;
    let project = locked_project_root()?;
    let crate_dir = project.path().join("services/drive");
    std::fs::create_dir_all(crate_dir.join("src"))?;
    std::fs::write(
        crate_dir.join("Cargo.toml"),
        "[package]\nname = \"drive\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[[bin]]\nname = \"drive\"\npath = \"src/main.rs\"\n",
    )?;
    std::fs::write(crate_dir.join("src/main.rs"), "fn main() {}")?;
    declare_workspace_member(project.path(), "services/drive")?;
    write_lock(project.path(), &["drive"])?;
    let crate_dir = crate_dir.canonicalize()?;

    let resolved = resolve_fixture(&robot, project.path(), ResolveOptions::default())?;

    let drive = resolved
        .platform_runtimes
        .iter()
        .find(|runtime| runtime.package == "phoxal/service-drive")
        .expect("drive still resolves as the official identity");
    assert_eq!(drive.path_override.as_deref(), Some(crate_dir.as_path()));
    assert_eq!(resolved.path_overrides.len(), 1);
    assert_eq!(resolved.path_overrides[0].artifact_name, "drive");
    Ok(())
}

#[test]
fn a_declared_user_service_with_no_workspace_crate_fails_resolution() -> anyhow::Result<()> {
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let robot = minimal_robot("services:\n  mission: {}\n")?;
    let project = locked_project_root()?;

    write_compiler_sources(project.path(), &robot)?;
    let error = resolve(&robot, project.path(), ResolveOptions::default())
        .expect_err("a declared service with no matching crate must fail");
    assert!(
        format!("{error:#}").contains("services/ workspace crate"),
        "{error:#}"
    );
    Ok(())
}

#[test]
fn an_undiscovered_workspace_service_is_a_drift_diagnostic_not_an_error() -> anyhow::Result<()> {
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let robot = minimal_robot("")?;
    let project = locked_project_root()?;
    let crate_dir = project.path().join("services/mission");
    std::fs::create_dir_all(crate_dir.join("src"))?;
    std::fs::write(
        crate_dir.join("Cargo.toml"),
        "[package]\nname = \"mission\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[[bin]]\nname = \"mission\"\npath = \"src/main.rs\"\n",
    )?;
    std::fs::write(crate_dir.join("src/main.rs"), "fn main() {}")?;
    declare_workspace_member(project.path(), "services/mission")?;
    write_lock(project.path(), &["mission"])?;

    let resolved = resolve_fixture(&robot, project.path(), ResolveOptions::default())?;
    assert_eq!(resolved.undeclared_runtimes.len(), 1);
    assert_eq!(resolved.undeclared_runtimes[0].name, "mission");
    assert_eq!(resolved.undeclared_runtimes[0].family, "services");
    Ok(())
}

/// A workspace `components/<id>` crate is resolved without ever touching the
/// registry (`resolve_components` skips the generated manifest entirely when
/// every component is workspace-provided).
#[test]
fn a_workspace_component_resolves_its_assets_and_driver_without_the_registry() -> anyhow::Result<()>
{
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let robot = minimal_robot_with_components(
        r#"
    left_drive:
      component: ddsm115
      mount_link: base
      driver:
        connection:
          type: serial
          port: /dev/ttyUSB0
          baud: 115200
"#,
        "",
    )?;
    let project = locked_project_root()?;
    let crate_dir = project.path().join("components/ddsm115");
    std::fs::create_dir_all(crate_dir.join("src"))?;
    std::fs::write(
        crate_dir.join("Cargo.toml"),
        "[package]\nname = \"ddsm115\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[[bin]]\nname = \"ddsm115\"\npath = \"src/main.rs\"\n",
    )?;
    std::fs::write(crate_dir.join("src/main.rs"), "fn main() {}")?;
    std::fs::write(crate_dir.join("component.yaml"), "schema: component/v0\n")?;
    declare_workspace_member(project.path(), "components/ddsm115")?;
    write_lock(project.path(), &["ddsm115"])?;
    let crate_dir = crate_dir.canonicalize()?;

    let resolved = resolve_fixture(&robot, project.path(), ResolveOptions::default())?;
    assert_eq!(resolved.components.len(), 1);
    let component = &resolved.components[0];
    assert_eq!(
        component.assets.resolved_dir.as_deref(),
        Some(crate_dir.as_path())
    );
    assert_eq!(
        component.assets.source,
        ResolvedComponentSource::Path {
            path: crate_dir.clone()
        }
    );
    let driver = component.driver.as_ref().expect("driver resolved");
    assert_eq!(driver.resolved_dir.as_deref(), Some(crate_dir.as_path()));
    Ok(())
}

/// The driver policy gates resolution itself (#936): an excluded driver
/// instance keeps `has_driver: true` (the declared intent) but resolves no
/// driver package at all, so nothing downstream ever requires, builds, or
/// installs a binary for it.
#[test]
fn an_excluded_driver_resolves_no_driver_package() -> anyhow::Result<()> {
    let _phoxal_home = ScratchPhoxalHome::new()?;
    let robot = minimal_robot_with_components(
        r#"
    left_drive:
      component: ddsm115
      mount_link: base
      driver:
        connection:
          type: serial
          port: /dev/ttyUSB0
          baud: 115200
"#,
        "",
    )?;
    let project = locked_project_root()?;
    let crate_dir = project.path().join("components/ddsm115");
    std::fs::create_dir_all(crate_dir.join("src"))?;
    std::fs::write(
        crate_dir.join("Cargo.toml"),
        "[package]\nname = \"ddsm115\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[[bin]]\nname = \"ddsm115\"\npath = \"src/main.rs\"\n",
    )?;
    std::fs::write(crate_dir.join("src/main.rs"), "fn main() {}")?;
    std::fs::write(crate_dir.join("component.yaml"), "schema: component/v0\n")?;
    declare_workspace_member(project.path(), "components/ddsm115")?;
    write_lock(project.path(), &["ddsm115"])?;

    let resolved = resolve_fixture(
        &robot,
        project.path(),
        ResolveOptions {
            drivers: phoxal_cli_core::project::layout::DriverSelection::None,
            ..ResolveOptions::default()
        },
    )?;
    let component = &resolved.components[0];
    assert!(component.has_driver, "declared intent is preserved");
    assert!(
        component.driver.is_none(),
        "an excluded driver resolves no package at all"
    );
    Ok(())
}

#[test]
fn resolve_target_triple_accepts_aliases_and_full_triples() {
    assert_eq!(
        resolve_target_triple("aarch64").unwrap(),
        "aarch64-unknown-linux-gnu"
    );
    assert_eq!(
        resolve_target_triple("arm64").unwrap(),
        "aarch64-unknown-linux-gnu"
    );
    assert_eq!(
        resolve_target_triple("x86_64").unwrap(),
        "x86_64-unknown-linux-gnu"
    );
    assert_eq!(
        resolve_target_triple("riscv64gc-unknown-linux-gnu").unwrap(),
        "riscv64gc-unknown-linux-gnu"
    );
    assert!(resolve_target_triple("nonsense").is_err());
}
