//! Process-group termination and forced shutdown.

use super::BoardBackend;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use std::time::Duration;
use std::time::Instant;
use tokio::process::Child;
use tokio::time::timeout;

/// Force-stop every process group currently recorded by the session board.
pub fn force_kill_supervised_process_groups(board: &BoardBackend) {
    for status in board.snapshot().participants.into_values() {
        let Some(pid) = status.pid else {
            continue;
        };
        #[cfg(unix)]
        if let Err(error) = send_process_group_signal(pid, libc::SIGKILL) {
            tracing::warn!(
                participant = %status.id,
                pid,
                error = %error,
                "forced process-group shutdown failed"
            );
        }
        #[cfg(not(unix))]
        tracing::warn!(
            participant = %status.id,
            pid,
            "forced process-group shutdown is unavailable on this platform"
        );
    }
}

pub async fn stop_child(child: &mut Child, budget: Duration, process_group: bool) -> Result<()> {
    let pid = child.id();
    if let Some(pid) = pid {
        let terminate_result = if process_group {
            send_process_group_terminate(pid)
        } else {
            send_terminate(pid)
        };
        if let Err(error) = terminate_result {
            tracing::warn!(pid, process_group, error = %error, "failed to send SIGTERM");
        }
    }
    match timeout(budget, child.wait()).await {
        Ok(status) => {
            status.context("failed to wait for child")?;
            if process_group && let Some(pid) = pid {
                ensure_process_group_stopped(pid).await?;
            }
            Ok(())
        }
        Err(_) => {
            if process_group {
                kill_child_process_group(child).await?;
            } else {
                child.start_kill().context("failed to kill child")?;
                child.wait().await.context("failed to wait after kill")?;
            }
            Ok(())
        }
    }
}

pub(crate) async fn kill_child_process_group(child: &mut Child) -> Result<()> {
    #[cfg(unix)]
    {
        let pid = child.id().context("process group leader has no pid")?;
        send_process_group_signal(pid, libc::SIGKILL)?;
        child
            .wait()
            .await
            .context("failed to wait for process group leader after SIGKILL")?;
        wait_for_process_group_exit(pid).await
    }
    #[cfg(not(unix))]
    {
        child.start_kill().context("failed to kill child")?;
        child.wait().await.context("failed to wait after kill")?;
        Ok(())
    }
}

#[cfg(unix)]
pub(crate) async fn ensure_process_group_stopped(pid: u32) -> Result<()> {
    if process_group_alive(pid)? {
        send_process_group_signal(pid, libc::SIGKILL)?;
        wait_for_process_group_exit(pid).await?;
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) async fn ensure_process_group_stopped(_pid: u32) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
pub(crate) async fn wait_for_process_group_exit(pid: u32) -> Result<()> {
    let kill_deadline = Instant::now() + Duration::from_secs(1);
    while process_group_alive(pid)? {
        if Instant::now() >= kill_deadline {
            bail!("child process group remained alive after SIGKILL");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn send_process_group_signal(pid: u32, signal: libc::c_int) -> Result<()> {
    let process_group =
        i32::try_from(pid).context("child pid does not fit in a process-group id")?;
    if unsafe { libc::kill(-process_group, signal) } == 0 {
        return Ok(());
    }
    Err(std::io::Error::last_os_error()).context("failed to signal child process group")
}

#[cfg(unix)]
pub(crate) fn send_process_signal(pid: u32, signal: libc::c_int) -> Result<()> {
    let process = i32::try_from(pid).context("child pid does not fit in a process id")?;
    if unsafe { libc::kill(process, signal) } == 0 {
        return Ok(());
    }
    Err(std::io::Error::last_os_error()).context("failed to signal child process")
}

#[cfg(unix)]
pub(crate) fn process_group_alive(pid: u32) -> Result<bool> {
    let process_group =
        i32::try_from(pid).context("child pid does not fit in a process-group id")?;
    if unsafe { libc::kill(-process_group, 0) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(error).context("failed to inspect child process group"),
    }
}

pub(crate) fn send_process_group_terminate(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        send_process_group_signal(pid, libc::SIGTERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        Ok(())
    }
}

pub(crate) fn send_terminate(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        send_process_signal(pid, libc::SIGTERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        Ok(())
    }
}
