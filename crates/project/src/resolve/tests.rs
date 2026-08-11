use super::*;

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
    // The root package IS the mandatory brain: one auto-discovered bin target
    // and no library ().
    std::fs::write(root.path().join("src/main.rs"), "fn main() {}")?;
    std::fs::write(
        root.path().join("train/phoxal/Cargo.toml"),
        "[package]\nname = \"phoxal\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    std::fs::write(root.path().join("train/phoxal/src/lib.rs"), "")?;
    std::fs::write(
        root.path().join("components/fixture/Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[[bin]]\nname = \"fixture\"\npath = \"src/main.rs\"\n",
    )?;
    std::fs::write(
        root.path().join("components/fixture/src/main.rs"),
        "fn main() {}",
    )?;
    std::fs::write(
        root.path().join("components/fixture/component.yaml"),
        "schema: phoxal/component/v0\ncapabilities:\n  motor:\n    kind: motor\n    command: velocity\n    target:\n      kind: joint\n      id: wheel_joint\n",
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

/// Turn `root`'s plain root brain package into a real Cargo workspace
/// listing itself plus `member` (a `services/` or `components/`
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
    crate::source::resolver::parse_robot_from_string(&format!(
        r#"schema: phoxal/robot/v0
robot:
  id: testbot
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
    crate::source::resolver::write_robot_to_dir(robot, root)?;
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
            "schema: phoxal/component/v0\ncapabilities:\n  motor:\n    kind: motor\n    command: velocity\n    target:\n      kind: joint\n      id: wheel_joint\n",
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

/// The executor a staged release would package. Resolution never reads it -
/// only the release step does - so a fixture path is exactly enough here.
fn fixture_executor() -> std::path::PathBuf {
    std::path::PathBuf::from("/fixture/phoxald")
}

#[derive(Default)]
struct RecordingReporter(std::sync::Mutex<Vec<crate::PreparationEvent>>);

impl crate::Reporter for RecordingReporter {
    fn report(&self, event: crate::PreparationEvent) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event);
    }
}

#[test]
fn container_resolution_compiles_once_and_rejects_profile_drift() -> anyhow::Result<()> {
    let robot = minimal_robot("")?;
    let project = locked_project_root()?;
    write_compiler_sources(project.path(), &robot)?;
    let reporter = RecordingReporter::default();

    let mut resolved = crate::build::resolve_container_staging(
        project.path(),
        &project.path().join(".phoxal/cache/registry"),
        "aarch64-unknown-linux-gnu",
        fixture_executor(),
        false,
        &reporter,
    )?;
    assert!(
        resolved
            .set_materialization_build(crate::build::profile::StagingBuild::host_runtime(
                fixture_executor()
            ))
            .is_err(),
        "a host-runtime profile must not replace a native-bundle resolution"
    );
    let target_dir = tempfile::tempdir()?;
    resolved.set_materialization_build(
        crate::build::profile::StagingBuild::prebuilt_native_bundle(
            "aarch64-unknown-linux-gnu".to_string(),
            fixture_executor(),
            target_dir.path().to_path_buf(),
            None,
        ),
    )?;

    // Later staging consumes `ResolvedStagingInput` directly and has no path
    // back to `resolve_staging`; this count pins the production container
    // helper that creates the sole compiler phase.
    let events = reporter
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let compile_phases = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                crate::PreparationEvent::PhaseStarted { id, .. }
                    if id.to_string() == "validate"
            )
        })
        .count();
    assert_eq!(
        compile_phases, 1,
        "container package selection must produce one manifest compilation"
    );
    Ok(())
}

#[test]
fn container_snapshot_uses_the_live_registry_cache_for_components_and_metadata()
-> anyhow::Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use sha2::{Digest, Sha256};

    struct NoHttp(AtomicUsize);
    impl crate::registry_package::RegistryHttp for NoHttp {
        fn get(&self, url: &str) -> anyhow::Result<Vec<u8>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("offline cache unexpectedly requested {url}")
        }
    }

    let robot = minimal_robot_with_components(
        r#"
    left_drive:
      component: wheel
      mount_link: base
"#,
        "",
    )?;
    let snapshot = locked_project_root()?;
    crate::source::resolver::write_robot_to_dir(&robot, snapshot.path())?;
    std::fs::write(
        snapshot.path().join("structure.urdf"),
        r#"<robot name="fixture"><link name="base_footprint"/><link name="base_link"/><link name="base"/><joint name="root" type="fixed"><parent link="base_footprint"/><child link="base_link"/></joint><joint name="base_mount" type="fixed"><parent link="base_link"/><child link="base"/></joint></robot>"#,
    )?;

    let live = tempfile::tempdir()?;
    let cache_root = live.path().join(".phoxal/cache/registry");
    let package = phoxal_cli_catalog::cargo_package_name("phoxal/component-wheel");
    let version = "0.1.0";
    let manifest = format!(
        "[package]\nname = {package:?}\nversion = {version:?}\n\n[[bin]]\nname = {package:?}\npath = \"src/main.rs\"\n"
    );
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut archive = tar::Builder::new(encoder);
    for (path, bytes) in [
        ("Cargo.toml", manifest.as_bytes()),
        ("src/main.rs", b"fn main() {}" as &[u8]),
        ("component.yaml", b"schema: phoxal/component/v0\ncapabilities:\n  motor:\n    kind: motor\n    command: velocity\n    target:\n      kind: joint\n      id: wheel_joint\n" as &[u8]),
        ("structure.urdf", br#"<robot name="component"><link name="base"/><link name="wheel"/><joint name="wheel_joint" type="continuous"><parent link="base"/><child link="wheel"/></joint></robot>"# as &[u8]),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive.append_data(
            &mut header,
            format!("{package}-{version}/{path}"),
            bytes,
        )?;
    }
    let bytes = archive.into_inner()?.finish()?;
    let checksum = hex::encode(Sha256::digest(&bytes));
    let cache_dir = cache_root.join(&package).join(version);
    std::fs::create_dir_all(&cache_dir)?;
    std::fs::write(cache_dir.join(format!("{checksum}.crate")), bytes)?;

    let reporter = RecordingReporter::default();
    let resolved = crate::build::resolve_container_staging(
        snapshot.path(),
        &cache_root,
        "aarch64-unknown-linux-gnu",
        fixture_executor(),
        true,
        &reporter,
    )?;
    assert_eq!(resolved.resolved().components.len(), 1);
    assert!(
        resolved.resolved().components[0]
            .assets_root
            .starts_with(&cache_root)
    );
    assert!(!snapshot.path().join(".phoxal/cache/registry").exists());

    let no_http = NoHttp(AtomicUsize::new(0));
    let metadata_cache = crate::registry_package::PackageCache::new(cache_root.clone());
    assert!(
        crate::registry_package::fetch_registry_package(
            &no_http,
            &metadata_cache,
            &package,
            version,
            true,
        )?
        .manifest()?
        .contains(&package)
    );
    assert_eq!(no_http.0.load(Ordering::SeqCst), 0);
    assert_eq!(
        std::fs::read_dir(&cache_dir)?
            .filter_map(Result::ok)
            .filter(|entry| entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "crate"))
            .count(),
        1
    );
    Ok(())
}

#[test]
fn an_invalid_declaration_fails_before_locked_project_resolution() -> anyhow::Result<()> {
    // The declaration validator is the first operation in `resolve` ():
    // an official identity in a map must fail with the declaration error even
    // when there is no Cargo project at all (which would otherwise be the
    // first failure) - proving the ordering, not just the presence, of the
    // check.
    let robot = minimal_robot("services:\n  drive: {}\n")?;
    let error = resolve(&robot, Path::new("/nonexistent"), ResolveOptions::default())
        .expect_err("an official identity in services: must fail resolution");
    let message = format!("{error:#}");
    assert!(
        message.contains("official service"),
        "the declaration error must win over the missing-project error: {message}"
    );
    Ok(())
}

#[test]
fn platform_runtimes_resolve_from_the_catalog_at_the_locked_train() -> anyhow::Result<()> {
    let robot = minimal_robot("")?;
    let project = locked_project_root()?;

    let resolved = resolve_fixture(&robot, project.path(), ResolveOptions::default())?;

    assert_eq!(resolved.train.version(), "0.1.0");
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
        phoxal_cli_catalog::Catalog::official()
            .native()
            .filter(|official| official.kind == ArtifactKind::Service)
            .count()
    );

    // Anything the supervisor absorbed - or that became a local CLI concern -
    // must never resolve as an artifact: a stale catalog entry here is not a
    // compile error, it is a `cargo install` failure at run time for a package
    // the train no longer publishes.
    for absorbed in [
        "router",
        "asset",
        "tool-bus",
        "tool-device",
        "tool-log",
        "tool-telemetry",
        "tool-joypad",
        // The whole `tool-` family is gone; catch any survivor by prefix too.
        "phoxal/tool-",
    ] {
        assert!(
            !resolved
                .platform_runtimes
                .iter()
                .any(|runtime| runtime.package.contains(absorbed)),
            "{absorbed} must not be a resolved artifact"
        );
    }
    Ok(())
}

#[test]
fn include_simulators_toggles_the_webots_controller_only() -> anyhow::Result<()> {
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
    std::fs::write(
        crate_dir.join("component.yaml"),
        "schema: phoxal/component/v0\n",
    )?;
    declare_workspace_member(project.path(), "components/ddsm115")?;
    write_lock(project.path(), &["ddsm115"])?;
    let crate_dir = crate_dir.canonicalize()?;

    let resolved = resolve_fixture(&robot, project.path(), ResolveOptions::default())?;
    assert_eq!(resolved.components.len(), 1);
    let component = &resolved.components[0];
    assert_eq!(component.assets_root, crate_dir);
    let driver = component.driver.as_ref().expect("driver resolved");
    assert_eq!(driver.source_path(), Some(crate_dir.as_path()));
    Ok(())
}

#[test]
fn local_component_roots_are_independent_of_driver_intent() -> anyhow::Result<()> {
    let driverless_robot = minimal_robot_with_components(
        r#"
    left_drive:
      component: fixture
      mount_link: base
"#,
        "",
    )?;
    let driver_project = locked_project_root()?;
    let resolved = resolve_fixture(
        &driverless_robot,
        driver_project.path(),
        ResolveOptions::default(),
    )?;
    assert_eq!(resolved.components.len(), 1);
    assert!(resolved.components[0].driver.is_none());
    assert_eq!(
        resolved.components[0].assets_root,
        driver_project
            .path()
            .join("components/fixture")
            .canonicalize()?
    );

    let asset_only_project = locked_project_root()?;
    let fixture = asset_only_project.path().join("components/fixture");
    std::fs::remove_file(fixture.join("Cargo.toml"))?;
    std::fs::remove_dir_all(fixture.join("src"))?;
    std::fs::write(
        asset_only_project.path().join("Cargo.toml"),
        "[package]\nname = \"robot\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n[workspace]\nmembers = [\".\"]\nresolver = \"2\"\n\n[dependencies]\nphoxal = { path = \"train/phoxal\" }\n",
    )?;
    let declared_driver_robot = minimal_robot_with_components(
        r#"
    left_drive:
      component: fixture
      mount_link: base
      driver:
        connection:
          type: serial
          port: /dev/ttyUSB0
          baud: 115200
"#,
        "",
    )?;
    let error = resolve_fixture(
        &declared_driver_robot,
        asset_only_project.path(),
        ResolveOptions::default(),
    )
    .expect_err("a local asset-only override must not fall through to the registry");
    assert!(error.to_string().contains("asset-only"), "{error:#}");
    Ok(())
}

#[test]
fn direct_component_scan_distinguishes_asset_only_and_invalid_driver_shapes() -> anyhow::Result<()>
{
    let project = locked_project_root()?;
    let fixture = project.path().join("components/fixture");
    let package = |bins: &[&str], has_library| crate::source::train::WorkspaceComponentCrate {
        manifest_path: fixture.join("Cargo.toml").canonicalize().unwrap(),
        crate_dir: fixture.canonicalize().unwrap(),
        binary_names: bins.iter().map(|name| (*name).to_string()).collect(),
        has_library,
    };
    std::fs::remove_file(fixture.join("Cargo.toml"))?;
    std::fs::remove_dir_all(fixture.join("src"))?;
    let discovered = discover_local_components_from_locked(project.path(), &[])?;
    assert!(discovered["fixture"].driver_crate.is_none());

    std::fs::create_dir_all(fixture.join("src"))?;
    std::fs::write(fixture.join("src/lib.rs"), "")?;
    let error = discover_local_components_from_locked(project.path(), &[])
        .expect_err("assets with source but no Cargo.toml are invalid");
    assert!(error.to_string().contains("asset-only"), "{error:#}");

    std::fs::write(
        fixture.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\npath = \"src/lib.rs\"\n",
    )?;
    let error = discover_local_components_from_locked(project.path(), &[])
        .expect_err("lib-only component driver is invalid");
    assert!(
        error.to_string().contains("obsolete anchor/assets crate"),
        "{error:#}"
    );

    std::fs::write(fixture.join("src/main.rs"), "fn main() {}")?;
    std::fs::create_dir_all(fixture.join("src/bin"))?;
    std::fs::write(fixture.join("src/bin/extra.rs"), "fn main() {}")?;
    let error = discover_local_components_from_locked(
        project.path(),
        &[package(&["fixture", "extra"], false)],
    )
    .expect_err("a driver must have exactly one binary and no library");
    assert!(
        error.to_string().contains("exactly one binary"),
        "{error:#}"
    );

    std::fs::remove_file(fixture.join("src/bin/extra.rs"))?;

    std::fs::write(
        fixture.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\npath = \"src/lib.rs\"\ncrate-type = [\"cdylib\"]\n\n[[bin]]\nname = \"fixture\"\npath = \"src/main.rs\"\n",
    )?;
    let error =
        discover_local_components_from_locked(project.path(), &[package(&["fixture"], true)])
            .expect_err("mixed library and driver targets are invalid");
    assert!(
        error.to_string().contains("must not define a library"),
        "{error:#}"
    );

    std::fs::remove_file(fixture.join("src/lib.rs"))?;
    std::fs::write(
        fixture.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[[bin]]\nname = \"fixture\"\npath = \"src/main.rs\"\n",
    )?;
    let missing = project.path().join("components/missing");
    std::fs::create_dir_all(&missing)?;
    let error =
        discover_local_components_from_locked(project.path(), &[package(&["fixture"], false)])
            .expect_err("every direct component needs component.yaml");
    assert!(
        error.to_string().contains("missing component.yaml"),
        "{error:#}"
    );
    std::fs::remove_dir_all(&missing)?;

    let nonmember = project.path().join("components/nonmember");
    std::fs::create_dir_all(nonmember.join("src"))?;
    std::fs::write(
        nonmember.join("component.yaml"),
        "schema: phoxal/component/v0\n",
    )?;
    std::fs::write(
        nonmember.join("Cargo.toml"),
        "[package]\nname = \"nonmember\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[[bin]]\nname = \"nonmember\"\npath = \"src/main.rs\"\n",
    )?;
    std::fs::write(nonmember.join("src/main.rs"), "fn main() {}")?;
    let error =
        discover_local_components_from_locked(project.path(), &[package(&["fixture"], false)])
            .expect_err("a local driver must join a locked workspace");
    assert!(error.to_string().contains("workspace.members"), "{error:#}");
    Ok(())
}

#[test]
fn direct_component_driver_requires_the_root_locked_workspace() -> anyhow::Result<()> {
    let project = locked_project_root()?;
    std::fs::remove_file(project.path().join("Cargo.lock"))?;
    let error = discover_local_components(project.path(), false)
        .expect_err("member driver needs the root lock");
    assert!(
        error.to_string().contains("missing committed Cargo.lock"),
        "{error:#}"
    );

    let standalone = tempfile::tempdir()?;
    let component = standalone.path().join("components/standalone");
    std::fs::create_dir_all(component.join("src"))?;
    std::fs::write(
        component.join("component.yaml"),
        "schema: phoxal/component/v0\n",
    )?;
    std::fs::write(
        component.join("Cargo.toml"),
        "[package]\nname = \"standalone\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n\n[[bin]]\nname = \"standalone\"\npath = \"src/main.rs\"\n",
    )?;
    std::fs::write(component.join("src/main.rs"), "fn main() {}")?;
    let error = discover_local_components(standalone.path(), true)
        .expect_err("standalone drivers are not container-buildable");
    assert!(error.to_string().contains("root Cargo.toml"), "{error:#}");
    std::fs::write(component.join("Cargo.lock"), "version = 4\n")?;
    let error = discover_local_components(standalone.path(), true)
        .expect_err("a standalone lock must not restore standalone driver support");
    assert!(error.to_string().contains("root Cargo.toml"), "{error:#}");
    Ok(())
}

#[test]
fn registry_component_resolution_fetches_distinct_ids_once_and_keeps_excluded_driver_assets()
-> anyhow::Result<()> {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use sha2::Digest;

    struct Http {
        responses: BTreeMap<String, Vec<u8>>,
        downloads: AtomicUsize,
    }
    impl crate::registry_package::RegistryHttp for Http {
        fn get(&self, url: &str) -> anyhow::Result<Vec<u8>> {
            if url.contains("download.invalid") {
                self.downloads.fetch_add(1, Ordering::SeqCst);
            }
            self.responses
                .get(url)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unexpected fake URL {url}"))
        }
    }
    fn archive(package: &str, version: &str) -> anyhow::Result<Vec<u8>> {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        let manifest = format!(
            "[package]\nname = {package:?}\nversion = {version:?}\n\n[[bin]]\nname = {package:?}\npath = \"src/main.rs\"\n"
        );
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut tar = tar::Builder::new(encoder);
        for (path, bytes) in [
            ("Cargo.toml", manifest.as_bytes()),
            ("src/main.rs", b"fn main() {}" as &[u8]),
            ("component.yaml", b"schema: phoxal/component/v0\ncapabilities:\n  motor:\n    kind: motor\n    command: velocity\n    target:\n      kind: joint\n      id: wheel_joint\n" as &[u8]),
            ("structure.urdf", br#"<robot name="component"><link name="base"/><link name="wheel"/><joint name="wheel_joint" type="continuous"><parent link="base"/><child link="wheel"/></joint></robot>"# as &[u8]),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, format!("{package}-{version}/{path}"), bytes)?;
        }
        Ok(tar.into_inner()?.finish()?)
    }

    let version = "0.1.0";
    let base = "https://phoxal.github.io/registry";
    let mut responses = BTreeMap::from([(
        format!("{base}/config.json"),
        br#"{"dl":"https://download.invalid/{lowerprefix}/{crate}/{version}.crate"}"#.to_vec(),
    )]);
    for id in ["left", "right"] {
        let package = phoxal_cli_catalog::cargo_package_name(&format!("phoxal/component-{id}"));
        let bytes = archive(&package, version)?;
        let checksum = hex::encode(sha2::Sha256::digest(&bytes));
        let index = crate::registry_package::index_path(&package)?;
        responses.insert(
            format!("{base}/{index}"),
            format!(r#"{{"vers":"{version}","cksum":"{checksum}"}}"#).into_bytes(),
        );
        let prefix = index.rsplit_once('/').unwrap().0;
        responses.insert(
            format!("https://download.invalid/{prefix}/{package}/{version}.crate"),
            bytes,
        );
    }
    let http = Http {
        responses,
        downloads: AtomicUsize::new(0),
    };
    let cache_root = tempfile::tempdir()?;
    let cache = crate::registry_package::PackageCache::new(cache_root.path().to_path_buf());
    let ids = BTreeSet::from(["left".to_string(), "right".to_string()]);
    let roots = resolve_registry_component_roots(&ids, &http, &cache, version, false)?;
    assert_eq!(roots.len(), 2);
    assert_eq!(http.downloads.load(Ordering::SeqCst), 2);
    let repeated = resolve_registry_component_roots(&ids, &http, &cache, version, false)?;
    assert_eq!(repeated, roots);
    assert_eq!(http.downloads.load(Ordering::SeqCst), 2);

    let project = locked_project_root()?;
    let project_cache =
        crate::registry_package::PackageCache::new(project.path().join(".phoxal/cache/registry"));
    let one_id = BTreeSet::from(["left".to_string()]);
    resolve_registry_component_roots(&one_id, &http, &project_cache, version, false)?;
    let robot = minimal_robot_with_components(
        r#"
    left_drive:
      component: left
      mount_link: base
      driver:
        connection:
          type: serial
          port: /dev/ttyUSB0
          baud: 115200
"#,
        "",
    )?;
    crate::source::resolver::write_robot_to_dir(&robot, project.path())?;
    std::fs::write(
        project.path().join("structure.urdf"),
        r#"<robot name="fixture"><link name="base_footprint"/><link name="base_link"/><link name="base"/><joint name="root" type="fixed"><parent link="base_footprint"/><child link="base_link"/></joint><joint name="base_mount" type="fixed"><parent link="base_link"/><child link="base"/></joint></robot>"#,
    )?;
    let resolved = resolve(
        &robot,
        project.path(),
        ResolveOptions {
            offline: true,
            drivers: crate::source::intent::DriverSelection::None,
            ..Default::default()
        },
    )?;
    let excluded = &resolved.components[0];
    assert!(excluded.assets_root.join("component.yaml").is_file());
    assert!(excluded.driver.is_none());
    assert!(
        resolved.source_manifest.robot.components["left_drive"]
            .driver
            .is_some()
    );
    Ok(())
}

/// The driver policy gates resolution itself (): an excluded driver
/// instance resolves no driver package at all, so nothing downstream ever
/// requires, builds, or installs a binary for it. Authored intent remains on
/// `source_manifest` for reporting.
#[test]
fn an_excluded_driver_resolves_no_driver_package() -> anyhow::Result<()> {
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
    std::fs::write(
        crate_dir.join("component.yaml"),
        "schema: phoxal/component/v0\n",
    )?;
    declare_workspace_member(project.path(), "components/ddsm115")?;
    write_lock(project.path(), &["ddsm115"])?;

    let resolved = resolve_fixture(
        &robot,
        project.path(),
        ResolveOptions {
            drivers: crate::source::intent::DriverSelection::None,
            ..ResolveOptions::default()
        },
    )?;
    let component = &resolved.components[0];
    assert!(
        resolved.source_manifest.robot.components["left_drive"]
            .driver
            .is_some(),
        "declared intent is preserved"
    );
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
