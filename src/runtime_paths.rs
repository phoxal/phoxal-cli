//! One path policy for source projects and the installed robot runtime.

use std::path::{Path, PathBuf};

pub const ACTIVE_RUNTIME_ROOT: &str = "/var/phoxal";
pub const INSTALL_ROOT: &str = "/var/lib/phoxal";
pub const RELEASES_ROOT: &str = "/var/lib/phoxal/releases";
pub const INSTALLED_STATE_ROOT: &str = "/var/lib/phoxal/state";
pub const INSTALLED_VOLATILE_ROOT: &str = "/run/phoxal";
pub const SYSTEMD_UNIT: &str = "phoxal.service";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePaths {
    pub ownership_root: PathBuf,
    pub state_root: PathBuf,
    pub volatile_root: PathBuf,
}

impl RuntimePaths {
    #[must_use]
    pub fn for_root(root: &Path) -> Self {
        if is_installed_root(root) {
            Self {
                ownership_root: PathBuf::from(ACTIVE_RUNTIME_ROOT),
                state_root: PathBuf::from(INSTALLED_STATE_ROOT),
                volatile_root: PathBuf::from(INSTALLED_VOLATILE_ROOT),
            }
        } else {
            let state = root.join(".phoxal");
            Self {
                ownership_root: root.to_path_buf(),
                state_root: state.clone(),
                volatile_root: state,
            }
        }
    }

    #[must_use]
    pub fn project_lock(&self) -> PathBuf {
        self.state_root.join("project.lock")
    }

    #[must_use]
    pub fn supervisor_socket(&self) -> PathBuf {
        self.volatile_root.join("supervisor.sock")
    }

    #[must_use]
    pub fn router_socket(&self) -> PathBuf {
        self.volatile_root.join("zenoh.sock")
    }

    #[must_use]
    pub fn supervisor_log(&self) -> PathBuf {
        self.state_root.join("supervisor.log")
    }

    #[must_use]
    pub fn plan_content_root(&self) -> PathBuf {
        self.state_root.join("plans").join("content")
    }
}

#[must_use]
pub fn is_installed_root(root: &Path) -> bool {
    root == Path::new(ACTIVE_RUNTIME_ROOT) || root.starts_with(RELEASES_ROOT)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_roots_keep_state_and_sockets_project_local() {
        let paths = RuntimePaths::for_root(Path::new("/tmp/robot"));
        assert_eq!(paths.ownership_root, Path::new("/tmp/robot"));
        assert_eq!(paths.state_root, Path::new("/tmp/robot/.phoxal"));
        assert_eq!(paths.volatile_root, paths.state_root);
    }

    #[test]
    fn active_and_pinned_installed_roots_share_stable_fhs_paths() {
        for root in [
            Path::new(ACTIVE_RUNTIME_ROOT),
            Path::new("/var/lib/phoxal/releases/20260725T012345.678Z-deadbeef"),
        ] {
            let paths = RuntimePaths::for_root(root);
            assert_eq!(paths.ownership_root, Path::new(ACTIVE_RUNTIME_ROOT));
            assert_eq!(paths.state_root, Path::new(INSTALLED_STATE_ROOT));
            assert_eq!(paths.volatile_root, Path::new(INSTALLED_VOLATILE_ROOT));
        }
    }
}
