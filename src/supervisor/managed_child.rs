//! Central child launch boundary and crash-containment guardian.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::ops::{Deref, DerefMut};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::process::Stdio;
use std::sync::{LazyLock, Mutex};
use tokio::process::{Child, Command};

static GUARDIAN: LazyLock<Mutex<Option<GuardianClient>>> = LazyLock::new(|| Mutex::new(None));

struct GuardianClient {
    writer: std::fs::File,
    _process: Option<std::process::Child>,
}

impl GuardianClient {
    fn start() -> Result<Self> {
        let mut fds = [0_i32; 2];
        // SAFETY: valid pointer to a two-element fd array.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error()).context("create guardian pipe");
        }
        let (read_fd, write_fd) = (fds[0], fds[1]);
        // The guardian inherits only the read end. The writer is CLOEXEC so
        // participants cannot keep containment alive after the supervisor dies.
        // SAFETY: fds were just returned by pipe.
        unsafe { libc::fcntl(write_fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        if cfg!(test) {
            // Unit-test executables use the libtest harness, not the CLI main.
            // Keep identical EOF semantics in a dedicated thread.
            let reader = unsafe { std::fs::File::from_raw_fd(read_fd) };
            std::thread::spawn(move || guard_reader(reader));
            let writer = unsafe { std::fs::File::from_raw_fd(write_fd) };
            return Ok(Self {
                writer,
                _process: None,
            });
        }
        let exe = std::env::current_exe().context("resolve supervisor executable")?;
        let process = std::process::Command::new(exe)
            .arg("__graph-guardian")
            .arg(read_fd.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn graph guardian")?;
        // SAFETY: ownership of each raw fd is transferred exactly once.
        unsafe { libc::close(read_fd) };
        let writer = unsafe { std::fs::File::from_raw_fd(write_fd) };
        Ok(Self {
            writer,
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
        self.writer.flush().context("flush graph guardian record")
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
        let registration =
            with_guardian(|guardian| guardian.writer.try_clone().context("clone guardian writer"))?;
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
        let inner = command.spawn().context("spawn managed child")?;
        let pgid = process_group.then(|| inner.id()).flatten();
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
        let identity = hex::encode(Sha256::digest(
            spec.executable.as_os_str().as_encoded_bytes(),
        ));
        let suffix = spec
            .executable
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("participant");
        let name = format!("{identity}-{suffix}");
        let path = revision.publish_content(project_root, &name, &bytes)?;
        std::fs::set_permissions(&path, std::fs::metadata(&spec.executable)?.permissions())?;
        spec.executable = path;
    }
    Ok(())
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
        if args.next().is_some() {
            bail!("unexpected guardian argument");
        }
        // SAFETY: the supervisor passed ownership of this inherited read fd.
        let reader = unsafe { std::fs::File::from_raw_fd(fd) };
        guard_reader(reader)
    })();
    Some(if result.is_ok() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::from(2)
    })
}

fn guard_reader(mut reader: std::fs::File) -> Result<()> {
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
        let guard = std::thread::spawn(move || guard_reader(reader));
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
