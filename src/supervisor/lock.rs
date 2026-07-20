//! Stable advisory-lock authority for one foreground supervisor per user.

use anyhow::{Context, Result, bail};
use phoxal_cli_core::session::SessionMode;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisorIdentity {
    pub project: PathBuf,
    pub entry: PathBuf,
    pub mode: String,
    pub pid: u32,
}

impl SupervisorIdentity {
    #[must_use]
    pub fn resolve(project: &Path, mode: SessionMode) -> Self {
        let project = best_effort_absolute(project);
        let entry = phoxal_cli_core::project::resolver::discover_robot_yaml(&project)
            .map(|entry| best_effort_absolute(&entry))
            .unwrap_or_else(|_| project.join("robot.yaml"));
        Self {
            project,
            entry,
            mode: mode.to_string(),
            pid: std::process::id(),
        }
    }
}

fn best_effort_absolute(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        }
    })
}

#[derive(Debug)]
pub struct SupervisorLock {
    file: File,
    #[cfg(test)]
    path: PathBuf,
}

impl SupervisorLock {
    pub fn acquire(identity: SupervisorIdentity) -> Result<Self> {
        Self::acquire_path(&crate::host_paths::supervisor_lock_path()?, identity)
    }

    pub fn acquire_path(path: &Path, identity: SupervisorIdentity) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("failed to open supervisor lock {}", path.display()))?;
        if let Err(error) = crate::native_artifacts::try_advisory_lock(&file, true) {
            let active = read_identity(&mut file).ok();
            if let Some(active) = active {
                bail!(
                    "another phoxal-cli supervisor is active: mode={}, project={}, entry={}, pid={} (lock: {}; {error})",
                    active.mode,
                    active.project.display(),
                    active.entry.display(),
                    active.pid,
                    path.display(),
                );
            }
            return Err(error).with_context(|| {
                format!("another phoxal-cli supervisor holds {}", path.display())
            });
        }
        write_identity(&mut file, &identity).with_context(|| {
            format!(
                "failed to write supervisor owner metadata to {}",
                path.display()
            )
        })?;
        Ok(Self {
            file,
            #[cfg(test)]
            path: path.to_path_buf(),
        })
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SupervisorLock {
    fn drop(&mut self) {
        // The inode is intentionally permanent. Unlinking a locked file lets a
        // competing process create and lock a different inode at the same path.
        let _ = crate::native_artifacts::unlock_advisory(&self.file);
    }
}

fn write_identity(file: &mut File, identity: &SupervisorIdentity) -> Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    serde_json::to_writer(&mut *file, identity)?;
    file.write_all(b"\n")?;
    file.sync_data()?;
    Ok(())
}

fn read_identity(file: &mut File) -> Result<SupervisorIdentity> {
    file.seek(SeekFrom::Start(0))?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    serde_json::from_str(&contents).context("invalid supervisor lock metadata")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    const HELPER_ENV: &str = "PHOXAL_SUPERVISOR_LOCK_HELPER";

    fn identity(root: &Path, mode: &str, pid: u32) -> SupervisorIdentity {
        SupervisorIdentity {
            project: root.join("project"),
            entry: root.join("project/robot.yaml"),
            mode: mode.to_string(),
            pid,
        }
    }

    #[test]
    fn subprocess_lock_helper() {
        let Some(path) = std::env::var_os(HELPER_ENV) else {
            return;
        };
        let path = PathBuf::from(path);
        let lock = SupervisorLock::acquire_path(&path, identity(&path, "run", std::process::id()))
            .expect("helper must acquire lock");
        println!("LOCKED");
        std::io::stdout().flush().expect("flush helper readiness");
        std::thread::sleep(Duration::from_secs(30));
        drop(lock);
    }

    #[test]
    fn advisory_lock_survives_stale_metadata_and_keeps_one_inode() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("supervisor.lock");
        fs::write(
            &path,
            serde_json::to_vec(&identity(temp.path(), "stale", 999_999))?,
        )?;
        let lock = SupervisorLock::acquire_path(&path, identity(temp.path(), "run", 42))?;
        assert_eq!(lock.path(), path);
        assert!(path.is_file());
        drop(lock);
        assert!(
            path.is_file(),
            "dropping the owner must retain the stable inode"
        );
        let stored: SupervisorIdentity = serde_json::from_slice(&fs::read(path)?)?;
        assert_eq!(stored.mode, "run");
        assert_eq!(stored.pid, 42);
        Ok(())
    }

    #[test]
    fn missing_diagnostic_entry_never_blocks_lock_authority() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let missing_project = temp.path().join("missing-project");
        let identity = SupervisorIdentity::resolve(&missing_project, SessionMode::Run);
        assert_eq!(identity.project, missing_project);
        assert_eq!(identity.entry, missing_project.join("robot.yaml"));

        let path = temp.path().join("supervisor.lock");
        let lock = SupervisorLock::acquire_path(&path, identity.clone())?;
        let stored: SupervisorIdentity = serde_json::from_slice(&fs::read(&path)?)?;
        assert_eq!(stored, identity);
        drop(lock);
        Ok(())
    }

    #[test]
    fn process_death_releases_authority_and_metadata_is_diagnostic() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("supervisor.lock");
        let mut child = Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "supervisor::lock::tests::subprocess_lock_helper",
                "--nocapture",
            ])
            .env(HELPER_ENV, &path)
            .stdout(Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().context("helper stdout")?;
        let mut ready = std::io::BufReader::new(stdout);
        let mut output = String::new();
        loop {
            let mut line = String::new();
            if std::io::BufRead::read_line(&mut ready, &mut line)? == 0 {
                break;
            }
            output.push_str(&line);
            if line.contains("LOCKED") {
                break;
            }
        }
        assert!(
            output.contains("LOCKED"),
            "unexpected helper output: {output}"
        );

        let error = SupervisorLock::acquire_path(
            &path,
            identity(temp.path(), "simulation", std::process::id()),
        )
        .expect_err("a second process must not acquire the advisory lock");
        let message = format!("{error:#}");
        assert!(message.contains("mode=run"), "{message}");
        assert!(message.contains("project="), "{message}");
        assert!(message.contains("entry="), "{message}");

        child.kill()?;
        child.wait()?;
        let replacement = SupervisorLock::acquire_path(
            &path,
            identity(temp.path(), "simulation", std::process::id()),
        )?;
        drop(replacement);
        assert!(path.is_file());
        Ok(())
    }
}
