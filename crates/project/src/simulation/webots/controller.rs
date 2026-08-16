//! Webots controller build environment and bundle-owned provisioning.

use std::ffi::OsString;
use std::path::Path;

use anyhow::{Context, Result};

use crate::simulation::webots::root;

/// Copy the materialized controller into Webots' required controller
/// directory.
///
/// A copy, never a link: Webots patches the controller binary *in place* when
/// it launches it. On macOS it runs `install_name_tool -add_rpath` to point the
/// executable at `/Applications/Webots.app/` and then re-signs the Mach-O ad
/// hoc. Through a symlink that edit would land on the CLI's cached tool, which
/// every later simulation and every other project shares. Webots may rewrite
/// its own copy as much as it likes; the cache stays what `cargo install`
/// produced.
pub(crate) fn stage_controller(project_root: &Path, binary: &Path) -> Result<()> {
    anyhow::ensure!(
        binary.is_file(),
        "the Webots controller is missing at {}",
        binary.display()
    );
    let name = phoxal_cli_catalog::WEBOTS_CONTROLLER_PACKAGE;
    let staged_dir = root::controller_dir(project_root, name);
    std::fs::create_dir_all(&staged_dir)
        .with_context(|| format!("failed to create {}", staged_dir.display()))?;
    copy_controller(binary, &staged_dir.join(name))
}

fn copy_controller(source: &Path, destination: &Path) -> Result<()> {
    // Remove rather than overwrite: the destination may be a symlink an older
    // CLI staged, and copying onto that would write straight through it into
    // the bundle - the exact corruption this function exists to prevent.
    if destination.symlink_metadata().is_ok() {
        std::fs::remove_file(destination)
            .with_context(|| format!("failed to replace {}", destination.display()))?;
    }
    std::fs::copy(source, destination).with_context(|| {
        format!(
            "failed to copy controller {} -> {}",
            source.display(),
            destination.display()
        )
    })?;
    // `fs::copy` preserves the mode on Unix, but Webots refuses a controller it
    // cannot execute, so the one bit that matters is asserted rather than
    // assumed.
    ensure_executable(destination)
}

#[cfg(unix)]
fn ensure_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = std::fs::metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?
        .permissions();
    let mode = permissions.mode();
    if mode & 0o111 == 0o111 {
        return Ok(());
    }
    permissions.set_mode(mode | 0o111);
    std::fs::set_permissions(path, permissions)
        .with_context(|| format!("failed to make {} executable", path.display()))
}

#[cfg(not(unix))]
const fn ensure_executable(_path: &Path) -> Result<()> {
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Webots rewrites the controller it launches, so what it launches must be
    /// this staging directory's own file - never a link into the shared tool
    /// cache - and a link left by an older CLI must be replaced by one.
    #[test]
    fn staging_produces_a_regular_executable_file_and_replaces_a_stale_link() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let bundled = directory.path().join("bin/controller");
        std::fs::create_dir_all(bundled.parent().context("the bin directory")?)?;
        std::fs::write(&bundled, b"original cached bytes")?;
        std::fs::set_permissions(&bundled, std::fs::Permissions::from_mode(0o755))?;

        let staged = directory.path().join("staged/controller");
        std::fs::create_dir_all(staged.parent().context("the staging directory")?)?;
        std::os::unix::fs::symlink(&bundled, &staged)?;

        copy_controller(&bundled, &staged)?;

        let metadata = std::fs::symlink_metadata(&staged)?;
        assert!(
            metadata.file_type().is_file(),
            "the staged controller must be a real file, not a link into the cache"
        );
        assert!(metadata.permissions().mode() & 0o111 != 0, "not executable");

        // What Webots does next must not reach the cached tool.
        std::fs::write(&staged, b"patched by webots")?;
        assert_eq!(std::fs::read(&bundled)?, b"original cached bytes");
        Ok(())
    }

    /// A source without the executable bit is still staged as an executable:
    /// Webots refuses a controller it cannot run.
    #[test]
    fn a_non_executable_source_is_staged_executable() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("controller");
        std::fs::write(&source, b"binary")?;
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o644))?;
        let staged = directory.path().join("staged-controller");

        copy_controller(&source, &staged)?;
        assert!(std::fs::metadata(&staged)?.permissions().mode() & 0o111 != 0);
        Ok(())
    }
}
