//! Central child launch boundary and crash-containment guardian.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::ops::{Deref, DerefMut};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt as _;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use tokio::process::{Child, Command};

static GUARDIAN: LazyLock<Mutex<Option<GuardianClient>>> = LazyLock::new(|| Mutex::new(None));
static SPAWN_GATE: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static NEXT_GUARDIAN_TOKEN: AtomicU64 = AtomicU64::new(1);
const GUARDIAN_RECORD_LEN: usize = 17;
const SUPERVISOR_ONLY_ENV: [&str; 9] = [
    "NOTIFY_SOCKET",
    "WATCHDOG_USEC",
    "WATCHDOG_PID",
    "LISTEN_FDS",
    "LISTEN_PID",
    "LISTEN_FDNAMES",
    "INVOCATION_ID",
    "JOURNAL_STREAM",
    "PHOXAL_PROJECT_LOCK_FD",
];
type GuardianRecord = [u8; GUARDIAN_RECORD_LEN];

fn next_guardian_token() -> u64 {
    NEXT_GUARDIAN_TOKEN.fetch_add(1, Ordering::Relaxed)
}

fn guardian_record(operation: u8, pgid: u32, token: u64) -> GuardianRecord {
    let mut record = [0_u8; GUARDIAN_RECORD_LEN];
    record[0] = operation;
    record[1..9].copy_from_slice(&u64::from(pgid).to_ne_bytes());
    record[9..].copy_from_slice(&token.to_ne_bytes());
    record
}

fn guardian_record_pgid(record: &GuardianRecord) -> u32 {
    u64::from_ne_bytes(record[1..9].try_into().expect("fixed guardian pgid")) as u32
}

fn guardian_record_token(record: &GuardianRecord) -> u64 {
    u64::from_ne_bytes(record[9..].try_into().expect("fixed guardian token"))
}

fn clear_close_on_exec(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: callers pass a live descriptor and invoke this either before
    // spawning or from the post-fork child before exec.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn scrub_std_environment(command: &mut std::process::Command) {
    for key in SUPERVISOR_ONLY_ENV {
        command.env_remove(key);
    }
    // Every participant launch-contract variable is removed before the spec's
    // own entries are applied, so a child is configured by its spec and by
    // nothing that happened to be in the operator's shell. An ambient
    // `PHOXAL_EXECUTION_ORIGIN` is the sharp case: inherited by the Webots
    // application and through it by the controller, it would hand a clockless
    // process real-clock authority over a run it is not part of (#952 section
    // B). An ambient producer id is the same story for identity - a
    // replacement controller would inherit the producer of the one it replaced.
    for key in phoxal::participant::launch::env::ALL {
        command.env_remove(key);
    }
}

fn guardian_command(
    exe: &std::path::Path,
    control_fd: RawFd,
    acknowledgement_fd: RawFd,
) -> std::process::Command {
    let mut command = std::process::Command::new(exe);
    command
        .arg("__graph-guardian")
        .arg(control_fd.to_string())
        .arg(acknowledgement_fd.to_string())
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    scrub_std_environment(&mut command);
    command
}

pub(crate) fn create_cloexec_pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0_i32; 2];
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // SAFETY: valid pointer to a two-element fd array. pipe2 applies
        // CLOEXEC atomically, eliminating the concurrent-fork inheritance
        // window on supported targets.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        // Darwin does not expose pipe2. Mark both descriptors immediately;
        // the same gate covers every Phoxal child spawn so none can race this
        // fallback's pipe-to-fcntl window.
        let _spawn_gate = SPAWN_GATE.lock().expect("spawn gate poisoned");
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        for fd in fds {
            if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
                let error = std::io::Error::last_os_error();
                // SAFETY: both descriptors were returned by pipe and have not
                // transferred to an owner yet.
                unsafe {
                    libc::close(fds[0]);
                    libc::close(fds[1]);
                }
                return Err(error);
            }
        }
    }
    // SAFETY: each descriptor returned by pipe/pipe2 transfers exactly once.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

struct GuardianClient {
    writer: std::fs::File,
    acknowledgements: std::fs::File,
    pending_acknowledgements: std::collections::BTreeMap<GuardianRecord, usize>,
    _process: Option<std::process::Child>,
}

impl GuardianClient {
    fn start() -> Result<Self> {
        let (read_fd, write_fd) = create_cloexec_pipe().context("create guardian control pipe")?;
        let (acknowledgement_read_fd, acknowledgement_write_fd) =
            create_cloexec_pipe().context("create guardian acknowledgement pipe")?;
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
        let process = {
            let _spawn_gate = SPAWN_GATE.lock().expect("spawn gate poisoned");
            let control_fd = read_fd.as_raw_fd();
            let acknowledgement_fd = acknowledgement_write_fd.as_raw_fd();
            let mut command = guardian_command(
                &exe,
                read_fd.as_raw_fd(),
                acknowledgement_write_fd.as_raw_fd(),
            );
            // SAFETY: this post-fork hook calls only fcntl. The spawn gate
            // prevents any unrelated child from inheriting these descriptors,
            // while the parent keeps its CLOEXEC defaults.
            unsafe {
                command.pre_exec(move || {
                    clear_close_on_exec(control_fd)?;
                    clear_close_on_exec(acknowledgement_fd)
                });
            }
            command.spawn().context("spawn graph guardian")?
        };
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
        let record = guardian_record(operation, pgid, next_guardian_token());
        self.writer
            .write_all(&record)
            .context("write graph guardian record")?;
        self.writer.flush().context("flush graph guardian record")?;
        self.confirm(record)
    }

    fn confirm(&mut self, expected: GuardianRecord) -> Result<()> {
        if let Some(count) = self.pending_acknowledgements.get_mut(&expected) {
            *count -= 1;
            if *count == 0 {
                self.pending_acknowledgements.remove(&expected);
            }
            return Ok(());
        }
        loop {
            let mut acknowledgement = [0_u8; GUARDIAN_RECORD_LEN];
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

    fn confirm_token_with_timeout(
        &mut self,
        operation: u8,
        token: u64,
        timeout: std::time::Duration,
    ) -> Result<Option<GuardianRecord>> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(record) = self
                .pending_acknowledgements
                .keys()
                .find(|record| record[0] == operation && guardian_record_token(record) == token)
                .copied()
            {
                self.confirm(record)?;
                return Ok(Some(record));
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let mut poll_fd = libc::pollfd {
                fd: self.acknowledgements.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let timeout_ms = i32::try_from(remaining.as_millis().max(1)).unwrap_or(i32::MAX);
            // SAFETY: poll_fd points to one initialized descriptor for the
            // duration of the call.
            let ready = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
            if ready == 0 {
                return Ok(None);
            }
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error).context("poll graph guardian acknowledgement");
            }
            let mut acknowledgement = [0_u8; GUARDIAN_RECORD_LEN];
            self.acknowledgements
                .read_exact(&mut acknowledgement)
                .context("read graph guardian acknowledgement")?;
            if acknowledgement[0] == operation && guardian_record_token(&acknowledgement) == token {
                return Ok(Some(acknowledgement));
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
            let token = next_guardian_token();
            // Register from the post-fork/pre-exec child: the inherited transient
            // writer prevents EOF until the ADD record is atomically visible.
            // SAFETY: the closure calls only async-signal-safe getpid/write and
            // writes one fixed record smaller than PIPE_BUF.
            unsafe {
                command.pre_exec(move || {
                    let pid = libc::getpid();
                    let record = guardian_record(b'+', pid as u32, token);
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
            Some((registration, token))
        } else {
            None
        };
        let spawn_result = {
            let _spawn_gate = SPAWN_GATE.lock().expect("spawn gate poisoned");
            command.spawn()
        };
        if let Err(spawn_error) = spawn_result {
            if let Some((_, token)) = &registration {
                with_guardian(|guardian| {
                    if let Some(acknowledged) = guardian.confirm_token_with_timeout(
                        b'+',
                        *token,
                        std::time::Duration::from_secs(1),
                    )? {
                        guardian.record(b'-', guardian_record_pgid(&acknowledged))?;
                    }
                    Ok(())
                })
                .context("roll back failed-exec guardian registration")?;
            }
            return Err(spawn_error).context("spawn managed child");
        }
        let mut inner = spawn_result.expect("handled managed child spawn error");
        let pgid = process_group.then(|| inner.id()).flatten();
        if let Some(pgid) = pgid
            && let Some((_, token)) = &registration
        {
            let pre_exec_record = guardian_record(b'+', pgid, *token);
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
        if let Some(pgid) = self.pgid.take()
            && let Err(error) = with_guardian(|guardian| guardian.record(b'-', pgid))
        {
            tracing::warn!(pgid, %error, "failed to unregister managed process group");
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
    scrub_std_environment(command.as_std_mut());
}

pub(crate) fn materialize_plan_binaries(
    project_root: &std::path::Path,
    revision: &phoxal_cli_core::project::launch_plan::PlanRevision,
    specs: &mut [crate::supervisor::ParticipantSpec],
) -> Result<()> {
    let content_root =
        phoxal_cli_core::runtime::paths::RuntimePaths::for_root(project_root).plan_content_root();
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
            &content_root,
            revision,
            &spec.executable,
            &identity,
            &bytes,
        )? {
            spec.executable = path;
            continue;
        }
        let name = format!("{identity}-{suffix}");
        let path = revision.publish_content_in(&content_root, &name, &bytes)?;
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
    content_root: &std::path::Path,
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
        revision.content_path_in(content_root, &format!("{identity}-{bundle_name}"));
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
        let mut record = [0_u8; GUARDIAN_RECORD_LEN];
        match reader.read_exact(&mut record) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error.into()),
        }
        let pgid = guardian_record_pgid(&record);
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
    fn guardian_strips_supervisor_bootstrap_environment() {
        let command = guardian_command(std::path::Path::new("/usr/bin/true"), 10, 11);
        // Keep this expectation independent from SUPERVISOR_ONLY_ENV so
        // accidentally shrinking the production scrub list fails the test.
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
            assert_eq!(
                command
                    .get_envs()
                    .find(|(candidate, _)| *candidate == std::ffi::OsStr::new(key))
                    .map(|(_, value)| value),
                Some(None),
                "{key} must be explicitly removed from the guardian environment"
            );
        }
    }

    /// #952: a managed child is configured by its spec, not by whatever the
    /// operator's shell happened to export. The Webots case is the sharp one:
    /// an ambient `PHOXAL_EXECUTION_ORIGIN` would be inherited by Webots and
    /// through it by the controller, handing a clockless process real-clock
    /// authority over a run it is not part of - and an ambient producer id
    /// would let a replacement controller reuse the identity of the one it
    /// replaced.
    #[test]
    fn a_managed_child_inherits_no_ambient_launch_contract_variable() {
        let mut command = std::process::Command::new("/usr/bin/true");
        scrub_std_environment(&mut command);
        let explicit = [(
            phoxal::participant::launch::env::EXECUTION_ID.to_string(),
            "x0123456789abcdef0123456789abcdef".to_string(),
        )];
        command.envs(explicit.iter().map(|(key, value)| (key, value)));

        let effective = |key: &str| {
            command
                .get_envs()
                .find(|(candidate, _)| *candidate == std::ffi::OsStr::new(key))
                .map(|(_, value)| value)
        };
        for key in phoxal::participant::launch::env::ALL {
            if *key == phoxal::participant::launch::env::EXECUTION_ID {
                continue;
            }
            assert_eq!(
                effective(key),
                Some(None),
                "{key} must be removed from a managed child's inherited environment"
            );
        }
        assert_eq!(
            effective(phoxal::participant::launch::env::EXECUTION_ID)
                .flatten()
                .map(std::ffi::OsStr::to_string_lossy),
            Some("x0123456789abcdef0123456789abcdef".into()),
            "the spec's own entries are applied after the scrub, not before it"
        );
    }

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
        let first = guardian_record(b'+', 1, 11);
        let second = guardian_record(b'+', 2, 22);
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

    #[test]
    fn guardian_descriptor_can_be_made_inheritable_for_exec() -> Result<()> {
        let (read_fd, _write_fd) = create_cloexec_pipe()?;
        let before = unsafe { libc::fcntl(read_fd.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(before, -1);
        assert_ne!(before & libc::FD_CLOEXEC, 0);

        clear_close_on_exec(read_fd.as_raw_fd())?;

        let after = unsafe { libc::fcntl(read_fd.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(after, -1);
        assert_eq!(after & libc::FD_CLOEXEC, 0);
        Ok(())
    }

    #[test]
    fn guardian_confirm_token_recovers_failed_exec_registration_identity() -> Result<()> {
        let (control_reader, control_writer) = create_cloexec_pipe()?;
        let (acknowledgement_reader, acknowledgement_writer) = create_cloexec_pipe()?;
        let writer = std::fs::File::from(control_writer);
        let _control_reader = std::fs::File::from(control_reader);
        let acknowledgements = std::fs::File::from(acknowledgement_reader);
        let mut acknowledgement_writer = std::fs::File::from(acknowledgement_writer);
        let unrelated = guardian_record(b'+', 41, 1);
        let failed_exec = guardian_record(b'+', 42, 2);
        acknowledgement_writer.write_all(&unrelated)?;
        acknowledgement_writer.write_all(&failed_exec)?;

        let mut guardian = GuardianClient {
            writer,
            acknowledgements,
            pending_acknowledgements: std::collections::BTreeMap::new(),
            _process: None,
        };
        assert_eq!(
            guardian.confirm_token_with_timeout(b'+', 2, std::time::Duration::from_secs(1))?,
            Some(failed_exec)
        );
        guardian.confirm(unrelated)?;
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
}
