//! Process-group termination and forced shutdown.

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use std::time::Duration;
use std::time::Instant;
use tokio::process::Child;
use tokio::time::timeout;

pub(crate) async fn stop_child(child: &mut Child, budget: Duration) -> Result<()> {
    let pid = child.id();
    if let Some(pid) = pid {
        let terminate_result = send_process_group_terminate(pid);
        if let Err(error) = terminate_result {
            tracing::warn!(pid, error = %error, "failed to send SIGTERM");
        }
    }
    match timeout(budget, child.wait()).await {
        Ok(status) => {
            status.context("failed to wait for child")?;
            if let Some(pid) = pid {
                ensure_process_group_stopped(pid).await?;
            }
            Ok(())
        }
        Err(_) => {
            kill_child_process_group(child).await?;
            Ok(())
        }
    }
}

pub(crate) async fn kill_child_process_group(child: &mut Child) -> Result<()> {
    let pid = child.id().context("process group leader has no pid")?;
    send_process_group_signal(pid, libc::SIGKILL)?;
    child
        .wait()
        .await
        .context("failed to wait for process group leader after SIGKILL")?;
    wait_for_process_group_exit(pid).await
}

pub(crate) async fn ensure_process_group_stopped(pid: u32) -> Result<()> {
    if process_group_alive(pid)? {
        send_process_group_signal(pid, libc::SIGKILL)?;
        wait_for_process_group_exit(pid).await?;
    }
    Ok(())
}

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

pub(crate) fn send_process_group_signal(pid: u32, signal: libc::c_int) -> Result<()> {
    let process_group =
        i32::try_from(pid).context("child pid does not fit in a process-group id")?;
    if unsafe { libc::kill(-process_group, signal) } == 0 {
        return Ok(());
    }
    Err(std::io::Error::last_os_error()).context("failed to signal child process group")
}

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
    send_process_group_signal(pid, libc::SIGTERM)
}
