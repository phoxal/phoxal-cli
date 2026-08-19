//! CLI-owned installation paths around the framework rendezvous contract.

use std::path::{Path, PathBuf};

use phoxal::supervisor::rendezvous::RuntimeRendezvous;

/// The single CLI executable distributed by this repository.
pub const CLIENT_BINARY: &str = "phoxal";
pub const ACTIVE_RUNTIME_ROOT: &str = "/var/phoxal";
pub const INSTALL_ROOT: &str = "/var/lib/phoxal";
pub const RELEASES_ROOT: &str = "/var/lib/phoxal/releases";
pub const INSTALLED_STATE_ROOT: &str = "/var/lib/phoxal/state";
pub const INSTALLED_VOLATILE_ROOT: &str = "/run/phoxal";
pub const INSTALLED_BINARY_ROOT: &str = "/usr/local/bin";
pub const INSTALLED_CLIENT_BINARY: &str = "/usr/local/bin/phoxal";
pub const SYSTEMD_UNIT: &str = "phoxal-supervisor.service";
pub const SYSTEMD_ACTIVE_ROOT: &str = "/run/systemd/system";
pub const SYSTEMD_UNIT_ROOT: &str = "/etc/systemd/system";
pub const SYSTEMD_UNIT_PATH: &str = "/etc/systemd/system/phoxal-supervisor.service";

/// The directory the running CLI executable lives in.
pub fn executable_directory() -> anyhow::Result<PathBuf> {
    use anyhow::Context;

    let current = std::env::current_exe().context("failed to locate the running executable")?;
    let current = current.canonicalize().unwrap_or(current);
    current
        .parent()
        .map(Path::to_path_buf)
        .context("the running executable has no parent directory")
}

/// CLI-private paths plus the framework-owned supervisor rendezvous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePaths {
    rendezvous: RuntimeRendezvous,
    pub state_root: PathBuf,
}

impl RuntimePaths {
    #[must_use]
    pub fn for_root(root: &Path) -> Self {
        Self {
            rendezvous: RuntimeRendezvous::for_root(root),
            state_root: if is_installed_root(root) {
                PathBuf::from(INSTALLED_STATE_ROOT)
            } else {
                root.join(".phoxal")
            },
        }
    }

    #[must_use]
    pub fn ownership_root(&self) -> &Path {
        self.rendezvous.ownership_root()
    }

    #[must_use]
    pub fn volatile_root(&self) -> &Path {
        self.rendezvous.volatile_root()
    }

    #[must_use]
    pub fn build_lock(&self) -> PathBuf {
        self.rendezvous.volatile_root().join("build.lock")
    }

    #[must_use]
    pub fn supervisor_lock(&self) -> PathBuf {
        self.rendezvous.supervisor_lock()
    }

    /// Everything one startup printed, plus the dependency output it did not.
    /// Truncated by each new startup: it describes the current attempt.
    #[must_use]
    pub fn startup_log(&self) -> PathBuf {
        self.rendezvous.volatile_root().join("startup.log")
    }

    /// The launched supervisor's own standard error. The supervisor is durable
    /// and outlives the client that started it, so it owns this file directly
    /// rather than writing down a pipe nobody is left to read.
    #[must_use]
    pub fn supervisor_log(&self) -> PathBuf {
        self.rendezvous.volatile_root().join("supervisor.log")
    }

    /// The Webots application's own output. It must never reach the terminal:
    /// Webots prints driver notices over whatever is on screen, and during a
    /// session that is the dashboard.
    #[must_use]
    pub fn webots_log(&self) -> PathBuf {
        self.rendezvous.volatile_root().join("webots.log")
    }

    #[must_use]
    pub fn supervisor_socket(&self) -> PathBuf {
        self.rendezvous.supervisor_socket()
    }

    pub fn checked_supervisor_socket(&self) -> anyhow::Result<PathBuf> {
        Ok(self.rendezvous.checked_supervisor_socket()?)
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
    fn cli_paths_extend_the_framework_rendezvous_without_redefining_it() {
        let source = RuntimePaths::for_root(Path::new("/tmp/robot"));
        assert_eq!(source.ownership_root(), Path::new("/tmp/robot"));
        assert_eq!(source.volatile_root(), Path::new("/tmp/robot/.phoxal/run"));
        assert_eq!(source.state_root, Path::new("/tmp/robot/.phoxal"));
        assert_eq!(
            source.build_lock(),
            Path::new("/tmp/robot/.phoxal/run/build.lock")
        );
        assert_eq!(
            source.supervisor_lock(),
            Path::new("/tmp/robot/.phoxal/run/supervisor.lock")
        );
        assert_eq!(
            source.supervisor_socket(),
            Path::new("/tmp/robot/.phoxal/run/supervisor.sock")
        );
        // Every per-session artifact shares the one volatile root, so a
        // project only ever has one place to look and one place to clean.
        assert_eq!(
            source.startup_log(),
            Path::new("/tmp/robot/.phoxal/run/startup.log")
        );
        assert_eq!(
            source.supervisor_log(),
            Path::new("/tmp/robot/.phoxal/run/supervisor.log")
        );
        assert_eq!(
            source.webots_log(),
            Path::new("/tmp/robot/.phoxal/run/webots.log")
        );

        let installed = RuntimePaths::for_root(Path::new(
            "/var/lib/phoxal/releases/20260814T010203.000Z-deadbeef",
        ));
        assert_eq!(installed.ownership_root(), Path::new(ACTIVE_RUNTIME_ROOT));
        assert_eq!(
            installed.volatile_root(),
            Path::new(INSTALLED_VOLATILE_ROOT)
        );
        assert_eq!(installed.state_root, Path::new(INSTALLED_STATE_ROOT));
    }

    /// The unit's name and the path it is installed at are one fact, and the
    /// install, the generated units, and every `systemctl` call read both.
    #[test]
    fn the_managed_unit_name_and_its_installed_path_agree() {
        assert_eq!(SYSTEMD_UNIT, "phoxal-supervisor.service");
        assert_eq!(
            SYSTEMD_UNIT_PATH,
            format!("{SYSTEMD_UNIT_ROOT}/{SYSTEMD_UNIT}")
        );
    }
}
