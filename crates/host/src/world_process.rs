//! Generic ownership of one local world-adapter host process.
//!
//! The CLI knows only the fixed host ABI and its one ready bootstrap line. The
//! adapter owns Webots, native controllers, mutable world state, registration,
//! evidence, and cleanup beneath that process boundary.

use std::io::{BufRead, BufReader};
use std::num::NonZeroI32;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};

use crate::world::{
    EVIDENCE_DIR_ENV, LOG_BYTE_LIMIT_ENV, REGISTRY_DIR_ENV, WorldPaths, parse_instance_id,
};

const READY_BUDGET: Duration = Duration::from_secs(5 * 60);
const GRACEFUL_BUDGET: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// A host that is Ready but still transaction-owned by this invocation.
/// Dropping it before [`Self::detach`] rolls the whole native process group
/// back.
pub struct LaunchedWorldHost {
    child: Option<Child>,
    process_group: Option<NonZeroI32>,
}

impl LaunchedWorldHost {
    fn from_spawned(mut child: Child) -> Result<Self> {
        let process_group = match libc::pid_t::try_from(child.id())
            .ok()
            .and_then(NonZeroI32::new)
        {
            Some(process_group) => process_group,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("world host PID does not fit a positive Unix process-group ID");
            }
        };
        Ok(Self {
            child: Some(child),
            process_group: Some(process_group),
        })
    }

    fn child_mut(&mut self) -> Result<&mut Child> {
        self.child
            .as_mut()
            .context("world host child ownership was already released")
    }

    /// Release launch ownership after the operation has reached its commit
    /// point. The host remains discoverable and stoppable through its typed
    /// session plus live registration.
    pub fn detach(mut self) {
        self.process_group = None;
        if let Some(child) = self.child.take() {
            reap_in_background(child);
        }
    }

    /// Roll back a host that this invocation still owns.
    pub async fn stop(mut self) -> Result<()> {
        self.stop_owned().await?;
        if let Some(child) = self.child.take() {
            reap_in_background(child);
        }
        Ok(())
    }

    async fn stop_owned(&mut self) -> Result<()> {
        let Some(process_group) = self.process_group else {
            return Ok(());
        };
        stop_process_group(self.child_mut()?, process_group).await?;
        self.process_group = None;
        Ok(())
    }
}

impl Drop for LaunchedWorldHost {
    fn drop(&mut self) {
        if let Some(process_group) = self.process_group.take() {
            let _ = signal_process_group(process_group, libc::SIGKILL);
        }
        if let Some(child) = self.child.take() {
            reap_in_background(child);
        }
    }
}

fn reap_in_background(child: Child) {
    let pid = child.id();
    let child = std::sync::Arc::new(std::sync::Mutex::new(child));
    let background = std::sync::Arc::clone(&child);
    let result = std::thread::Builder::new()
        .name(format!("world-host-reaper-{pid}"))
        .spawn(move || {
            let mut child = background
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Err(error) = child.wait() {
                tracing::warn!(pid, %error, "failed to reap the world host");
            }
        });
    if let Err(error) = result {
        tracing::warn!(pid, %error, "failed to start the world host reaper");
        let mut child = child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Err(error) = child.wait() {
            tracing::warn!(pid, %error, "failed to reap the world host synchronously");
        }
    }
}

struct BootstrapLog {
    path: Option<std::path::PathBuf>,
}

impl BootstrapLog {
    fn new(path: std::path::PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn path(&self) -> Result<&Path> {
        self.path
            .as_deref()
            .context("world host bootstrap log was already published")
    }

    fn publish(&mut self, destination: &Path) -> Result<()> {
        let source = self.path()?;
        std::fs::rename(source, destination).with_context(|| {
            format!(
                "failed to publish world host log {} as {}",
                source.display(),
                destination.display()
            )
        })?;
        self.path = None;
        Ok(())
    }
}

impl Drop for BootstrapLog {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "failed to remove world bootstrap log residue");
            }
        }
    }
}

/// Launch the exact-train adapter host and wait for its sole ready line.
///
/// The line is emitted only after the host has persisted its closed bundle,
/// become Ready/Paused, and atomically created the live registration.
pub async fn launch(
    executable: &Path,
    world_bundle: &Path,
    paths: &WorldPaths,
    log_byte_limit: u64,
) -> Result<(String, LaunchedWorldHost)> {
    let temporary_log = tempfile::Builder::new()
        .prefix(".starting-")
        .suffix(".host.log")
        .tempfile_in(paths.evidence())
        .context("failed to create the world host bootstrap log")?;
    let (log, temporary_log_path) = temporary_log
        .keep()
        .context("failed to retain the world host bootstrap log")?;
    let mut bootstrap_log = BootstrapLog::new(temporary_log_path);

    let mut command = Command::new(executable);
    command
        .arg("--world-bundle")
        .arg(world_bundle)
        .env(REGISTRY_DIR_ENV, paths.registry())
        .env(EVIDENCE_DIR_ENV, paths.evidence())
        .env(LOG_BYTE_LIMIT_ENV, log_byte_limit.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(log));
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command
        .spawn()
        .with_context(|| format!("failed to launch world host {}", executable.display()))?;
    let mut host = LaunchedWorldHost::from_spawned(child)?;
    let stdout = host
        .child_mut()?
        .stdout
        .take()
        .context("world host stdout pipe was not created")?;
    let ready = tokio::task::spawn_blocking(move || {
        let mut line = String::new();
        let read = BufReader::new(stdout).read_line(&mut line)?;
        Ok::<_, std::io::Error>((read, line))
    });

    let line = tokio::select! {
        ready = tokio::time::timeout(READY_BUDGET, ready) => {
            match ready {
                Ok(Ok(Ok((read, line)))) if read > 0 => line,
                Ok(Ok(Ok(_))) => {
                    let status = host.child_mut()?.try_wait()?;
                    let error = bootstrap_failure("world host exited before publishing readiness", status, bootstrap_log.path()?);
                    host.stop_owned().await?;
                    return Err(error);
                }
                Ok(Ok(Err(error))) => {
                    host.stop_owned().await?;
                    return Err(error).context("failed to read the world host ready line");
                }
                Ok(Err(error)) => {
                    host.stop_owned().await?;
                    return Err(error).context("world host ready reader failed");
                }
                Err(_) => {
                    let error = bootstrap_failure("timed out waiting for the world host to become Ready and Paused", host.child_mut()?.try_wait()?, bootstrap_log.path()?);
                    host.stop_owned().await?;
                    return Err(error);
                }
            }
        }
        interrupted = tokio::signal::ctrl_c() => {
            if let Err(error) = interrupted {
                tracing::debug!(%error, "failed to observe world-start interrupt");
            }
            host.stop_owned().await?;
            bail!("world start was interrupted before readiness; the new world was rolled back");
        }
    };

    let instance = line
        .strip_suffix('\n')
        .context("world host ready output was not one newline-terminated instance ID")?;
    ensure!(
        !instance.contains('\r') && !instance.contains('\n'),
        "world host ready output contained more than one line"
    );
    parse_instance_id(instance).context("world host emitted an invalid instance ID")?;

    let evidence = paths.evidence_path(instance);
    ensure!(
        evidence.is_dir(),
        "world host reported readiness before creating {}",
        evidence.display()
    );
    let host_log = evidence.join("host.log");
    bootstrap_log.publish(&host_log)?;

    Ok((instance.to_string(), host))
}

fn bootstrap_failure(
    summary: &str,
    status: Option<std::process::ExitStatus>,
    log: &Path,
) -> anyhow::Error {
    let status = status.map_or_else(|| "still running".to_string(), |status| status.to_string());
    let diagnostics = tail(log, 8 * 1024).unwrap_or_default();
    let diagnostics = diagnostics.trim();
    if diagnostics.is_empty() {
        anyhow::anyhow!("{summary} ({status}); bootstrap log: {}", log.display())
    } else {
        anyhow::anyhow!("{summary} ({status}): {diagnostics}")
    }
}

fn tail(path: &Path, limit: usize) -> Result<String> {
    let bytes = std::fs::read(path)?;
    let start = bytes.len().saturating_sub(limit);
    Ok(String::from_utf8_lossy(&bytes[start..]).into_owned())
}

async fn stop_process_group(child: &mut Child, process_group: NonZeroI32) -> Result<()> {
    for (signal, budget) in [
        (libc::SIGTERM, GRACEFUL_BUDGET),
        (libc::SIGKILL, Duration::from_secs(1)),
    ] {
        signal_process_group(process_group, signal)?;
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            if !process_group_alive(child, process_group)? {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            tokio::time::sleep(POLL_INTERVAL.min(remaining)).await;
        }
    }
    bail!("world host process group remained alive after SIGKILL")
}

fn signal_process_group(process_group: NonZeroI32, signal: libc::c_int) -> Result<()> {
    // SAFETY: `kill` takes no pointer. The negative PID targets the exact
    // process group created for the direct adapter-host child.
    if unsafe { libc::kill(-process_group.get(), signal) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error).context("failed to signal the world host process group")
}

fn process_group_alive(child: &mut Child, process_group: NonZeroI32) -> Result<bool> {
    let _ = child.try_wait()?;
    // SAFETY: `kill` takes no pointer. Signal zero only probes the group.
    if unsafe { libc::kill(-process_group.get(), 0) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(error).context("failed to inspect the world host process group"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn spawn_group(program: &str, arguments: &[&str]) -> Child {
        use std::os::unix::process::CommandExt;

        let mut command = Command::new(program);
        command
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.process_group(0);
        command.spawn().unwrap()
    }

    #[cfg(unix)]
    fn wait_for_group_exit(pid: u32) {
        let pid = libc::pid_t::try_from(pid).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            // SAFETY: signal zero only probes the process group.
            if unsafe { libc::kill(-pid, 0) } != 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "process group {pid} was not reaped"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn fixed_host_abi_contains_only_the_world_bundle_argument() {
        let mut command = Command::new("phoxal-simulator-webots-host");
        command.arg("--world-bundle").arg("/tmp/world-bundle");
        assert_eq!(
            command
                .get_args()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            ["--world-bundle", "/tmp/world-bundle"]
        );
    }

    #[test]
    fn bootstrap_tail_is_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let log = directory.path().join("host.log");
        std::fs::write(&log, b"0123456789").unwrap();
        assert_eq!(tail(&log, 4).unwrap(), "6789");
    }

    #[test]
    fn unpublished_bootstrap_logs_are_removed_but_published_logs_remain() {
        let directory = tempfile::tempdir().unwrap();
        let abandoned = directory.path().join(".starting-Ab12Cd.host.log");
        std::fs::write(&abandoned, b"bootstrap failed").unwrap();
        drop(BootstrapLog::new(abandoned.clone()));
        assert!(!abandoned.exists());

        let source = directory.path().join(".starting-Ef34Gh.host.log");
        let published = directory.path().join("host.log");
        std::fs::write(&source, b"ready").unwrap();
        let mut log = BootstrapLog::new(source);
        log.publish(&published).unwrap();
        drop(log);
        assert_eq!(std::fs::read(&published).unwrap(), b"ready");
    }

    #[cfg(unix)]
    #[test]
    fn dropping_an_uncommitted_host_kills_and_reaps_its_group() {
        let child = spawn_group("sleep", &["120"]);
        let pid = child.id();
        drop(LaunchedWorldHost::from_spawned(child).unwrap());
        wait_for_group_exit(pid);
    }

    #[cfg(unix)]
    #[test]
    fn detaching_a_committed_host_reaps_its_eventual_exit() {
        let child = spawn_group("sh", &["-c", "exit 0"]);
        let pid = child.id();
        LaunchedWorldHost::from_spawned(child).unwrap().detach();
        wait_for_group_exit(pid);
    }
}
