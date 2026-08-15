//! Stable project-operation authority for execution and artifact mutation.
//!
//! Two locks guard a root, and they answer different questions. `build.lock`
//! ([`ProjectLock`]) serializes *this* tool's own exclusive project
//! operations against each other. `supervisor.lock` is taken by `phoxal-supervisor` for
//! its whole life, so it answers the other question - whether an execution is
//! running out of the very files a command is about to replace - and
//! [`refuse_while_execution_is_live`] is the one place that asks it.

use anyhow::{Context, Result, bail};
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectOperation {
    Run,
    Build,
    Validate,
    Install,
}

impl std::fmt::Display for ProjectOperation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Run => "run",
            Self::Build => "build",
            Self::Validate => "validate",
            Self::Install => "install",
        })
    }
}

impl ProjectLockIdentity {
    #[must_use]
    pub fn resolve(project: &Path, operation: ProjectOperation) -> Self {
        let project = best_effort_absolute(project);
        let entry = phoxal_cli_project::source::resolver::discover_robot_yaml(&project)
            .map(|entry| best_effort_absolute(&entry))
            .unwrap_or_else(|_| project.join("robot.yaml"));
        Self {
            project,
            entry,
            operation,
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
pub struct ProjectLock {
    file: File,
    #[cfg(test)]
    path: PathBuf,
}

impl ProjectLock {
    #[must_use]
    pub fn lock_path(project: &Path) -> PathBuf {
        phoxal_cli_host::paths::RuntimePaths::for_root(project).build_lock()
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
        if let Err(error) = phoxal_cli_host::advisory::try_advisory_lock(&file, true) {
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

// ---------------------------------------------------------------------------
// the live-execution gate
// ---------------------------------------------------------------------------

/// A live framework supervisor that owns a root's execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionHolder {
    /// The supervisor lock the supervisor holds.
    pub lock: PathBuf,
    /// The pid the supervisor recorded in it. Diagnostic only - the lock itself is
    /// the authority, and a lock held with no readable pid is still held.
    pub pid: Option<u32>,
}

/// Probe `root`'s supervisor lock and report the supervisor holding it, if any.
///
/// The lock - not the socket file, and not a completed handshake - is what
/// "an execution is live" means: the supervisor takes it before it reads the
/// bundle and the kernel releases it when the process ends however it ended.
/// A socket file survives a killed supervisor; an advisory lock does not.
///
/// The probe is a non-blocking exclusive try-lock on a second descriptor.
/// Advisory locks are held per open file description, so this conflicts with
/// the supervisor's hold even when both are in one process, and a free lock is
/// released again immediately rather than being kept for the caller.
///
/// # Errors
///
/// Only when the lock exists but cannot be opened at all. A missing lock is
/// not an error: it means no supervisor has ever run here.
pub fn execution_holder(root: &Path) -> Result<Option<ExecutionHolder>> {
    let lock = phoxal_cli_host::paths::RuntimePaths::for_root(root).supervisor_lock();
    let mut file = match File::open(&lock) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to probe the supervisor lock {}", lock.display())
            });
        }
    };
    if phoxal_cli_host::advisory::try_advisory_lock(&file, true).is_ok() {
        // Free. Release the probe at once: holding it would block the very
        // supervisor this command is about to launch.
        let _ = phoxal_cli_host::advisory::unlock_advisory(&file);
        return Ok(None);
    }
    let mut recorded = String::new();
    let pid = file
        .read_to_string(&mut recorded)
        .ok()
        .and_then(|_| recorded.trim().parse::<u32>().ok());
    Ok(Some(ExecutionHolder { lock, pid }))
}

/// Refuse a bundle-mutating command while an execution owns `root`.
///
/// Every command that replaces what a running graph executes from - building
/// and publishing, `run`/`start`, a simulation run, and installing or rolling
/// back a release - calls this after taking [`ProjectLock`]. The build lock
/// serializes this tool against itself; only the supervisor lock can say that
/// somebody else is *running* the files about to be replaced.
///
/// # Errors
///
/// When a supervisor holds the lock, naming the two commands that apply.
pub fn refuse_while_execution_is_live(root: &Path) -> Result<()> {
    let Some(holder) = execution_holder(root)? else {
        return Ok(());
    };
    let display = root.display();
    let pid = holder
        .pid
        .map_or_else(String::new, |pid| format!(" (pid {pid})"));
    bail!(
        "an execution is live under {display}: phoxal-supervisor{pid} holds the supervisor lock {}. This \
         command replaces the bundle that execution is running from, so it is refused while the \
         supervisor owns it. Attach to it with `phoxal attach {display}`, or end it with `phoxal stop \
         {display}`, then run this command again",
        holder.lock.display()
    );
}

impl Drop for ProjectLock {
    fn drop(&mut self) {
        // The inode is intentionally permanent. Unlinking a locked file lets a
        // competing process create and lock a different inode at the same path.
        let _ = phoxal_cli_host::advisory::unlock_advisory(&self.file);
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
                ProjectOperation::Build
            } else {
                ProjectOperation::Run
            },
            pid,
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
            ProjectOperation::Build,
        ))?;

        assert!(first_project.join(".phoxal/run/build.lock").is_file());
        assert!(second_project.join(".phoxal/run/build.lock").is_file());
        drop((first, second));
        assert!(first_project.join(".phoxal/run/build.lock").is_file());
        assert!(second_project.join(".phoxal/run/build.lock").is_file());
        Ok(())
    }

    /// The whole point of the supervisor lock: a running execution prevents
    /// mutation of the bundle it is running from, and releasing it lets the
    /// same mutation through unchanged.
    #[test]
    fn a_running_execution_prevents_mutation_until_it_releases_the_lock() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let project = temp.path().join("project");
        fs::create_dir_all(&project)?;

        // Nothing has ever run here, so there is no lock and nothing to refuse.
        refuse_while_execution_is_live(&project)?;

        // A supervisor-style holder takes the supervisor lock for its whole life
        // and records its pid, exactly as the supervisor does.
        let lock_path = phoxal_cli_host::paths::RuntimePaths::for_root(&project).supervisor_lock();
        fs::create_dir_all(lock_path.parent().context("the run directory")?)?;
        let mut held = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        phoxal_cli_host::advisory::try_advisory_lock(&held, true)
            .expect("the supervisor-style holder takes the supervisor lock");
        writeln!(held, "4242")?;
        held.sync_data()?;

        // The build lock is free and stays free: the two locks are
        // independent, so it is the supervisor lock that refuses the mutation.
        let build = ProjectLock::acquire(ProjectLockIdentity::resolve(
            &project,
            ProjectOperation::Build,
        ))?;
        let error = refuse_while_execution_is_live(&project)
            .expect_err("a live execution refuses a bundle-mutating command");
        let rendered = format!("{error:#}");
        assert!(rendered.contains("an execution is live"), "{rendered}");
        assert!(rendered.contains("pid 4242"), "{rendered}");
        assert!(rendered.contains("phoxal attach"), "{rendered}");
        assert!(rendered.contains("phoxal stop"), "{rendered}");
        drop(build);

        // Released: the probe finds it free and the mutation proceeds.
        phoxal_cli_host::advisory::unlock_advisory(&held)
            .expect("the holder releases the supervisor lock");
        refuse_while_execution_is_live(&project)?;
        Ok(())
    }

    /// A lock that exists but is unheld is not an execution: a supervisor that
    /// exited leaves the file behind, and only the advisory hold is authority.
    #[test]
    fn an_abandoned_supervisor_lock_file_never_refuses_a_mutation() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let project = temp.path().join("project");
        let lock_path = phoxal_cli_host::paths::RuntimePaths::for_root(&project).supervisor_lock();
        fs::create_dir_all(lock_path.parent().context("the run directory")?)?;
        fs::write(&lock_path, "999999\n")?;

        assert_eq!(execution_holder(&project)?, None);
        refuse_while_execution_is_live(&project)?;
        Ok(())
    }

    #[test]
    fn validate_operation_round_trips_and_identifies_itself_precisely() -> Result<()> {
        let identity = ProjectLockIdentity {
            project: PathBuf::from("/project"),
            entry: PathBuf::from("/project/robot.yaml"),
            operation: ProjectOperation::Validate,
            pid: 7,
        };
        let restored: ProjectLockIdentity =
            serde_json::from_slice(&serde_json::to_vec(&identity)?)?;
        assert_eq!(restored.operation, ProjectOperation::Validate);
        assert_eq!(restored.operation.to_string(), "validate");
        Ok(())
    }
}
