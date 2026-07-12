use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub const PROJECT_ROOT_ENV: &str = "PHOXAL_PROJECT_ROOT";

pub fn project_root() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os(PROJECT_ROOT_ENV) {
        return Ok(PathBuf::from(path));
    }
    let cwd =
        std::env::current_dir().context("failed to determine the current project directory")?;
    let discovered = crate::resolver::discover_robot_yaml(&cwd)
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .ok_or_else(|| anyhow::anyhow!("cannot locate a robot project from {}", cwd.display()));
    #[cfg(test)]
    return discovered.or(Ok(cwd));
    #[cfg(not(test))]
    discovered
}

pub fn project_state_dir() -> Result<PathBuf> {
    project_root().map(|root| root.join(".phoxal"))
}

pub fn artifacts_dir() -> Result<PathBuf> {
    project_state_dir().map(|root| root.join("artifacts"))
}

pub fn git_artifacts_dir() -> Result<PathBuf> {
    project_state_dir().map(|root| root.join("git"))
}

pub fn deploy_dir() -> Result<PathBuf> {
    project_state_dir().map(|root| root.join("build"))
}

pub fn webots_dir() -> Result<PathBuf> {
    project_state_dir().map(|root| root.join("webots"))
}

pub fn run_dir() -> Result<PathBuf> {
    project_state_dir().map(|root| root.join("run"))
}

pub fn simulator_lock_path() -> Result<PathBuf> {
    dirs::home_dir()
        .context("$HOME is not set; cannot locate the simulator lock")
        .map(|home| home.join(".phoxal").join("simulator.lock"))
}

pub fn cli_update_cache_path() -> Result<PathBuf> {
    dirs::home_dir()
        .context("$HOME is not set; cannot locate the CLI update cache")
        .map(|home| home.join(".phoxal").join("cli-update.json"))
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::ffi::OsString;
    use std::sync::Mutex;

    static PROJECT_ROOT_TEST_LOCK: Mutex<()> = Mutex::new(());

    pub(crate) struct ScratchPhoxalHome {
        _root: std::path::PathBuf,
        previous: Option<OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl ScratchPhoxalHome {
        pub(crate) fn new() -> anyhow::Result<Self> {
            let lock = PROJECT_ROOT_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let root = tempfile::tempdir()?.keep();
            let previous = std::env::var_os(super::PROJECT_ROOT_ENV);
            // SAFETY: tests serialize mutations through PROJECT_ROOT_TEST_LOCK.
            unsafe { std::env::set_var(super::PROJECT_ROOT_ENV, &root) };
            Ok(Self {
                _root: root,
                previous,
                _lock: lock,
            })
        }
    }

    impl Drop for ScratchPhoxalHome {
        fn drop(&mut self) {
            // SAFETY: tests serialize mutations through PROJECT_ROOT_TEST_LOCK.
            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var(super::PROJECT_ROOT_ENV, value),
                    None => std::env::remove_var(super::PROJECT_ROOT_ENV),
                }
            }
        }
    }
}
