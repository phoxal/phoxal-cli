use std::path::PathBuf;

use anyhow::{Context, Result};

pub fn phoxal_home() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("PHOXAL_HOME") {
        return Ok(PathBuf::from(path));
    }
    dirs::home_dir()
        .context("$HOME is not set; cannot locate ~/.phoxal")
        .map(|home| home.join(".phoxal"))
}

pub fn cache_dir() -> Result<PathBuf> {
    phoxal_home().map(|path| path.join("cache"))
}

pub fn run_dir() -> Result<PathBuf> {
    phoxal_home().map(|path| path.join("run"))
}

/// The LOCAL download manifest: a `phoxal::catalog::Manifest` holding only the
/// entries whose tarball(s) are present in [`artifacts_dir`] - NOT a cached
/// copy of the remote catalog (see `crate::catalog`). Directly under
/// `cache/`, not a `cache/catalog/` subdir.
pub fn local_manifest_path() -> Result<PathBuf> {
    cache_dir().map(|path| path.join(LOCAL_MANIFEST_FILE))
}

/// The flat, content-addressed native-artifact tarball store: one file per
/// release asset, named exactly as published (`phoxal-driver-bno085-v0.1.5-
/// <triple>.tar.zst`) - no per-package subdirectories.
pub fn artifacts_dir() -> Result<PathBuf> {
    cache_dir().map(|path| path.join("artifacts"))
}

/// Shallow git checkouts for `robot.yaml` `artifacts.pins` git sources (any
/// kind, not just components) - see `crate::git_artifact`.
pub fn git_artifacts_dir() -> Result<PathBuf> {
    cache_dir().map(|path| path.join("git-artifacts"))
}

/// The cross-build toolchain + target dir `deploy` caches under
/// `cache/deploy/` (zigbuild toolchain provisioning, cross-compiled target
/// directories).
pub fn deploy_dir() -> Result<PathBuf> {
    cache_dir().map(|path| path.join("deploy"))
}

const LOCAL_MANIFEST_FILE: &str = "phoxal-artifacts.json";

/// Test-only support for pointing `PHOXAL_HOME` (and therefore
/// `cache_dir()`/`run_dir()`/the Webots staging root) at a scratch directory.
/// `cargo test`'s default thread pool runs unit tests concurrently within one
/// process, and env vars are process-global, so every test across the crate
/// that mutates `PHOXAL_HOME` must go through [`test_support::ScratchPhoxalHome`]
/// - it serializes on a shared lock instead of racing another test's value.
#[cfg(test)]
pub(crate) mod test_support {
    use std::ffi::OsString;
    use std::sync::Mutex;

    /// Serializes every test that mutates the process-global `PHOXAL_HOME`.
    static PHOXAL_HOME_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// RAII guard: points `PHOXAL_HOME` at a fresh scratch directory for the
    /// life of the guard (holding [`PHOXAL_HOME_TEST_LOCK`] so no other test
    /// observes or clobbers it) and restores the previous value on drop.
    pub(crate) struct ScratchPhoxalHome {
        _root: tempfile::TempDir,
        previous: Option<OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl ScratchPhoxalHome {
        pub(crate) fn new() -> anyhow::Result<Self> {
            let lock = PHOXAL_HOME_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let root = tempfile::tempdir()?;
            let previous = std::env::var_os("PHOXAL_HOME");
            // SAFETY: test-only, serialized by `PHOXAL_HOME_TEST_LOCK` above;
            // `Drop` restores the prior value before `root` is removed.
            unsafe {
                std::env::set_var("PHOXAL_HOME", root.path());
            }
            Ok(Self {
                _root: root,
                previous,
                _lock: lock,
            })
        }
    }

    impl Drop for ScratchPhoxalHome {
        fn drop(&mut self) {
            // SAFETY: see `new`.
            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var("PHOXAL_HOME", value),
                    None => std::env::remove_var("PHOXAL_HOME"),
                }
            }
        }
    }
}
