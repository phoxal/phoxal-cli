use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::host_paths;
use crate::resolver::{ResolvedComponentSource, ResolvedRobot};
use crate::shell;
use crate::utils::{copy_dir_recursive, resolve_project_path};

const COMPONENT_FILE: &str = "component.yaml";
const STRUCTURE_FILE: &str = "structure.urdf";
const SIMULATION_FILE: &str = "simulation.yaml";
const DRIVER_DIR: &str = "driver";

pub fn assemble(project_root: &Path, resolved: &ResolvedRobot, out_dir: &Path) -> Result<()> {
    if out_dir.exists() {
        fs::remove_dir_all(out_dir)
            .with_context(|| format!("failed to clear run view {}", out_dir.display()))?;
    }
    fs::create_dir_all(out_dir)
        .with_context(|| format!("failed to create run view {}", out_dir.display()))?;

    let robot_yaml =
        serde_yaml::to_string(&phoxal::model::robot::Robot::V1(resolved.robot.clone()))
            .context("failed to serialize robot")?;
    fs::write(out_dir.join("robot.yaml"), robot_yaml)
        .with_context(|| format!("failed to write {}", out_dir.join("robot.yaml").display()))?;

    copy_file(
        &project_root.join(&resolved.robot.structure),
        &out_dir.join(STRUCTURE_FILE),
    )?;

    let components_dir = out_dir.join("components");
    fs::create_dir_all(&components_dir)
        .with_context(|| format!("failed to create {}", components_dir.display()))?;

    for component in &resolved.components {
        let source_dir = match &component.source {
            ResolvedComponentSource::Git {
                git,
                commit,
                directory,
                ..
            } => component_source_dir(ensure_repo_cache(git, commit)?, directory.as_deref())?,
            ResolvedComponentSource::Path { path } => resolve_project_path(project_root, path),
        };
        let destination = components_dir.join(&component.source_name);
        copy_component_definition(&source_dir, &destination)?;
    }

    Ok(())
}

/// Clone a component repository at a pinned commit into the host cache,
/// keyed by `(repository, commit)` so a shared catalog repository hosting
/// many components is cloned once and reused across every component that
/// references it.
fn ensure_repo_cache(git: &str, commit: &str) -> Result<PathBuf> {
    let cache_dir = host_paths::cache_dir()?
        .join("components")
        .join(format!("{}-{commit}", repo_slug(git)));
    if cache_dir.join(".git").is_dir() {
        return Ok(cache_dir);
    }
    let parent = cache_dir
        .parent()
        .context("component cache path did not have a parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create component cache {}", parent.display()))?;
    let staging = tempfile::TempDir::new_in(parent)
        .with_context(|| format!("failed to create staging cache in {}", parent.display()))?;
    let cache_arg = staging.path().to_string_lossy().to_string();
    shell::run_status(
        "git",
        ["clone", "--depth", "1", git, cache_arg.as_str()],
        None,
    )?;
    shell::run_status("git", ["checkout", commit], Some(staging.path()))?;
    match fs::rename(staging.path(), &cache_dir) {
        Ok(()) => {}
        Err(error) if cache_dir.join(".git").is_dir() => {
            tracing::debug!(
                "component cache {} appeared during clone; keeping existing destination ({error})",
                cache_dir.display()
            );
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to move component cache to {}", cache_dir.display())
            });
        }
    }
    Ok(cache_dir)
}

/// Resolve the component definition directory inside a cached repository,
/// applying the optional `directory` subdirectory selector. Guards against
/// absolute paths and `..` traversal so a manifest cannot read outside the
/// cloned repository.
fn component_source_dir(repo_root: PathBuf, directory: Option<&Path>) -> Result<PathBuf> {
    let Some(directory) = directory else {
        return Ok(repo_root);
    };
    for part in directory.components() {
        if !matches!(part, std::path::Component::Normal(_)) {
            bail!(
                "component source directory {} must be a relative path without '..'",
                directory.display()
            );
        }
    }
    Ok(repo_root.join(directory))
}

/// Filesystem-safe slug for a git remote URL, so distinct repositories never
/// share a cache directory.
fn repo_slug(git: &str) -> String {
    let trimmed = git.strip_suffix(".git").unwrap_or(git);
    trimmed
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

fn copy_component_definition(source_dir: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    for file_name in [COMPONENT_FILE, STRUCTURE_FILE, SIMULATION_FILE] {
        let source = source_dir.join(file_name);
        if source.is_file() {
            copy_file(&source, &destination.join(file_name))?;
        } else if file_name != SIMULATION_FILE {
            bail!(
                "component source {} is missing required {}",
                source_dir.display(),
                file_name
            );
        }
    }
    let driver_dir = source_dir.join(DRIVER_DIR);
    if driver_dir.is_dir() {
        copy_dir_recursive(&driver_dir, &destination.join(DRIVER_DIR))?;
    }
    Ok(())
}

fn copy_file(source: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::copy(source, destination).with_context(|| {
        format!(
            "failed to copy {} to {}",
            source.display(),
            destination.display()
        )
    })?;
    Ok(())
}
