//! Infrastructure-router process launch and teardown.

use super::readiness::wait_for_router_connection;
use crate::ManagedChild;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Debug, Clone)]
pub(super) struct RouterLaunch {
    pub(super) binary: PathBuf,
    pub(super) config: Option<PathBuf>,
}

pub(super) struct RouterProcess {
    pub(super) child: ManagedChild,
    pub(super) stdout_task: tokio::task::JoinHandle<()>,
    pub(super) stderr_task: tokio::task::JoinHandle<()>,
}

struct RouterLaunchAttempt {
    child: Option<ManagedChild>,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
}

impl RouterLaunchAttempt {
    fn new(child: ManagedChild, stderr_task: tokio::task::JoinHandle<()>) -> Self {
        Self {
            child: Some(child),
            stderr_task: Some(stderr_task),
        }
    }

    async fn stop_failed(mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        if let Some(stderr_task) = self.stderr_task.take() {
            stderr_task.abort();
        }
    }

    fn finish(mut self, stdout_task: tokio::task::JoinHandle<()>) -> RouterProcess {
        RouterProcess {
            child: self.child.take().expect("launch attempt owns its child"),
            stdout_task,
            stderr_task: self
                .stderr_task
                .take()
                .expect("launch attempt owns its stderr task"),
        }
    }
}

impl Drop for RouterLaunchAttempt {
    fn drop(&mut self) {
        // A session cancellation may drop the launch future at any await.
        // Keep failed/cancelled attempts from detaching their stderr pump or
        // leaving a router child alive even when no explicit error path runs.
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
        if let Some(stderr_task) = self.stderr_task.take() {
            stderr_task.abort();
        }
    }
}

impl RouterProcess {
    pub(super) async fn stop(&mut self) {
        if let Some(pid) = self.child.id() {
            // SAFETY: `pid` is the live child id returned by Tokio. SIGTERM
            // lets the router close its Zenoh session before the bounded
            // fallback below forces termination.
            let _ = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        }
        if tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
            let _ = self.child.wait().await;
        }
        self.stdout_task.abort();
        self.stderr_task.abort();
    }
}

pub(super) async fn launch_router_process(
    launch: &RouterLaunch,
    endpoint: &str,
) -> Result<RouterProcess> {
    let mut command = tokio::process::Command::new(&launch.binary);
    if let Some(config) = &launch.config {
        command.arg("--config").arg(config);
    }
    command.arg("--listen").arg(endpoint);
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = ManagedChild::spawn(&mut command, true, &[])
        .context("failed to launch phoxal-infrastructure-router")?;
    let stdout = child
        .stdout
        .take()
        .context("router stdout was not captured")?;
    let stderr = child
        .stderr
        .take()
        .context("router stderr was not captured")?;
    let stderr_tail = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let task_stderr_tail = std::sync::Arc::clone(&stderr_tail);
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::info!(target: "infrastructure_router", "{line}");
            if let Ok(mut tail) = task_stderr_tail.lock() {
                tail.push_str(&line);
                tail.push('\n');
                if tail.len() > 8_192 {
                    let mut drain = tail.len() - 8_192;
                    while !tail.is_char_boundary(drain) {
                        drain += 1;
                    }
                    tail.drain(..drain);
                }
            }
        }
    });
    let mut attempt = RouterLaunchAttempt::new(child, stderr_task);
    if let Err(error) = wait_for_router_connection(
        attempt
            .child
            .as_mut()
            .expect("launch attempt owns its child"),
        endpoint,
        &stderr_tail,
    )
    .await
    {
        attempt.stop_failed().await;
        return Err(error);
    }
    let mut lines = BufReader::new(stdout).lines();
    let stdout_task = tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::info!(target: "infrastructure_router", "{line}");
        }
    });
    Ok(attempt.finish(stdout_task))
}
