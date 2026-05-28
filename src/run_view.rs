use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::resolver::{ResolvedComponentSource, ResolvedRobot};
use crate::shell;
use crate::utils::copy_dir_recursive;

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

    let robot_yaml = serde_yaml::to_string(&phoxal_robot::Robot::V1(resolved.robot.clone()))
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
            ResolvedComponentSource::Git { git, commit, .. } => {
                ensure_component_cache(project_root, &component.source_name, git, commit)?
            }
            ResolvedComponentSource::Path { path } => resolve_project_path(project_root, path),
        };
        let destination = components_dir.join(&component.source_name);
        copy_component_definition(&source_dir, &destination)?;
    }

    Ok(())
}

fn ensure_component_cache(
    project_root: &Path,
    source_name: &str,
    git: &str,
    commit: &str,
) -> Result<PathBuf> {
    let cache_dir = project_root
        .join(".phoxal")
        .join("cache")
        .join("components")
        .join(format!("{source_name}-{commit}"));
    if cache_dir.join(COMPONENT_FILE).is_file() {
        return Ok(cache_dir);
    }
    if let Some(parent) = cache_dir.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create component cache {}", parent.display()))?;
    }
    if cache_dir.exists() {
        fs::remove_dir_all(&cache_dir)
            .with_context(|| format!("failed to clear {}", cache_dir.display()))?;
    }
    let cache_arg = cache_dir.to_string_lossy().to_string();
    shell::run_status(
        "git",
        ["clone", "--depth", "1", git, cache_arg.as_str()],
        None,
    )?;
    shell::run_status("git", ["checkout", commit], Some(&cache_dir))?;
    Ok(cache_dir)
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

fn resolve_project_path(project_root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    }
}
