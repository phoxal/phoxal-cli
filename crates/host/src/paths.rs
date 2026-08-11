//! Runtime path policy shared by project preparation and supervision.

use std::path::{Path, PathBuf};

/// The daemon executable's file name.
///
/// It is one name in three places that must agree: the installed pair beside
/// the client, the executor a deployment release packages, and the published
/// release archive's per-target member.
pub const DAEMON_BINARY: &str = "phoxald";
/// The interactive client's file name. It is never the daemon.
pub const CLIENT_BINARY: &str = "phoxal";

pub const ACTIVE_RUNTIME_ROOT: &str = "/var/phoxal";
pub const INSTALL_ROOT: &str = "/var/lib/phoxal";
pub const RELEASES_ROOT: &str = "/var/lib/phoxal/releases";
pub const INSTALLED_STATE_ROOT: &str = "/var/lib/phoxal/state";
pub const INSTALLED_VOLATILE_ROOT: &str = "/run/phoxal";
/// Where the exact `phoxal` + `phoxald` pair is installed on a managed host.
/// The verified release archive carries both binaries and they are placed here
/// together, so the unit and the client resolve the daemon
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

/// The directory the running executable lives in.
///
/// # Errors
///
/// When the platform cannot report the running executable, or it has no parent
/// directory.
pub fn executable_directory() -> anyhow::Result<PathBuf> {
    use anyhow::Context;

    let current = std::env::current_exe().context("failed to locate the running executable")?;
    let current = current.canonicalize().unwrap_or(current);
    current
        .parent()
        .map(Path::to_path_buf)
        .context("the running executable has no parent directory")
}

/// Resolve the `phoxald` beside the running executable.
///
/// `PATH` is the fallback for the one case where the running executable cannot
/// be resolved at all; every other case answers with the sibling, present or
/// not, so a missing sibling is reported as a broken installation rather than
/// papered over by an unrelated daemon on `PATH`.
#[must_use]
pub fn sibling_daemon() -> PathBuf {
    match executable_directory() {
        Ok(directory) => directory.join(DAEMON_BINARY),
        Err(_) => PathBuf::from(DAEMON_BINARY),
    }
}

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

    /// The supervisor socket, rejected up front when the platform cannot bind
    /// it.
    ///
    /// Unix-socket paths are bounded by `sun_path`; a project nested deeply
    /// enough exceeds it and the daemon would fail at bind(2) with a transport
    /// error long after a build. Both the client (before building) and the
    /// daemon (before opening the router) resolve through this check so the
    /// operator gets the actionable refusal instead.
    ///
    /// # Errors
    ///
    /// Names the byte length, the platform maximum, and the fix (a shorter
    /// project path).
    pub fn checked_supervisor_socket(&self) -> anyhow::Result<PathBuf> {
        let path = self.supervisor_socket();
        let bytes = std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str());
        // The bindable maximum is the `sun_path` array itself minus its NUL
        // terminator, computed from the field offset because macOS carries an
        // extra `sun_len` byte before the family field.
        let maximum = std::mem::size_of::<libc::sockaddr_un>()
            - std::mem::offset_of!(libc::sockaddr_un, sun_path)
            - 1;
        if bytes.len() > maximum {
            anyhow::bail!(
                "the supervisor socket path is {} bytes but this platform supports at most \
                 {maximum}: {}; move the project to a shorter path",
                bytes.len(),
                path.display()
            );
        }
        Ok(path)
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

    /// Boundary proof that the precheck agrees with the platform: the longest
    /// path the precheck accepts must actually bind, and one byte more must be
    /// refused by the precheck rather than by a raw bind(2) later.
    #[test]
    fn socket_precheck_boundary_matches_a_real_bind() {
        let suffix = "/.phoxal/run/supervisor.sock";
        let scratch = tempfile::tempdir().expect("scratch dir");
        let base = scratch.path().to_str().expect("utf-8 scratch path");

        let socket_for = |total: usize| {
            let pad = total - base.len() - 1 - suffix.len();
            let root = std::path::PathBuf::from(format!("{base}/{}", "p".repeat(pad)));
            RuntimePaths::for_root(&root).checked_supervisor_socket()
        };
        let accepted_max = (60..160)
            .rev()
            .find(|total| socket_for(*total).is_ok())
            .expect("some socket path length must be accepted");

        let over = socket_for(accepted_max + 1)
            .expect_err("one byte past the maximum must fail the precheck");
        assert!(over.to_string().contains("shorter"));

        let path = socket_for(accepted_max).expect("precheck accepts the maximum");
        std::fs::create_dir_all(path.parent().expect("socket parent")).expect("create run dir");
        let listener = std::os::unix::net::UnixListener::bind(&path)
            .expect("precheck-accepted maximum-length path must bind");
        drop(listener);
        let _ = std::fs::remove_file(&path);
    }
}
