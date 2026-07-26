//! Stable project-operation authority for execution and artifact mutation.

use anyhow::{Context, Result, bail};
use phoxal::bus::ExecutionId;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectLockIdentity {
    pub project: PathBuf,
    pub entry: PathBuf,
    pub operation: ProjectOperation,
    pub pid: u32,
    /// The supervised run this lock holder started, for a `Run` operation.
    ///
    /// The bus key root is execution-scoped (#952 section B), so an ad hoc
    /// inspector has to join the *running* execution rather than mint its own -
    /// otherwise it subscribes a root nobody publishes on. The lock is written
    /// when the run starts and released when it ends, which is exactly the
    /// execution's lifetime, so it is the natural place to publish the id.
    #[serde(default)]
    pub execution: Option<ExecutionId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectOperation {
    Run,
    Build,
    Update,
    Install,
}

impl std::fmt::Display for ProjectOperation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Run => "run",
            Self::Build => "build",
            Self::Update => "update",
            Self::Install => "install",
        })
    }
}

impl ProjectLockIdentity {
    #[must_use]
    pub fn resolve(project: &Path, operation: ProjectOperation) -> Self {
        let project = if crate::runtime_paths::is_installed_root(project) {
            crate::runtime_paths::RuntimePaths::for_root(project).ownership_root
        } else {
            best_effort_absolute(project)
        };
        let entry = phoxal_cli_core::project::resolver::discover_robot_yaml(&project)
            .map(|entry| best_effort_absolute(&entry))
            .unwrap_or_else(|_| project.join("robot.yaml"));
        Self {
            project,
            entry,
            operation,
            pid: std::process::id(),
            execution: None,
        }
    }

    /// Record the supervised run this holder is about to start.
    #[must_use]
    pub fn in_execution(mut self, execution: ExecutionId) -> Self {
        self.execution = Some(execution);
        self
    }
}

/// The execution an ad hoc client should join to observe the running project.
///
/// `None` means nothing is running, which is a better error for the caller than
/// silently subscribing an empty root.
pub fn active_execution(project: &Path) -> Result<Option<ExecutionId>> {
    Ok(match ProjectLock::inspect(project)? {
        ProjectLockStatus::Held(identity) if identity.operation == ProjectOperation::Run => {
            identity.execution
        }
        ProjectLockStatus::Held(_) | ProjectLockStatus::Free => None,
    })
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
pub struct ProjectLock {
    file: File,
    #[cfg(test)]
    path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectLockStatus {
    Free,
    Held(ProjectLockIdentity),
}

impl ProjectLock {
    #[must_use]
    pub fn lock_path(project: &Path) -> PathBuf {
        crate::runtime_paths::RuntimePaths::for_root(project).project_lock()
    }

    pub fn inspect(project: &Path) -> Result<ProjectLockStatus> {
        let path = Self::lock_path(project);
        let mut file = match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ProjectLockStatus::Free);
            }
            Err(error) => {
                return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
            }
        };
        match crate::native_artifacts::try_advisory_lock(&file, true) {
            Ok(()) => {
                crate::native_artifacts::unlock_advisory(&file)?;
                Ok(ProjectLockStatus::Free)
            }
            Err(_) => Ok(ProjectLockStatus::Held(
                read_identity(&mut file).with_context(|| {
                    format!("failed to read active operation from {}", path.display())
                })?,
            )),
        }
    }

    pub fn acquire(identity: ProjectLockIdentity) -> Result<Self> {
        let path = Self::lock_path(&identity.project);
        Self::acquire_path(&path, identity)
    }

    pub fn acquire_path(path: &Path, identity: ProjectLockIdentity) -> Result<Self> {
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
            .with_context(|| format!("failed to open project-operation lock {}", path.display()))?;
        if let Err(error) = crate::native_artifacts::try_advisory_lock(&file, true) {
            let active = read_identity(&mut file).ok();
            if let Some(active) = active {
                bail!(
                    "another exclusive phoxal project operation is active: operation={}, project={}, entry={}, pid={} (lock: {}; {error})",
                    active.operation,
                    active.project.display(),
                    active.entry.display(),
                    active.pid,
                    path.display(),
                );
            }
            return Err(error).with_context(|| {
                format!("another phoxal project operation holds {}", path.display())
            });
        }
        write_identity(&mut file, &identity).with_context(|| {
            format!(
                "failed to write project-operation owner metadata to {}",
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

impl Drop for ProjectLock {
    fn drop(&mut self) {
        // The inode is intentionally permanent. Unlinking a locked file lets a
        // competing process create and lock a different inode at the same path.
        let _ = crate::native_artifacts::unlock_advisory(&self.file);
    }
}

fn write_identity(file: &mut File, identity: &ProjectLockIdentity) -> Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    serde_json::to_writer(&mut *file, identity)?;
    file.write_all(b"\n")?;
    file.sync_data()?;
    Ok(())
}

fn read_identity(file: &mut File) -> Result<ProjectLockIdentity> {
    file.seek(SeekFrom::Start(0))?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    serde_json::from_str(&contents).context("invalid project-operation lock metadata")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(root: &Path, mode: &str, pid: u32) -> ProjectLockIdentity {
        ProjectLockIdentity {
            project: root.join("project"),
            entry: root.join("project/robot.yaml"),
            operation: if mode == "stale" {
                ProjectOperation::Update
            } else {
                ProjectOperation::Run
            },
            pid,
            execution: None,
        }
    }

    #[test]
    fn advisory_lock_survives_stale_metadata_and_keeps_one_inode() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("supervisor.lock");
        fs::write(
            &path,
            serde_json::to_vec(&identity(temp.path(), "stale", 999_999))?,
        )?;
        let lock = ProjectLock::acquire_path(&path, identity(temp.path(), "run", 42))?;
        assert_eq!(lock.path(), path);
        assert!(path.is_file());
        drop(lock);
        assert!(
            path.is_file(),
            "dropping the owner must retain the stable inode"
        );
        let stored: ProjectLockIdentity = serde_json::from_slice(&fs::read(path)?)?;
        assert_eq!(stored.operation, ProjectOperation::Run);
        assert_eq!(stored.pid, 42);
        Ok(())
    }

    #[test]
    fn missing_diagnostic_entry_never_blocks_lock_authority() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let missing_project = temp.path().join("missing-project");
        let identity = ProjectLockIdentity::resolve(&missing_project, ProjectOperation::Run);
        assert_eq!(identity.project, missing_project);
        assert_eq!(identity.entry, missing_project.join("robot.yaml"));

        let path = temp.path().join("supervisor.lock");
        let lock = ProjectLock::acquire_path(&path, identity.clone())?;
        let stored: ProjectLockIdentity = serde_json::from_slice(&fs::read(&path)?)?;
        assert_eq!(stored, identity);
        drop(lock);
        Ok(())
    }

    #[test]
    fn different_projects_have_independent_operation_authority() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let first_project = temp.path().join("first");
        let second_project = temp.path().join("second");
        fs::create_dir_all(&first_project)?;
        fs::create_dir_all(&second_project)?;

        let first = ProjectLock::acquire(ProjectLockIdentity::resolve(
            &first_project,
            ProjectOperation::Run,
        ))?;
        let second = ProjectLock::acquire(ProjectLockIdentity::resolve(
            &second_project,
            ProjectOperation::Update,
        ))?;

        assert!(first_project.join(".phoxal/project.lock").is_file());
        assert!(second_project.join(".phoxal/project.lock").is_file());
        drop((first, second));
        assert!(first_project.join(".phoxal/project.lock").is_file());
        assert!(second_project.join(".phoxal/project.lock").is_file());
        Ok(())
    }
}
