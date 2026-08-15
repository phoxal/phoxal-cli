//! One path policy for source projects and the installed robot runtime.

use std::path::{Path, PathBuf};

/// A source project's own deployment release, holding the supervisor and the
/// bundle this project's last staging produced. It is a directory of its own so
/// the release stays exactly its two entries, with all local runtime state
/// (locks, sockets, caches, archives) beside it under `.phoxal/` rather than
/// inside it.
pub(crate) const RUNTIME_RELEASE_ROOT_RELATIVE: &str = ".phoxal/release";

#[must_use]
pub(crate) fn runtime_release_root(project_root: &Path) -> PathBuf {
    project_root.join(RUNTIME_RELEASE_ROOT_RELATIVE)
}

pub use phoxal_cli_host::paths::{
    ACTIVE_RUNTIME_ROOT, INSTALL_ROOT, INSTALLED_BINARY_ROOT, INSTALLED_CLIENT_BINARY,
    INSTALLED_STATE_ROOT, INSTALLED_VOLATILE_ROOT, RELEASES_ROOT, RuntimePaths,
    SYSTEMD_ACTIVE_ROOT, SYSTEMD_UNIT, SYSTEMD_UNIT_PATH, SYSTEMD_UNIT_ROOT, is_installed_root,
};

/// Resolve `/var/phoxal` once after its run lock is held, so the rest of the
/// execution reads one immutable release directory instead of a symlink an
/// install may swap underneath it.
pub fn pin_installed_release(root: &Path) -> anyhow::Result<PathBuf> {
    if root != Path::new(ACTIVE_RUNTIME_ROOT) {
        return Ok(root.to_path_buf());
    }
    let metadata = std::fs::symlink_metadata(root)
        .map_err(|error| anyhow::anyhow!("failed to inspect {}: {error}", root.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_symlink(),
        "{} must be a symlink to one immutable release",
        root.display()
    );
    let target = std::fs::read_link(root)?;
    Ok(if target.is_absolute() {
        target
    } else {
        root.parent().unwrap_or_else(|| Path::new("/")).join(target)
    })
}
