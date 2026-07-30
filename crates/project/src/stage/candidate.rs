//! Candidate construction and asset validation before publication.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, ensure};
use phoxal_cli_core::project::resolver::ResolvedRobot;

const BEHAVIORS_DIR: &str = "behaviors";
const MESHES_DIR: &str = "meshes";
const COMPONENT_FILE: &str = "component.yaml";
const COMPONENT_OPTIONAL_FILES: [&str; 2] = ["structure.urdf", "simulation.yaml"];

/// An unpublished runtime-layout candidate: the compiled `robot.yaml` and
/// runtime assets already staged (and manifest-shape validated), but NOT yet
/// swapped into the live `.phoxal/bundle/`.
///
/// This is the type that makes the stager's atomicity promise real: every
/// caller MUST materialize officials
/// ([`materialize_official_store`](super::participants::materialize_official_store)),
/// build/stage source and override binaries, run its source/loader
/// validation, and only THEN call
/// [`publish_runtime_layout`](super::publish::publish_runtime_layout). A failure at
/// any point before publish touches only this candidate's own temporary
/// directory - the previous live bundle is untouched and still runnable.
/// [`StagedCandidate::path`] is a plain filesystem path, so every existing
/// staging function (`materialize_official_store`, `stage_named_binary`,
/// `crate::load::layout::validate_layout_plan`, ...) that takes `staged_root: &Path`
/// already works against it with no changes.
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

/// Stage the compiled `robot.yaml` and runtime assets into a fresh,
/// UNPUBLISHED candidate directory - a sibling of the live `.phoxal/bundle/`,
/// never the live path itself. The caller owns the project run lock for the
/// whole operation (candidate creation through
/// [`publish_runtime_layout`](super::publish::publish_runtime_layout)),
/// so no participant observes anything until the single atomic rename at the
/// end. `bin/` is created empty; the caller populates it (typically with
/// [`materialize_official_store`](super::participants::materialize_official_store)
/// and [`stage_participant_binary`](super::participants::stage_participant_binary))
/// and runs
/// its own validation against [`StagedCandidate::path`] BEFORE publishing -
/// see the module docs.
pub(crate) fn begin_runtime_layout(
    project_root: &Path,
    resolved: &ResolvedRobot,
) -> Result<StagedCandidate> {
    let build_dir =
        project_root.join(phoxal_cli_core::project::launch_plan::RUNTIME_BUNDLE_ROOT_RELATIVE);
    let parent = build_dir
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

    let compiled = compile_manifest(resolved);
    stage_candidate(project_root, candidate.path(), resolved, &compiled)?;
    validate_candidate(candidate.path(), resolved, &compiled)?;

    Ok(StagedCandidate {
        dir: candidate,
        project_root: project_root.to_path_buf(),
    })
}

/// The compiled `robot/v0` manifest for the staged layout. Under the
/// declaration model (#950) the authored `services:` and `tools:` maps are
/// already complete - they select which discovered workspace runtimes belong
/// to the robot - so compilation carries them verbatim (the `extends:` chain
/// was already flattened by the framework loader); nothing is injected from
/// discovery.
fn compile_manifest(resolved: &ResolvedRobot) -> phoxal::model::source::robot::v0::Manifest {
    resolved.robot.clone()
}

fn stage_candidate(
    project_root: &Path,
    candidate: &Path,
    resolved: &ResolvedRobot,
    compiled: &phoxal::model::source::robot::v0::Manifest,
) -> Result<()> {
    phoxal::model::source::robot::write_to_dir(compiled, candidate)
        .context("failed to write compiled runtime robot.yaml")?;
    crate::load::header::RuntimeHeader::for_phoxal_version(&resolved.train)
        .write_to(candidate)
        .context("failed to write compiled runtime compatibility header")?;

    let structure = &resolved.robot.robot.structure;
    ensure_safe_relative_path(structure, "robot structure")?;
    copy_file_preserving_path(project_root, candidate, structure, "robot structure")?;
    if let Some(structure_parent) = structure.parent() {
        let mesh_path = structure_parent.join(MESHES_DIR);
        copy_optional_dir_preserving_path(project_root, candidate, &mesh_path)?;
    }
    copy_optional_dir_preserving_path(project_root, candidate, Path::new(BEHAVIORS_DIR))?;

    // The router's optional Zenoh config file is a real runtime asset: stage it
    // into the layout under its runtime-root-relative path so a `build.phoxal`
    // extracted anywhere resolves the same relative path a source run does
    // (#936, finding 4). It must be a safe relative path with no escapes, exactly
    // like the robot structure.
    if let Some(router_config) = &resolved.robot.router.config {
        ensure_safe_relative_path(router_config, "router.config")?;
        copy_file_preserving_path(project_root, candidate, router_config, "router.config")?;
    }

    stage_component_bundles(candidate, resolved)
        .context("failed to stage component assets into the runtime layout")
}

/// Copy every distinct component's asset bundle (`component.yaml`, the
/// optional `structure.urdf`/`simulation.yaml`, and `meshes/`) into the
/// staged layout, from whichever on-disk directory resolution already
/// settled for it - a workspace crate directory or a registry package's
/// extraction directory `cargo metadata` reported. Both sources are read the
/// same way: resolution is the only place that knows which is which.
fn stage_component_bundles(candidate: &Path, resolved: &ResolvedRobot) -> Result<()> {
    let mut staged = std::collections::BTreeSet::new();
    for component in &resolved.components {
        let component_id = &component.source_name;
        if !staged.insert(component_id.clone()) {
            continue;
        }
        let source_dir = component
            .assets
            .path_override()
            .with_context(|| format!("failed to locate component assets for '{component_id}'"))?;
        // Schema gate every referenced component document before it is
        // staged: verify the declared `component/vX` revision this CLI
        // supports, then strict-parse it, so an unknown field or unsupported
        // revision fails here - naming the exact file - instead of being
        // copied through silently (#936, finding 5).
        gate_component_document(source_dir, component_id)?;
        let dest_dir = candidate.join("components").join(component_id);
        if source_dir == dest_dir {
            continue;
        }
        copy_component_bundle_files(source_dir, &dest_dir)?;
    }
    Ok(())
}

fn gate_component_document(source_dir: &Path, component_id: &str) -> Result<()> {
    let component_file = source_dir.join(COMPONENT_FILE);
    phoxal_cli_core::schema::ensure_supported_revision(
        &component_file,
        phoxal_cli_core::schema::DocumentKind::Component,
    )?;
    phoxal::model::source::component::read_from_dir(source_dir).with_context(|| {
        format!(
            "component '{component_id}' failed strict parsing of {}",
            component_file.display()
        )
    })?;
    Ok(())
}

fn copy_component_bundle_files(source_dir: &Path, dest_dir: &Path) -> Result<()> {
    fs::create_dir_all(dest_dir)
        .with_context(|| format!("failed to create {}", dest_dir.display()))?;

    let component_file = source_dir.join(COMPONENT_FILE);
    fs::copy(&component_file, dest_dir.join(COMPONENT_FILE)).with_context(|| {
        format!(
            "failed to stage component metadata {} to {}",
            component_file.display(),
            dest_dir.display()
        )
    })?;

    for optional_file in COMPONENT_OPTIONAL_FILES {
        let source_file = source_dir.join(optional_file);
        if !source_file.is_file() {
            continue;
        }
        fs::copy(&source_file, dest_dir.join(optional_file)).with_context(|| {
            format!(
                "failed to stage {} to {}",
                source_file.display(),
                dest_dir.display()
            )
        })?;
    }

    let meshes_source = source_dir.join(MESHES_DIR);
    if meshes_source.is_dir() {
        copy_dir_recursive(&meshes_source, &dest_dir.join(MESHES_DIR))?;
    }
    Ok(())
}

fn ensure_safe_relative_path(path: &Path, label: &str) -> Result<()> {
    ensure!(
        !path.as_os_str().is_empty()
            && path
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "{label} must be a non-empty relative path without '.' or '..': {}",
        path.display()
    );
    Ok(())
}

fn validate_candidate(
    candidate: &Path,
    resolved: &ResolvedRobot,
    compiled: &phoxal::model::source::robot::v0::Manifest,
) -> Result<()> {
    // Resolution already ran the model's semantic validation. Reparse the
    // serialized candidate here to prove the on-disk manifest is complete
    // and strict without losing that owner-specific validation context.
    let staged = phoxal::model::source::robot::parse_from_dir(candidate)
        .context("compiled runtime robot.yaml failed strict parsing")?;
    ensure!(
        &staged == compiled,
        "compiled runtime robot.yaml differs from the resolved manifest"
    );
    ensure!(
        candidate.join(&resolved.robot.robot.structure).is_file(),
        "compiled runtime layout is missing robot structure {}",
        resolved.robot.robot.structure.display()
    );

    for component in &resolved.components {
        {
            let component_file = candidate
                .join("components")
                .join(&component.source_name)
                .join("component.yaml");
            ensure!(
                component_file.is_file(),
                "compiled runtime layout is missing component metadata {}",
                component_file.display()
            );
        }
    }
    Ok(())
}

fn copy_file_preserving_path(
    project_root: &Path,
    candidate: &Path,
    relative: &Path,
    label: &str,
) -> Result<()> {
    let source = project_root.join(relative);
    let dest = candidate.join(relative);
    let parent = dest
        .parent()
        .context("runtime layout destination has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    fs::copy(&source, &dest).with_context(|| {
        format!(
            "failed to stage {label} {} to {}",
            source.display(),
            dest.display()
        )
    })?;
    Ok(())
}

fn copy_optional_dir_preserving_path(
    project_root: &Path,
    candidate: &Path,
    relative: &Path,
) -> Result<()> {
    let source = project_root.join(relative);
    if !source.is_dir() {
        return Ok(());
    }
    copy_dir_recursive(&source, &candidate.join(relative))
}

/// Copy a directory tree into `dest` (files and directories; permissions
/// preserved by `fs::copy`). Shared with `phoxal build`'s container path, which
/// publishes the snapshot-staged layout into the real project (#936).
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
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_recursive(&source_path, &dest_path)?;
        } else if file_type.is_file() {
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
