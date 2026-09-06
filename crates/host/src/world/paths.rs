//! Per-user world paths, identifiers, and owner-only filesystem access.

use super::*;

/// The two CLI-owned per-user roots shared with a locally launched host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorldPaths {
    registry: PathBuf,
    evidence: PathBuf,
}

impl WorldPaths {
    /// Resolve and secure the platform's per-user runtime and data roots.
    pub fn discover() -> Result<Self> {
        let runtime = dirs::runtime_dir().unwrap_or_else(|| {
            std::env::temp_dir().join(format!("phoxal-{}", effective_user_id()))
        });
        let data = dirs::data_local_dir().context("the host has no per-user data directory")?;
        Self::create(
            runtime.join("phoxal").join("simulation"),
            data.join("phoxal").join("simulation"),
        )
    }

    /// Build paths at explicit roots, primarily for deterministic tests.
    pub fn create(registry: PathBuf, evidence: PathBuf) -> Result<Self> {
        secure_directory(&registry)?;
        secure_directory(&evidence)?;
        Ok(Self { registry, evidence })
    }

    #[must_use]
    pub fn registry(&self) -> &Path {
        &self.registry
    }

    #[must_use]
    pub fn evidence(&self) -> &Path {
        &self.evidence
    }

    #[must_use]
    pub fn registration_path(&self, instance: &str) -> PathBuf {
        self.registry.join(format!("{instance}.json"))
    }

    #[must_use]
    pub fn evidence_path(&self, instance: &str) -> PathBuf {
        self.evidence.join(instance)
    }
}
pub fn parse_instance_id(instance: &str) -> Result<WorldInstanceId> {
    WorldInstanceId::parse(instance).map_err(Into::into)
}

pub fn validate_instance_id(instance: &str) -> Result<()> {
    parse_instance_id(instance).map(|_| ())
}

pub(super) fn validate_relative_evidence_path(value: &str) -> Result<()> {
    let path = Path::new(value);
    ensure!(!path.as_os_str().is_empty(), "evidence path is empty");
    ensure!(!path.is_absolute(), "evidence path must be relative");
    ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_))),
        "evidence path `{value}` escapes its session directory"
    );
    Ok(())
}

pub(super) fn unix_ms() -> Result<u64> {
    let elapsed = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .context("system clock predates the Unix epoch")?;
    u64::try_from(elapsed.as_millis()).context("Unix timestamp overflows u64 milliseconds")
}

pub(super) enum AtomicPublish {
    Published,
    AlreadyExists,
}

pub(super) fn atomic_owner_json_if_absent(
    path: &Path,
    value: &impl Serialize,
) -> Result<AtomicPublish> {
    let parent = path
        .parent()
        .context("terminal summary path has no parent")?;
    validate_owner_directory(parent)?;
    let temporary = parent.join(format!(
        ".summary-recovery-{}-{}.tmp",
        std::process::id(),
        unix_ms()?
    ));
    let mut created = false;
    let result = (|| -> Result<AtomicPublish> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        }
        let mut file = options.open(&temporary).with_context(|| {
            format!("failed to create recovery summary {}", temporary.display())
        })?;
        created = true;
        serde_json::to_writer(&mut file, value)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        match fs::hard_link(&temporary, path) {
            Ok(()) => {
                File::open(parent)?.sync_all()?;
                Ok(AtomicPublish::Published)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Ok(AtomicPublish::AlreadyExists)
            }
            Err(error) => Err(error).with_context(|| {
                format!(
                    "failed to publish recovered terminal summary {}",
                    path.display()
                )
            }),
        }
    })();
    if created {
        match fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to remove recovery summary temporary {}",
                        temporary.display()
                    )
                });
            }
        }
    }
    result
}

pub(super) fn read_owner_file(path: &Path) -> Result<Vec<u8>> {
    let mut file = open_owner_file(path, false)
        .with_context(|| format!("failed to open owner-only file {}", path.display()))?;
    let mut document = Vec::new();
    file.read_to_end(&mut document)
        .with_context(|| format!("failed to read {}", path.display()))?;
    Ok(document)
}

pub(super) fn open_and_read_owner_file_if_present(path: &Path) -> Result<Option<(File, Vec<u8>)>> {
    match open_owner_file(path, false) {
        Ok(mut file) => {
            let mut document = Vec::new();
            file.read_to_end(&mut document)
                .with_context(|| format!("failed to read {}", path.display()))?;
            Ok(Some((file, document)))
        }
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => {
            Err(error).with_context(|| format!("failed to open owner-only file {}", path.display()))
        }
    }
}

pub(super) fn read_owner_file_if_present(path: &Path) -> Result<Option<Vec<u8>>> {
    match open_owner_file(path, false) {
        Ok(mut file) => {
            let mut document = Vec::new();
            file.read_to_end(&mut document)
                .with_context(|| format!("failed to read {}", path.display()))?;
            Ok(Some(document))
        }
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => {
            Err(error).with_context(|| format!("failed to open owner-only file {}", path.display()))
        }
    }
}

#[cfg(unix)]
pub(super) fn open_owner_file(path: &Path, writable: bool) -> Result<File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let mut options = OpenOptions::new();
    options.read(true).write(writable);
    options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file(),
        "{} is not a regular file",
        path.display()
    );
    ensure!(
        metadata.uid() == effective_user_id(),
        "{} is not owned by the current user",
        path.display()
    );
    ensure!(
        metadata.mode() & 0o777 == 0o600,
        "{} must have mode 0600, found {:04o}",
        path.display(),
        metadata.mode() & 0o777
    );
    let _ = file.as_raw_fd();
    Ok(file)
}

#[cfg(not(unix))]
pub(super) fn open_owner_file(_path: &Path, _writable: bool) -> Result<File> {
    bail!("local world sessions require Unix owner and lease semantics")
}

#[cfg(unix)]
pub(super) fn secure_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fs::create_dir_all(path)
        .with_context(|| format!("failed to create owner-only directory {}", path.display()))?;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.file_type().is_dir(),
        "{} is not a directory",
        path.display()
    );
    ensure!(
        metadata.uid() == effective_user_id(),
        "{} is not owned by the current user",
        path.display()
    );
    if metadata.mode() & 0o777 != 0o700 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn validate_owner_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect owner-only directory {}", path.display()))?;
    ensure!(
        metadata.file_type().is_dir(),
        "{} is not an owner-only directory",
        path.display()
    );
    ensure!(
        metadata.uid() == effective_user_id(),
        "{} is not owned by the current user",
        path.display()
    );
    ensure!(
        metadata.mode() & 0o777 == 0o700,
        "{} must have mode 0700, found {:04o}",
        path.display(),
        metadata.mode() & 0o777
    );
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn secure_directory(_path: &Path) -> Result<()> {
    bail!("local world sessions require Unix owner and lease semantics")
}

#[cfg(not(unix))]
pub(super) fn validate_owner_directory(_path: &Path) -> Result<()> {
    bail!("local world sessions require Unix owner and lease semantics")
}

#[cfg(unix)]
pub(super) fn remove_exact_open_file(path: &Path, open: &File) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let open_metadata = open.metadata()?;
    let path_metadata = fs::symlink_metadata(path).with_context(|| {
        format!(
            "stale world file {} disappeared before exact cleanup",
            path.display()
        )
    })?;
    ensure!(
        path_metadata.file_type().is_file()
            && path_metadata.dev() == open_metadata.dev()
            && path_metadata.ino() == open_metadata.ino(),
        "stale world file {} was replaced during recovery; refusing to remove it",
        path.display()
    );
    fs::remove_file(path)
        .with_context(|| format!("failed to remove stale world file {}", path.display()))
}

#[cfg(not(unix))]
pub(super) fn remove_exact_open_file(_path: &Path, _open: &File) -> Result<()> {
    bail!("local world sessions require Unix owner and lease semantics")
}

#[cfg(unix)]
pub(super) fn try_lock_lease(file: &File) -> Result<bool> {
    use std::os::fd::AsRawFd;

    // SAFETY: `file` owns a valid open descriptor for this call's duration.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if matches!(error.raw_os_error(), Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN)
    {
        return Ok(false);
    }
    Err(error).context("failed to inspect the world host lease")
}

#[cfg(not(unix))]
pub(super) fn try_lock_lease(_file: &File) -> Result<bool> {
    bail!("local world sessions require Unix owner and lease semantics")
}

#[cfg(unix)]
fn effective_user_id() -> u32 {
    // SAFETY: `geteuid` has no preconditions and does not mutate memory.
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
const fn effective_user_id() -> u32 {
    0
}
