//! Finalized-bundle candidate construction and validation.
//!
//! The live `.phoxal/bundle/` is replaced only after every canonical input and
//! every binary has passed validation, so staging always builds into an
//! adjacent `.phoxal/.bundle-candidate-<unique>` directory first and publishes
//! it with one atomic rename. A build that fails halfway through must never
//! leave a robot with no runtime.
//!
//! The candidate is exactly the shape the framework's finalized-bundle loader
//! reads:
//!
//! ```text
//! robot.yaml                              finalized robot/v0
//! assets/robot/structure.urdf
//! assets/robot/meshes/...
//! assets/components/<type>/component.yaml
//! assets/components/<type>/structure.urdf
//! assets/components/<type>/simulation.yaml   when the component has one
//! assets/components/<type>/meshes/...
//! assets/router/config.json5                 when the robot declares one
//! bin/...
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use phoxal_cli_core::project::intent::RunIntent;
use phoxal_cli_core::project::layout::{ASSETS_DIR, BIN_DIR, ROBOT_FILE, ROUTER_CONFIG_ASSET};
use phoxal_cli_core::project::resolver::BundlePlan;

use super::finalize::{ROBOT_STRUCTURE_ASSET, finalize_manifest, render_finalized};

/// The frozen files a component definition contributes to a bundle, beside its
/// meshes. `simulation.yaml` is optional; the other two are not.
const COMPONENT_DEFINITION_FILES: [(&str, bool); 3] = [
    ("component.yaml", true),
    ("structure.urdf", true),
    ("simulation.yaml", false),
];

/// An unpublished bundle candidate.
pub(crate) struct StagedCandidate {
    pub(super) dir: tempfile::TempDir,
    pub(super) project_root: PathBuf,
}

impl StagedCandidate {
    #[must_use]
    pub(crate) fn path(&self) -> &Path {
        self.dir.path()
    }
}

pub(crate) fn begin_runtime_layout(
    project_root: &Path,
    resolved: &BundlePlan,
    intent: &RunIntent,
) -> Result<StagedCandidate> {
    let live =
        project_root.join(phoxal_cli_core::project::launch_plan::RUNTIME_BUNDLE_ROOT_RELATIVE);
    let parent = live
        .parent()
        .context("runtime bundle directory has no parent")?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create runtime layout directory {}",
            parent.display()
        )
    })?;
    // The candidate is a sibling of the live bundle so publication is a rename
    // on one filesystem, never a copy.
    let candidate = tempfile::Builder::new()
        .prefix(".bundle-candidate-")
        .tempdir_in(parent)
        .with_context(|| {
            format!(
                "failed to create runtime layout candidate in {}",
                parent.display()
            )
        })?;

    stage_candidate(project_root, candidate.path(), resolved, intent)?;

    Ok(StagedCandidate {
        dir: candidate,
        project_root: project_root.to_path_buf(),
    })
}

fn stage_candidate(
    project_root: &Path,
    candidate: &Path,
    resolved: &BundlePlan,
    intent: &RunIntent,
) -> Result<()> {
    let finalized = finalize_manifest(&resolved.source_manifest, intent)?;
    fs::write(candidate.join(ROBOT_FILE), render_finalized(&finalized)?)
        .context("failed to write the finalized robot document")?;

    let asset_root = candidate.join(ASSETS_DIR);
    fs::create_dir_all(&asset_root)
        .with_context(|| format!("failed to create {}", asset_root.display()))?;

    // The canonical model's logical assets: robot and component meshes.
    for (id, bytes) in &resolved.compiled.assets {
        write_asset(&asset_root, id.as_str(), bytes)?;
    }

    // The robot structure, at its deterministic bundle-relative location.
    let structure = project_root.join(&resolved.source_manifest.robot.structure);
    let structure_bytes = fs::read(&structure)
        .with_context(|| format!("failed to read robot structure {}", structure.display()))?;
    write_asset(&asset_root, ROBOT_STRUCTURE_ASSET, &structure_bytes)?;

    // The frozen component definitions, keyed by component type so the loader
    // derives their roots deterministically.
    for component in &resolved.components {
        let destination = format!("components/{}", component.source_name);
        for (file, required) in COMPONENT_DEFINITION_FILES {
            let source = component.assets_root.join(file);
            if !source.is_file() {
                ensure!(
                    !required,
                    "component type '{}' is missing {file} at {}",
                    component.source_name,
                    component.assets_root.display()
                );
                continue;
            }
            let bytes = fs::read(&source)
                .with_context(|| format!("failed to read {}", source.display()))?;
            write_asset(&asset_root, &format!("{destination}/{file}"), &bytes)?;
        }
    }

    if let Some(source) = &resolved.source_manifest.router.config {
        ensure!(
            !source.as_os_str().is_empty()
                && source
                    .components()
                    .all(|component| matches!(component, std::path::Component::Normal(_))),
            "router.config must be a non-empty relative path without '.' or '..': {}",
            source.display()
        );
        let source = project_root.join(source);
        let bytes = fs::read(&source)
            .with_context(|| format!("failed to read router config {}", source.display()))?;
        write_asset(&asset_root, ROUTER_CONFIG_ASSET, &bytes)?;
    }

    fs::create_dir_all(candidate.join(BIN_DIR))
        .context("failed to create the bundle bin directory")?;
    Ok(())
}

fn write_asset(root: &Path, id: &str, bytes: &[u8]) -> Result<()> {
    validate_asset_path(id)?;
    let path = root.join(id);
    let parent = path.parent().context("bundle asset has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    fs::write(&path, bytes).with_context(|| format!("failed to write {}", path.display()))
}

fn validate_asset_path(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && Path::new(id)
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_))),
        "bundle asset '{id}' must contain only normal relative path components"
    );
    Ok(())
}

/// Copy a validated staged tree into a build destination.
pub(crate) fn copy_tree_into(source: &Path, dest: &Path) -> Result<()> {
    copy_dir_recursive(source, dest)
}

fn copy_dir_recursive(source: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("failed to create {}", dest.display()))?;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry?;
        let source_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "bundle content must not contain symlinks: {}",
            source_path.display()
        );
        if metadata.is_dir() {
            copy_dir_recursive(&source_path, &dest_path)?;
        } else if metadata.is_file() {
            fs::copy(&source_path, &dest_path).with_context(|| {
                format!(
                    "failed to stage runtime asset {} to {}",
                    source_path.display(),
                    dest_path.display()
                )
            })?;
        }
    }
    Ok(())
}

/// Materialize a complete finalized bundle at `root` from an authored
/// `robot.yaml` body, so tests exercise the real loader against real staged
/// files rather than a hand-assembled directory.
///
/// Every component type the document uses gets a minimal frozen definition;
/// `simulated_types` additionally get a `simulation.yaml`, which is what makes a
/// `clock: simulated` fixture coherent.
#[cfg(test)]
pub(crate) fn write_test_bundle(
    root: &Path,
    robot_yaml: &str,
    intent: &RunIntent,
    simulated_types: &[&str],
) -> Result<()> {
    let source = tempfile::tempdir()?;
    let phoxal_manifest::source::robot::Manifest::V0(authored) =
        phoxal_manifest::source::robot::parse_from_string(robot_yaml)?;
    fs::write(source.path().join("robot.yaml"), robot_yaml)?;
    let structure = source.path().join(&authored.robot.structure);
    if let Some(parent) = structure.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        &structure,
        r#"<robot name="fixture"><link name="base_footprint"/><link name="base_link"/><link name="base"/><joint name="root" type="fixed"><parent link="base_footprint"/><child link="base_link"/></joint><joint name="base_mount" type="fixed"><parent link="base_link"/><child link="base"/></joint></robot>"#,
    )?;
    let mut component_roots = std::collections::BTreeMap::new();
    for component_type in authored.used_component_types() {
        let component_root = source.path().join("components").join(component_type);
        fs::create_dir_all(&component_root)?;
        fs::write(
            component_root.join("component.yaml"),
            "schema: component/v0\ncapabilities:\n  motor:\n    kind: motor\n    command: velocity\n    target:\n      kind: joint\n      id: wheel_joint\n",
        )?;
        fs::write(
            component_root.join("structure.urdf"),
            r#"<robot name="component"><link name="base"/><link name="wheel"/><joint name="wheel_joint" type="continuous"><parent link="base"/><child link="wheel"/></joint></robot>"#,
        )?;
        if simulated_types.contains(&component_type) {
            fs::write(
                component_root.join("simulation.yaml"),
                "schema: simulation/v0\ncapabilities: {}\n",
            )?;
        }
        component_roots.insert(component_type.to_string(), component_root);
    }
    let compiled = phoxal_cli_core::project::resolver::CompiledBundle::from_project(
        phoxal_manifest::compile(phoxal_manifest::SourceSet {
            project_root: source.path().to_path_buf(),
            robot_manifest: source.path().join("robot.yaml"),
            component_roots: component_roots.clone(),
        })?,
    );
    let resolved = BundlePlan {
        source_manifest: authored,
        compiled,
        train: "0.54.0".to_string(),
        target: crate::resolve::project::host_target_triple(),
        brain: phoxal_cli_core::project::resolver::ResolvedBrain {
            crate_dir: source.path().to_path_buf(),
            package: "fixture-robot".to_string(),
            bin_target: "fixture-robot".to_string(),
        },
        platform_runtimes: Vec::new(),
        simulators: Vec::new(),
        user_runtimes: Vec::new(),
        undeclared_runtimes: Vec::new(),
        components: component_roots
            .iter()
            .map(|(component_type, assets_root)| {
                phoxal_cli_core::project::resolver::ResolvedComponent {
                    instance: component_type.clone(),
                    source_name: component_type.clone(),
                    assets_root: assets_root.clone(),
                    driver: None,
                }
            })
            .collect(),
        path_overrides: Vec::new(),
    };
    fs::create_dir_all(root)?;
    stage_candidate(source.path(), root, &resolved, intent)
}

/// Compile a minimal real project into a [`CompiledBundle`], for fixtures that
/// need a plausible compiler output without staging a whole bundle.
#[cfg(test)]
pub(crate) fn compile_test_bundle(
    source_manifest: &phoxal_manifest::source::robot::v0::Manifest,
) -> Result<phoxal_cli_core::project::resolver::CompiledBundle> {
    let source = tempfile::tempdir()?;
    let component = source.path().join("components/wheel");
    fs::create_dir_all(&component)?;
    fs::write(
        source.path().join("robot.yaml"),
        r#"schema: robot/v0
robot:
  id: testbot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  structure: structure.urdf
  kinematic:
    kind: omnidirectional
    actuators:
      - wheel.motor
    encoders: []
  components:
    wheel:
      component: wheel
      mount_link: base_link
"#,
    )?;
    fs::write(
        source.path().join("structure.urdf"),
        r#"<robot name="testbot"><link name="base_footprint"/><link name="base_link"/><joint name="root" type="fixed"><parent link="base_footprint"/><child link="base_link"/></joint></robot>"#,
    )?;
    fs::write(
        component.join("component.yaml"),
        "schema: component/v0\ncapabilities:\n  motor:\n    kind: motor\n    command: velocity\n    target:\n      kind: joint\n      id: wheel_joint\n",
    )?;
    fs::write(
        component.join("structure.urdf"),
        r#"<robot name="wheel"><link name="base"/><link name="wheel"/><joint name="wheel_joint" type="continuous"><parent link="base"/><child link="wheel"/></joint></robot>"#,
    )?;
    let mut bundle = phoxal_cli_core::project::resolver::CompiledBundle::from_project(
        phoxal_manifest::compile(phoxal_manifest::SourceSet {
            project_root: source.path().to_path_buf(),
            robot_manifest: source.path().join("robot.yaml"),
            component_roots: std::collections::BTreeMap::from([("wheel".to_string(), component)]),
        })?,
    );
    bundle.participants = source_manifest
        .services
        .iter()
        .map(|(id, service)| phoxal_manifest::Participant {
            id: id.clone(),
            kind: phoxal_manifest::ParticipantKind::Service,
            component_instance: None,
            config: service.config.clone(),
        })
        .collect();
    Ok(bundle)
}
