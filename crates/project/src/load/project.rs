use anyhow::{Context, Result};
use phoxal_cli_core::project::resolver::{discover_robot_yaml, load_robot};
use std::path::{Path, PathBuf};

pub(crate) struct LoadedProject {
    pub(crate) root: PathBuf,
    pub(crate) robot_path: PathBuf,
    pub(crate) robot: phoxal_manifest::source::robot::v0::Manifest,
}

pub(crate) fn load(start: &Path) -> Result<LoadedProject> {
    let robot_path = discover_robot_yaml(start)
        .with_context(|| format!("failed to find robot.yaml from {}", start.display()))?;
    let root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?
        .to_path_buf();
    let robot = load_robot(&robot_path)?;
    Ok(LoadedProject {
        root,
        robot_path,
        robot,
    })
}
