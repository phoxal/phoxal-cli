//! Disposable project-local Webots project generation.
//!
//! Every `simulation webots run` removes `.phoxal/webots` and recreates the
//! complete `worlds|controllers|protos` project before launching Webots.

//! Every path below is derived from the project root the caller passes in.
//! Nothing here reads an environment variable: the root is a fact the command
//! resolved, not a process global two call sites could disagree about
//! (organization#978).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The generated Webots project root: `<project>/.phoxal/webots`.
#[must_use]
pub fn root(project_root: &Path) -> PathBuf {
    crate::paths::runtime::RuntimePaths::for_root(project_root)
        .state_root
        .join("webots")
}

/// Where generated robot/component PROTOs are written.
#[must_use]
pub fn protos_dir(project_root: &Path) -> PathBuf {
    root(project_root).join("protos")
}

/// Where generated PROTO-owned mesh assets are written.
#[must_use]
pub fn meshes_dir(project_root: &Path) -> PathBuf {
    protos_dir(project_root).join("meshes")
}

/// Where the generated world is written.
#[must_use]
pub fn worlds_dir(project_root: &Path) -> PathBuf {
    root(project_root).join("worlds")
}

/// The generated path of the world named `world_name`.
#[must_use]
pub fn world_path(project_root: &Path, world_name: &str) -> PathBuf {
    worlds_dir(project_root).join(format!("{world_name}.wbt"))
}

/// The generated controller directory. Webots expects the executable to be
/// named exactly `<name>` inside `controllers/<name>/`.
#[must_use]
pub fn controller_dir(project_root: &Path, controller_name: &str) -> PathBuf {
    root(project_root).join("controllers").join(controller_name)
}

/// Delete any previous Webots project and recreate its standard directories.
pub fn wipe_and_recreate(project_root: &Path) -> Result<PathBuf> {
    let root = root(project_root);
    if root.exists() {
        std::fs::remove_dir_all(&root).with_context(|| {
            format!(
                "failed to remove the previous Webots project at {}",
                root.display()
            )
        })?;
    }
    for directory in ["worlds", "controllers", "protos"] {
        let path = root.join(directory);
        std::fs::create_dir_all(&path).with_context(|| {
            format!(
                "failed to create Webots project directory {}",
                path.display()
            )
        })?;
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recreates_the_complete_disposable_webots_project() -> Result<()> {
        let project = tempfile::tempdir()?;
        let previous = root(project.path());
        std::fs::create_dir_all(previous.join("worlds"))?;
        std::fs::write(previous.join("worlds/old-state"), b"old Webots state")?;
        std::fs::create_dir_all(previous.join("custom"))?;

        let recreated = wipe_and_recreate(project.path())?;

        assert!(!recreated.join("worlds/old-state").exists());
        assert!(!recreated.join("custom").exists());
        for directory in ["worlds", "controllers", "protos"] {
            assert!(recreated.join(directory).is_dir());
        }
        assert_eq!(std::fs::read_dir(recreated)?.count(), 3);
        Ok(())
    }
}
