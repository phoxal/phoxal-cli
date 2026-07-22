//! Central child launch boundary and crash-containment guardian.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::ops::{Deref, DerefMut};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt as _;
use std::process::Stdio;
use std::sync::{LazyLock, Mutex};
use tokio::process::{Child, Command};

static GUARDIAN: LazyLock<Mutex<Option<GuardianClient>>> = LazyLock::new(|| Mutex::new(None));

struct GuardianClient {
    writer: std::fs::File,
    acknowledgements: std::fs::File,
    pending_acknowledgements: std::collections::BTreeMap<[u8; 9], usize>,
    _process: Option<std::process::Child>,
}

impl GuardianClient {
    fn start() -> Result<Self> {
        let mut fds = [0_i32; 2];
        // SAFETY: valid pointer to a two-element fd array.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error()).context("create guardian pipe");
        }
        // SAFETY: ownership of every descriptor returned by pipe transfers to
        // exactly one guard, including all startup error paths below.
        let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        let mut acknowledgement_fds = [0_i32; 2];
        if unsafe { libc::pipe(acknowledgement_fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("create guardian acknowledgement pipe");
        }
        let acknowledgement_read_fd = unsafe { OwnedFd::from_raw_fd(acknowledgement_fds[0]) };
        let acknowledgement_write_fd = unsafe { OwnedFd::from_raw_fd(acknowledgement_fds[1]) };
        // The guardian inherits only the read end. The writer is CLOEXEC so
        // participants cannot keep containment alive after the supervisor dies.
        // SAFETY: fds were just returned by pipe.
        if unsafe { libc::fcntl(write_fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0
            || unsafe {
                libc::fcntl(
                    acknowledgement_read_fd.as_raw_fd(),
                    libc::F_SETFD,
                    libc::FD_CLOEXEC,
                )
            } != 0
        {
            return Err(std::io::Error::last_os_error())
                .context("mark guardian parent descriptors close-on-exec");
        }
        if cfg!(test) {
            // Unit-test executables use the libtest harness, not the CLI main.
            // Keep identical EOF semantics in a dedicated thread.
            let reader = std::fs::File::from(read_fd);
            let acknowledgement_writer = std::fs::File::from(acknowledgement_write_fd);
            std::thread::spawn(move || guard_reader(reader, Some(acknowledgement_writer)));
            let writer = std::fs::File::from(write_fd);
            let acknowledgements = std::fs::File::from(acknowledgement_read_fd);
            return Ok(Self {
                writer,
                acknowledgements,
                pending_acknowledgements: std::collections::BTreeMap::new(),
                _process: None,
            });
        }
        let exe = std::env::current_exe().context("resolve supervisor executable")?;
        let process = std::process::Command::new(exe)
            .arg("__graph-guardian")
            .arg(read_fd.as_raw_fd().to_string())
            .arg(acknowledgement_write_fd.as_raw_fd().to_string())
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn graph guardian")?;
        drop(read_fd);
        drop(acknowledgement_write_fd);
        let writer = std::fs::File::from(write_fd);
        let acknowledgements = std::fs::File::from(acknowledgement_read_fd);
        Ok(Self {
            writer,
            acknowledgements,
            pending_acknowledgements: std::collections::BTreeMap::new(),
            _process: Some(process),
        })
    }

    fn record(&mut self, operation: u8, pgid: u32) -> Result<()> {
        let mut record = [0_u8; 9];
        record[0] = operation;
        record[1..].copy_from_slice(&u64::from(pgid).to_ne_bytes());
        self.writer
            .write_all(&record)
            .context("write graph guardian record")?;
        self.writer.flush().context("flush graph guardian record")?;
        self.confirm(record)
    }

    fn confirm(&mut self, expected: [u8; 9]) -> Result<()> {
        if let Some(count) = self.pending_acknowledgements.get_mut(&expected) {
            *count -= 1;
            if *count == 0 {
                self.pending_acknowledgements.remove(&expected);
            }
            return Ok(());
        }
        loop {
            let mut acknowledgement = [0_u8; 9];
            self.acknowledgements
                .read_exact(&mut acknowledgement)
                .context("read graph guardian acknowledgement")?;
            if acknowledgement == expected {
                return Ok(());
            }
            *self
                .pending_acknowledgements
                .entry(acknowledgement)
                .or_default() += 1;
        }
    }
}

fn with_guardian<T>(operation: impl FnOnce(&mut GuardianClient) -> Result<T>) -> Result<T> {
    let mut slot = GUARDIAN.lock().expect("guardian mutex poisoned");
    if slot.is_none() {
        *slot = Some(GuardianClient::start()?);
    }
    operation(slot.as_mut().expect("guardian initialized"))
}

/// A child whose isolated process group is registered with the out-of-process
/// guardian. Every supervisor/router spawn must cross this boundary.
pub(crate) struct ManagedChild {
    inner: Child,
    pgid: Option<u32>,
}

impl ManagedChild {
    pub(crate) fn spawn(
        command: &mut Command,
        process_group: bool,
        explicit_env: &[(String, String)],
    ) -> Result<Self> {
        scrub_environment(command);
        command.envs(explicit_env.iter().map(|(key, value)| (key, value)));
        #[cfg(unix)]
        if process_group {
            command.process_group(0);
        }
        let registration = if process_group {
            let registration = with_guardian(|guardian| {
                guardian.writer.try_clone().context("clone guardian writer")
            })?;
            let registration_fd = registration.as_raw_fd();
            // SAFETY: valid cloned fd; CLOEXEC preserves the transient-writer
            // handoff through pre_exec but prevents participant inheritance.
            if unsafe { libc::fcntl(registration_fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .context("mark guardian writer close-on-exec");
            }
            // Register from the post-fork/pre-exec child: the inherited transient
            // writer prevents EOF until the ADD record is atomically visible.
            // SAFETY: the closure calls only async-signal-safe getpid/write and
            // writes one fixed record smaller than PIPE_BUF.
            unsafe {
                command.pre_exec(move || {
                    let pid = libc::getpid();
                    let mut record = [0_u8; 9];
                    record[0] = b'+';
                    record[1..].copy_from_slice(&(pid as u64).to_ne_bytes());
                    let written = libc::write(
                        registration_fd,
                        record.as_ptr().cast::<libc::c_void>(),
                        record.len(),
                    );
                    if written == record.len() as isize {
                        Ok(())
                    } else {
                        Err(std::io::Error::last_os_error())
                    }
                });
            }
            Some(registration)
        } else {
            None
        };
        let mut inner = command.spawn().context("spawn managed child")?;
        let pgid = process_group.then(|| inner.id()).flatten();
        if let Some(pgid) = pgid {
            let mut pre_exec_record = [0_u8; 9];
            pre_exec_record[0] = b'+';
            pre_exec_record[1..].copy_from_slice(&u64::from(pgid).to_ne_bytes());
            let confirmation = with_guardian(|guardian| {
                guardian.confirm(pre_exec_record)?;
                guardian.record(b'+', pgid)
            });
            if let Err(error) = confirmation {
                // Never return a successfully spawned but uncontained child. The
                // pre-exec ADD closes the supervisor-death race; this parent-side
                // idempotent ADD also confirms the long-lived guardian channel is
                // writable before ownership escapes this boundary.
                let _ = inner.start_kill();
                return Err(error).context("confirm managed process group registration");
            }
        }
        drop(registration);
        Ok(Self { inner, pgid })
    }

    pub(crate) async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let status = self.inner.wait().await;
        if status.is_ok() {
            self.unregister();
        }
        status
    }

    pub(crate) async fn kill(&mut self) -> std::io::Result<()> {
        self.inner.kill().await
    }

    fn unregister(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            if let Err(error) = with_guardian(|guardian| guardian.record(b'-', pgid)) {
                tracing::warn!(pgid, %error, "failed to unregister managed process group");
            }
        }
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        self.unregister();
    }
}

impl Deref for ManagedChild {
    type Target = Child;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for ManagedChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

pub(crate) fn scrub_environment(command: &mut Command) {
    for key in [
        "NOTIFY_SOCKET",
        "WATCHDOG_USEC",
        "WATCHDOG_PID",
        "LISTEN_FDS",
        "LISTEN_PID",
        "LISTEN_FDNAMES",
        "INVOCATION_ID",
        "JOURNAL_STREAM",
        "PHOXAL_PROJECT_LOCK_FD",
    ] {
        command.env_remove(key);
    }
}

pub(crate) fn materialize_plan_binaries(
    project_root: &std::path::Path,
    revision: &phoxal_cli_core::project::launch_plan::PlanRevision,
    specs: &mut [crate::supervisor::ParticipantSpec],
) -> Result<()> {
    for spec in specs {
        if !spec.executable.is_file() {
            continue;
        }
        let bytes = std::fs::read(&spec.executable)
            .with_context(|| format!("read planned binary {}", spec.executable.display()))?;
        let identity = content_identity(&bytes);
        let suffix = spec
            .executable
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("participant");
        #[cfg(target_os = "macos")]
        if let Some(path) = materialize_macos_app_binary(
            project_root,
            revision,
            &spec.executable,
            &identity,
            &bytes,
        )? {
            spec.executable = path;
            continue;
        }
        let name = format!("{identity}-{suffix}");
        let path = revision.publish_content(project_root, &name, &bytes)?;
        std::fs::set_permissions(&path, std::fs::metadata(&spec.executable)?.permissions())?;
        spec.executable = path;
    }
    Ok(())
}

fn content_identity(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(target_os = "macos")]
fn materialize_macos_app_binary(
    project_root: &std::path::Path,
    revision: &phoxal_cli_core::project::launch_plan::PlanRevision,
    executable: &std::path::Path,
    identity: &str,
    bytes: &[u8],
) -> Result<Option<std::path::PathBuf>> {
    let Some(bundle_root) = executable
        .ancestors()
        .find(|path| path.extension().is_some_and(|extension| extension == "app"))
    else {
        return Ok(None);
    };
    let relative = executable.strip_prefix(bundle_root)?;
    let mut components = relative.components();
    if components.next().and_then(|part| part.as_os_str().to_str()) != Some("Contents")
        || components.next().and_then(|part| part.as_os_str().to_str()) != Some("MacOS")
        || components.next().is_none()
        || components.next().is_some()
    {
        return Ok(None);
    }

    let bundle_name = bundle_root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Participant.app");
    let materialized_root =
        revision.content_path(project_root, &format!("{identity}-{bundle_name}"));
    let materialized_contents = materialized_root.join("Contents");
    let materialized_macos = materialized_contents.join("MacOS");
    std::fs::create_dir_all(&materialized_macos)?;

    let source_contents = bundle_root.join("Contents");
    for entry in std::fs::read_dir(&source_contents)? {
        let entry = entry?;
        if entry.file_name() == "MacOS" {
            for macos_entry in std::fs::read_dir(entry.path())? {
                let macos_entry = macos_entry?;
                let destination = materialized_macos.join(macos_entry.file_name());
                if macos_entry.path() == executable {
                    publish_immutable_file(
                        &destination,
                        bytes,
                        std::fs::metadata(executable)?.permissions(),
                    )?;
                } else {
                    publish_immutable_symlink(&destination, &macos_entry.path())?;
                }
            }
        } else {
            publish_immutable_symlink(
                &materialized_contents.join(entry.file_name()),
                &entry.path(),
            )?;
        }
    }
    Ok(Some(materialized_root.join(relative)))
}

#[cfg(target_os = "macos")]
fn publish_immutable_file(
    path: &std::path::Path,
    bytes: &[u8],
    permissions: std::fs::Permissions,
) -> Result<()> {
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => {
            file.write_all(bytes)?;
            file.sync_all()?;
            std::fs::set_permissions(path, permissions)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            anyhow::ensure!(
                std::fs::read(path)? == bytes,
                "immutable plan content collision at {}",
                path.display()
            );
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn publish_immutable_symlink(path: &std::path::Path, target: &std::path::Path) -> Result<()> {
    match std::os::unix::fs::symlink(target, path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            anyhow::ensure!(
                std::fs::read_link(path)? == target,
                "immutable plan symlink collision at {}",
                path.display()
            );
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

/// Hidden guardian entry point. EOF means the supervisor and every transient
/// writer are gone, so all still-registered process groups are killed.
pub fn maybe_run_guardian() -> Option<std::process::ExitCode> {
    let mut args = std::env::args();
    let _exe = args.next();
    if args.next().as_deref() != Some("__graph-guardian") {
        return None;
    }
    let result = (|| -> Result<()> {
        let fd: RawFd = args.next().context("missing guardian pipe fd")?.parse()?;
        let acknowledgement_fd: RawFd = args
            .next()
            .context("missing guardian acknowledgement pipe fd")?
            .parse()?;
        if args.next().is_some() {
            bail!("unexpected guardian argument");
        }
        // SAFETY: the supervisor passed ownership of this inherited read fd.
        let reader = unsafe { std::fs::File::from_raw_fd(fd) };
        let acknowledgement_writer = unsafe { std::fs::File::from_raw_fd(acknowledgement_fd) };
        guard_reader(reader, Some(acknowledgement_writer))
    })();
    Some(if result.is_ok() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::from(2)
    })
}

fn guard_reader(
    mut reader: std::fs::File,
    mut acknowledgement_writer: Option<std::fs::File>,
) -> Result<()> {
    let mut groups = std::collections::BTreeSet::<u32>::new();
    loop {
        let mut record = [0_u8; 9];
        match reader.read_exact(&mut record) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error.into()),
        }
        let pgid = u64::from_ne_bytes(record[1..].try_into().expect("fixed record")) as u32;
        match record[0] {
            b'+' => {
                groups.insert(pgid);
            }
            b'-' => {
                groups.remove(&pgid);
            }
            _ => bail!("invalid guardian operation"),
        }
        if let Some(writer) = acknowledgement_writer.as_mut() {
            writer
                .write_all(&record)
                .context("write guardian acknowledgement")?;
            writer.flush().context("flush guardian acknowledgement")?;
        }
    }
    for pgid in groups {
        // SAFETY: negative pid targets the registered process group.
        unsafe { libc::kill(-(pgid as i32), libc::SIGKILL) };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planned_binary_identity_changes_only_with_its_bytes() {
        assert_eq!(content_identity(b"first"), content_identity(b"first"));
        assert_ne!(content_identity(b"first"), content_identity(b"second"));
    }

    #[test]
    fn guardian_confirm_buffers_concurrent_out_of_order_acknowledgements() -> Result<()> {
        let mut control_fds = [0_i32; 2];
        let mut acknowledgement_fds = [0_i32; 2];
        if unsafe { libc::pipe(control_fds.as_mut_ptr()) } != 0
            || unsafe { libc::pipe(acknowledgement_fds.as_mut_ptr()) } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let writer = unsafe { std::fs::File::from_raw_fd(control_fds[1]) };
        let _control_reader = unsafe { std::fs::File::from_raw_fd(control_fds[0]) };
        let acknowledgements = unsafe { std::fs::File::from_raw_fd(acknowledgement_fds[0]) };
        let mut acknowledgement_writer =
            unsafe { std::fs::File::from_raw_fd(acknowledgement_fds[1]) };
        let first = [b'+', 1, 0, 0, 0, 0, 0, 0, 0];
        let second = [b'+', 2, 0, 0, 0, 0, 0, 0, 0];
        acknowledgement_writer.write_all(&second)?;
        acknowledgement_writer.write_all(&first)?;
        drop(acknowledgement_writer);

        let mut guardian = GuardianClient {
            writer,
            acknowledgements,
            pending_acknowledgements: std::collections::BTreeMap::new(),
            _process: None,
        };
        guardian.confirm(first)?;
        guardian.confirm(second)?;
        assert!(guardian.pending_acknowledgements.is_empty());
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn app_bundle_binary_keeps_bundle_layout_and_immutable_bytes() -> Result<()> {
        use phoxal_cli_core::project::launch_plan::{LaunchMode, LaunchPlan, PlanRevision};

        let temp = tempfile::tempdir()?;
        let bundle = temp.path().join("Source.app");
        let executable = bundle.join("Contents/MacOS/webots");
        let resources = bundle.join("Contents/Resources");
        std::fs::create_dir_all(executable.parent().expect("executable parent"))?;
        std::fs::create_dir_all(&resources)?;
        std::fs::write(&executable, b"recorded-webots")?;
        std::fs::write(resources.join("marker"), b"bundle-resource")?;
        let revision = PlanRevision::compile(
            1,
            LaunchPlan {
                mode: LaunchMode::Run,
                site: Vec::new(),
                robots: Vec::new(),
            },
        )?;
        let identity = hex::encode(Sha256::digest(executable.as_os_str().as_encoded_bytes()));

        let materialized = materialize_macos_app_binary(
            temp.path(),
            &revision,
            &executable,
            &identity,
            b"recorded-webots",
        )?
        .expect("app binary is recognized");
        assert_eq!(std::fs::read(&materialized)?, b"recorded-webots");
        assert_eq!(
            std::fs::read_link(
                materialized
                    .parent()
                    .expect("MacOS")
                    .parent()
                    .expect("Contents")
                    .join("Resources")
            )?,
            resources
        );

        std::fs::write(&executable, b"overwritten-webots")?;
        assert!(
            materialize_macos_app_binary(
                temp.path(),
                &revision,
                &executable,
                &identity,
                b"overwritten-webots",
            )
            .is_err()
        );
        assert_eq!(std::fs::read(materialized)?, b"recorded-webots");
        Ok(())
    }

    #[tokio::test]
    async fn managed_child_strips_supervisor_bootstrap_env_but_preserves_display() -> Result<()> {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("test -z \"$NOTIFY_SOCKET\" && test \"$DISPLAY\" = preserved")
            .env("NOTIFY_SOCKET", "forbidden")
            .env("DISPLAY", "preserved")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = ManagedChild::spawn(&mut command, true, &[])?;
        assert!(child.wait().await?.success());
        Ok(())
    }

    #[test]
    fn guardian_eof_kills_a_registered_process_group() -> Result<()> {
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 30 & wait")
            .process_group(0)
            .spawn()?;
        let pid = child.id();
        let mut fds = [0_i32; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let reader = unsafe { std::fs::File::from_raw_fd(fds[0]) };
        let mut writer = unsafe { std::fs::File::from_raw_fd(fds[1]) };
        let guard = std::thread::spawn(move || guard_reader(reader, None));
        let mut record = [0_u8; 9];
        record[0] = b'+';
        record[1..].copy_from_slice(&u64::from(pid).to_ne_bytes());
        writer.write_all(&record)?;
        drop(writer);
        guard.join().expect("guardian thread panicked")?;
        let status = child.wait()?;
        assert!(!status.success());
        Ok(())
    }
}
