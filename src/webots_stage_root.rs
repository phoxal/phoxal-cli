//! Disposable project-local Webots project generation.
//!
//! Every `simulation webots run` removes `.phoxal/webots` and recreates the
//! complete `worlds|controllers|protos` project before launching Webots.

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::host_paths;

/// The generated Webots project root: `<project>/.phoxal/webots`.
pub fn root() -> Result<PathBuf> {
    host_paths::webots_dir()
}

/// Where generated robot/component PROTOs are written.
pub fn protos_dir() -> Result<PathBuf> {
    Ok(root()?.join("protos"))
}

/// Where generated PROTO-owned mesh assets are written.
pub fn meshes_dir() -> Result<PathBuf> {
    Ok(protos_dir()?.join("meshes"))
}

/// Where the generated world is written.
pub fn worlds_dir() -> Result<PathBuf> {
    Ok(root()?.join("worlds"))
}

/// The generated path of the world named `world_name`.
pub fn world_path(world_name: &str) -> Result<PathBuf> {
    Ok(worlds_dir()?.join(format!("{world_name}.wbt")))
}

/// The generated controller directory. Webots expects the executable to be
/// named exactly `<name>` inside `controllers/<name>/`.
pub fn controller_dir(controller_name: &str) -> Result<PathBuf> {
    Ok(root()?.join("controllers").join(controller_name))
}

/// Delete any previous Webots project and recreate its standard directories.
pub fn wipe_and_recreate() -> Result<PathBuf> {
    let root = root()?;
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
    use crate::host_paths::test_support::ScratchPhoxalHome;

    #[test]
    fn recreates_the_complete_disposable_webots_project() -> Result<()> {
        let _home = ScratchPhoxalHome::new()?;
        let previous = root()?;
        std::fs::create_dir_all(previous.join("worlds"))?;
        std::fs::write(previous.join("worlds/old-state"), b"old Webots state")?;
        std::fs::create_dir_all(previous.join("custom"))?;

        let recreated = wipe_and_recreate()?;

        assert!(!recreated.join("worlds/old-state").exists());
        assert!(!recreated.join("custom").exists());
        for directory in ["worlds", "controllers", "protos"] {
            assert!(recreated.join(directory).is_dir());
        }
        assert_eq!(std::fs::read_dir(recreated)?.count(), 3);
        Ok(())
    }
}
