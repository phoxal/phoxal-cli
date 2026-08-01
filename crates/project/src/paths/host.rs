use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub use phoxal_cli_core::runtime::PROJECT_ROOT_ENV;

pub fn project_root() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os(PROJECT_ROOT_ENV) {
        return Ok(PathBuf::from(path));
    }
    let cwd =
        std::env::current_dir().context("failed to determine the current project directory")?;
    let discovered = phoxal_cli_core::project::resolver::discover_robot_yaml(&cwd)
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .ok_or_else(|| anyhow::anyhow!("cannot locate a robot project from {}", cwd.display()));
    #[cfg(test)]
    return discovered.or(Ok(cwd));
    #[cfg(not(test))]
    discovered
}

pub fn project_state_dir() -> Result<PathBuf> {
    project_root().map(|root| crate::paths::runtime::RuntimePaths::for_root(&root).state_root)
}

/// Project-local staging root for generated Webots worlds and controllers.
pub fn webots_dir() -> Result<PathBuf> {
    project_state_dir().map(|root| root.join("webots"))
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::ffi::OsString;
    use std::sync::Mutex;

    static PROJECT_ROOT_TEST_LOCK: Mutex<()> = Mutex::new(());

    pub(crate) struct ScratchPhoxalHome {
        _root: std::path::PathBuf,
        previous: Option<OsString>,
        previous_train: Option<OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl ScratchPhoxalHome {
        pub(crate) fn new() -> anyhow::Result<Self> {
            let lock = PROJECT_ROOT_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let root = tempfile::tempdir()?.keep();
            let previous = std::env::var_os(super::PROJECT_ROOT_ENV);
            let previous_train = std::env::var_os("PHOXAL_TEST_LOCKED_TRAIN");
            // SAFETY: tests serialize mutations through PROJECT_ROOT_TEST_LOCK.
            unsafe {
                std::env::set_var(super::PROJECT_ROOT_ENV, &root);
                std::env::set_var("PHOXAL_TEST_LOCKED_TRAIN", "0.1.0");
            };
            Ok(Self {
                _root: root,
                previous,
                previous_train,
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
                match &self.previous_train {
                    Some(value) => std::env::set_var("PHOXAL_TEST_LOCKED_TRAIN", value),
                    None => std::env::remove_var("PHOXAL_TEST_LOCKED_TRAIN"),
                }
            }
        }
    }
}
