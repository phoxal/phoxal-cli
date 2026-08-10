//! Webots controller build environment and bundle-owned provisioning.

use std::ffi::OsString;
use std::path::Path;

use anyhow::{Context, Result};

use crate::simulation::webots::root;

/// Link the verified simulator artifact from the runtime bundle into Webots'
/// required controller directory.
pub(crate) fn stage_bundled_controller(project_root: &Path, binary: &Path) -> Result<()> {
    anyhow::ensure!(
        binary.is_file(),
        "simulator controller is missing at {}",
        binary.display()
    );
    let name = crate::simulation::prepare::WEBOTS_CONTROLLER_NAME;
    let staged_dir = root::controller_dir(project_root, name);
    std::fs::create_dir_all(&staged_dir)
        .with_context(|| format!("failed to create {}", staged_dir.display()))?;
    symlink_controller(binary, &staged_dir.join(name))
}

fn symlink_controller(source: &Path, destination: &Path) -> Result<()> {
    if destination.symlink_metadata().is_ok() {
        std::fs::remove_file(destination)
            .with_context(|| format!("failed to replace {}", destination.display()))?;
    }
    std::os::unix::fs::symlink(source, destination).with_context(|| {
        format!(
            "failed to symlink controller {} -> {}",
            destination.display(),
            source.display()
        )
    })?;
    Ok(())
}

/// Scope the build-only `WEBOTS_HOME` input to the simulator source build.
pub(crate) struct WebotsHomeEnvGuard {
    previous: Option<OsString>,
}

impl WebotsHomeEnvGuard {
    pub(crate) fn set(home: &Path) -> Self {
        let previous = std::env::var_os("WEBOTS_HOME");
        // SAFETY: simulation preparation is synchronous for the guard's life.
        unsafe { std::env::set_var("WEBOTS_HOME", home) };
        Self { previous }
    }
}

impl Drop for WebotsHomeEnvGuard {
    fn drop(&mut self) {
        // SAFETY: restore the exact process state captured by `set`.
        unsafe {
            if let Some(previous) = &self.previous {
                std::env::set_var("WEBOTS_HOME", previous);
            } else {
                std::env::remove_var("WEBOTS_HOME");
            }
        }
    }
}
