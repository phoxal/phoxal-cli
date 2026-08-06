//! Canonical runtime-bundle candidate construction and validation.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use phoxal_cli_core::project::layout::{
    ASSETS_DIR, PARTICIPANTS_ASSET, ROBOT_FILE, ROUTER_CONFIG_ASSET, RUNTIME_HEADER_ASSET,
};
use phoxal_cli_core::project::resolver::{BundlePlan, CompiledBundle};

/// An unpublished bundle candidate. The live `.phoxal/bundle/` is replaced
/// only after binaries and every canonical input have passed validation.
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
    let candidate = tempfile::Builder::new()
        .prefix(".bundle-candidate-")
        .tempdir_in(parent)
        .with_context(|| {
            format!(
                "failed to create runtime layout candidate in {}",
                parent.display()
            )
        })?;

    stage_candidate(project_root, candidate.path(), resolved, &resolved.compiled)?;
    validate_candidate(candidate.path(), &resolved.compiled)?;

    Ok(StagedCandidate {
        dir: candidate,
        project_root: project_root.to_path_buf(),
    })
}

fn stage_candidate(
    project_root: &Path,
    candidate: &Path,
    resolved: &BundlePlan,
    compiled: &CompiledBundle,
) -> Result<()> {
    ensure!(
        !compiled.robot.is_empty(),
        "resolved bundle plan has no canonical compiler output"
    );
    for reserved in [
        PARTICIPANTS_ASSET,
        ROUTER_CONFIG_ASSET,
        RUNTIME_HEADER_ASSET,
    ] {
        ensure!(
            !compiled
                .assets
                .keys()
                .any(|asset| asset.as_str() == reserved),
            "compiled asset '{reserved}' collides with CLI-owned bundle metadata"
        );
    }
    fs::write(candidate.join(ROBOT_FILE), &compiled.robot)
        .context("failed to write canonical robot.json")?;
    crate::load::header::RuntimeHeader::for_phoxal_version(&resolved.train)
        .write_to(candidate)
        .context("failed to write runtime compatibility header")?;

    let asset_root = candidate.join(ASSETS_DIR);
    fs::create_dir_all(&asset_root)
        .with_context(|| format!("failed to create {}", asset_root.display()))?;
    for (id, bytes) in &compiled.assets {
        write_asset(&asset_root, id.as_str(), bytes)?;
    }
    let participants =
        phoxal_cli_core::project::layout::encode_participants(&compiled.participants)?;
    write_asset(&asset_root, PARTICIPANTS_ASSET, &participants)?;

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
    fs::create_dir_all(candidate.join("bin"))
        .context("failed to create canonical bundle bin directory")?;
    Ok(())
}

fn write_asset(root: &Path, id: &str, bytes: &[u8]) -> Result<()> {
    let id = phoxal_model::AssetId::new(id.to_string())
        .context("compiled asset has an invalid logical id")?;
    validate_asset_path(id.as_str())?;
    let path = root.join(id.as_str());
    let parent = path.parent().context("compiled asset has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    fs::write(&path, bytes).with_context(|| format!("failed to write {}", path.display()))
}

fn validate_asset_path(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && Path::new(id)
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_))),
        "compiled asset '{id}' must contain only normal relative path components"
    );
    Ok(())
}

fn validate_candidate(candidate: &Path, compiled: &CompiledBundle) -> Result<()> {
    let bytes = fs::read(candidate.join(ROBOT_FILE)).context("staged robot.json is missing")?;
    ensure!(
        bytes == compiled.robot,
        "staged robot.json differs from the compiler output"
    );
    let robot =
        phoxal_model::Robot::decode(&bytes).context("staged robot.json failed canonical decode")?;
    let asset_root = candidate.join(ASSETS_DIR);
    let canonical_asset_root = asset_root
        .canonicalize()
        .context("failed to resolve staged asset root")?;
    let declared = compiled
        .assets
        .keys()
        .map(phoxal_model::AssetId::as_str)
        .collect::<BTreeSet<_>>();
    let mut referenced = robot.structure().asset_ids().collect::<Vec<_>>();
    for instance in robot.components() {
        let component = robot
            .component_for_instance(instance.id())
            .with_context(|| {
                format!(
                    "canonical robot component instance '{}' has no component definition",
                    instance.id()
                )
            })?;
        referenced.extend(component.structure().asset_ids());
    }
    for id in referenced {
        ensure!(
            declared.contains(id.as_str()),
            "canonical robot references undeclared asset '{}'",
            id.as_str()
        );
        validate_asset_path(id.as_str())?;
        let path = asset_root.join(id.as_str());
        let metadata = fs::symlink_metadata(&path).with_context(|| {
            format!(
                "canonical robot asset '{}' is missing below assets/",
                id.as_str()
            )
        })?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "canonical robot asset '{}' must be a regular file below assets/",
            id.as_str()
        );
        let canonical = path
            .canonicalize()
            .with_context(|| format!("failed to resolve staged asset '{}'", id.as_str()))?;
        ensure!(
            canonical.starts_with(&canonical_asset_root),
            "canonical robot asset '{}' escapes the staged asset root",
            id.as_str()
        );
    }
    phoxal_cli_core::project::layout::decode_participants(&asset_root.join(PARTICIPANTS_ASSET))?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn compile_test_bundle(
    source_manifest: &phoxal_manifest::source::robot::v0::Manifest,
) -> Result<CompiledBundle> {
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
    let compiled = phoxal_manifest::compile(phoxal_manifest::SourceSet {
        project_root: source.path().to_path_buf(),
        robot_manifest: source.path().join("robot.yaml"),
        component_roots: std::collections::BTreeMap::from([("wheel".to_string(), component)]),
    })?;
    let mut bundle = CompiledBundle::from_project(compiled)?;
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

#[cfg(test)]
pub(crate) fn write_test_layout(root: &Path, robot_yaml: &str) -> Result<()> {
    let source_manifest = phoxal_cli_core::project::resolver::parse_robot_from_string(robot_yaml)?;
    let compiled = compile_test_bundle(&source_manifest)?;
    fs::create_dir_all(root.join(ASSETS_DIR))?;
    fs::create_dir_all(root.join("bin"))?;
    fs::write(root.join(ROBOT_FILE), &compiled.robot)?;
    fs::write(
        root.join(ASSETS_DIR).join(PARTICIPANTS_ASSET),
        phoxal_cli_core::project::layout::encode_participants(&compiled.participants)?,
    )?;
    for (id, bytes) in &compiled.assets {
        write_asset(&root.join(ASSETS_DIR), id.as_str(), bytes)?;
    }
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
