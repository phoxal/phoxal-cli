//! Exclusive session lock acquisition and stale-owner recovery.

use super::SUPERVISOR_LOCK_FILE;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

#[derive(Debug)]
pub struct SupervisorLock {
    path: PathBuf,
    owned: bool,
}

impl SupervisorLock {
    pub fn acquire(run_dir: &Path) -> Result<Self> {
        fs::create_dir_all(run_dir)
            .with_context(|| format!("failed to create {}", run_dir.display()))?;
        Self::acquire_path(&run_dir.join(SUPERVISOR_LOCK_FILE))
    }

    pub fn acquire_path(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let path = path.to_path_buf();
        match try_create_lock(&path) {
            Ok(()) => Ok(Self { path, owned: true }),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing_pid = fs::read_to_string(&path)
                    .ok()
                    .and_then(|contents| contents.trim().parse::<u32>().ok());
                if existing_pid.is_some_and(pid_alive) {
                    bail!(
                        "another phoxal-cli supervisor session is already active on this host (lock: {})",
                        path.display()
                    );
                }
                let _ = fs::remove_file(&path);
                try_create_lock(&path).with_context(|| {
                    format!("failed to replace stale supervisor lock {}", path.display())
                })?;
                Ok(Self { path, owned: true })
            }
            Err(error) => Err(error)
                .with_context(|| format!("failed to create supervisor lock {}", path.display())),
        }
    }
}

impl Drop for SupervisorLock {
    fn drop(&mut self) {
        if self.owned {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(crate) fn try_create_lock(path: &Path) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    writeln!(file, "{}", std::process::id())
}

pub(crate) fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .is_ok_and(|status| status.success())
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}
