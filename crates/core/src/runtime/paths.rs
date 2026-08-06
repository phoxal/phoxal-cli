//! Pure runtime path policy shared by project preparation and supervision.

use std::path::{Path, PathBuf};

pub const ACTIVE_RUNTIME_ROOT: &str = "/var/phoxal";
pub const INSTALL_ROOT: &str = "/var/lib/phoxal";
pub const RELEASES_ROOT: &str = "/var/lib/phoxal/releases";
pub const INSTALLED_STATE_ROOT: &str = "/var/lib/phoxal/state";
pub const INSTALLED_VOLATILE_ROOT: &str = "/run/phoxal";
/// Where the exact `phoxal` + `phoxald` pair is installed on a managed host.
/// The verified release archive carries both binaries and they are placed here
/// together (organization#978), so the unit and the client resolve the daemon
/// from one place rather than from `PATH`.
pub const INSTALLED_BINARY_ROOT: &str = "/usr/local/bin";
/// The installed interactive client. It is never the daemon.
pub const INSTALLED_CLIENT_BINARY: &str = "/usr/local/bin/phoxal";
/// The installed supervisor. This is what `phoxal.service` executes.
pub const INSTALLED_DAEMON_BINARY: &str = "/usr/local/bin/phoxald";
pub const SYSTEMD_UNIT: &str = "phoxal.service";
pub const SYSTEMD_ACTIVE_ROOT: &str = "/run/systemd/system";
pub const SYSTEMD_UNIT_ROOT: &str = "/etc/systemd/system";
pub const SYSTEMD_UNIT_PATH: &str = "/etc/systemd/system/phoxal.service";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePaths {
    installed: bool,
    pub ownership_root: PathBuf,
    pub state_root: PathBuf,
    pub volatile_root: PathBuf,
}

impl RuntimePaths {
    #[must_use]
    pub fn for_root(root: &Path) -> Self {
        if is_installed_root(root) {
            Self {
                installed: true,
                ownership_root: PathBuf::from(ACTIVE_RUNTIME_ROOT),
                state_root: PathBuf::from(INSTALLED_STATE_ROOT),
                volatile_root: PathBuf::from(INSTALLED_VOLATILE_ROOT),
            }
        } else {
            let state = root.join(".phoxal");
            Self {
                installed: false,
                ownership_root: root.to_path_buf(),
                state_root: state.clone(),
                volatile_root: state.join("run"),
            }
        }
    }

    /// The lock every bundle-mutating command takes. A live execution holds
    /// [`Self::supervisor_lock`] for its whole lifetime, so a build can never
    /// replace a running daemon's files.
    #[must_use]
    pub fn build_lock(&self) -> PathBuf {
        self.volatile_root.join("build.lock")
    }

    /// The lock one `phoxald` holds for its complete lifetime. Its presence
    /// under an exclusive holder - not the socket's existence - is what
    /// "an execution is live" means.
    #[must_use]
    pub fn supervisor_lock(&self) -> PathBuf {
        self.volatile_root.join("supervisor.lock")
    }

    #[must_use]
    pub fn supervisor_socket(&self) -> PathBuf {
        self.volatile_root.join("supervisor.sock")
    }

    #[must_use]
    pub fn supervisor_log(&self) -> PathBuf {
        if self.installed {
            self.state_root.join("supervisor.log")
        } else {
            self.volatile_root.join("supervisor.log")
        }
    }
}

#[must_use]
pub fn is_installed_root(root: &Path) -> bool {
    root == Path::new(ACTIVE_RUNTIME_ROOT) || root.starts_with(RELEASES_ROOT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_roots_keep_state_but_group_transient_files_under_run() {
        let paths = RuntimePaths::for_root(Path::new("/tmp/robot"));
        assert_eq!(paths.ownership_root, Path::new("/tmp/robot"));
        assert_eq!(paths.state_root, Path::new("/tmp/robot/.phoxal"));
        assert_eq!(paths.volatile_root, Path::new("/tmp/robot/.phoxal/run"));
        assert_eq!(
            paths.build_lock(),
            Path::new("/tmp/robot/.phoxal/run/build.lock")
        );
        assert_eq!(
            paths.supervisor_lock(),
            Path::new("/tmp/robot/.phoxal/run/supervisor.lock")
        );
        assert_eq!(
            paths.supervisor_socket(),
            Path::new("/tmp/robot/.phoxal/run/supervisor.sock")
        );
        assert_eq!(
            paths.supervisor_log(),
            Path::new("/tmp/robot/.phoxal/run/supervisor.log")
        );
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
            assert_eq!(
                paths.supervisor_log(),
                Path::new(INSTALLED_STATE_ROOT).join("supervisor.log")
            );
        }
    }
}
