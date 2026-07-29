//! One path policy for source projects and the installed robot runtime.

use std::path::{Path, PathBuf};

pub use phoxal_cli_core::runtime::paths::{
    ACTIVE_RUNTIME_ROOT, INSTALL_ROOT, INSTALLED_STATE_ROOT, INSTALLED_VOLATILE_ROOT,
    RELEASES_ROOT, RuntimePaths, SYSTEMD_ACTIVE_ROOT, SYSTEMD_UNIT, SYSTEMD_UNIT_PATH,
    SYSTEMD_UNIT_ROOT, is_installed_root,
};

/// Resolve `/var/phoxal` once after its run lock is held and require its direct
/// target to be one immutable release directory.
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
    let target = if target.is_absolute() {
        target
    } else {
        root.parent().unwrap_or_else(|| Path::new("/")).join(target)
    };
    let pinned = target.canonicalize()?;
    let releases = Path::new(RELEASES_ROOT).canonicalize()?;
    anyhow::ensure!(
        pinned.parent() == Some(releases.as_path()),
        "{} points outside the immutable release directory: {}",
        root.display(),
        pinned.display()
    );
    Ok(pinned)
}
