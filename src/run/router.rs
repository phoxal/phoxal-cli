//! Router responsibilities for run.

use super::{ROUTER_READY_TIMEOUT, locate_tool_binary};
use crate::supervisor::BoardBackend;
use crate::supervisor::ParticipantSpec;
use crate::supervisor::SupervisionStage;
use crate::supervisor::SupervisorOptions;
use crate::supervisor::SupervisorOutcome;
use crate::supervisor::supervise_until_shutdown;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use phoxal::participant::launch::env;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::launch_plan::SITE_INFRASTRUCTURE_ROUTER;
use phoxal_cli_core::project::resolver::ResolvedRobot;
use phoxal_cli_core::project::tooling::resolve_project_path;
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;

#[derive(Debug, serde::Deserialize)]
pub(crate) struct RouterReadyEvent {
    event: String,
    listen: Vec<String>,
}

pub(crate) struct InfrastructureRouter {
    child: tokio::process::Child,
    stdout_task: tokio::task::JoinHandle<()>,
    stderr_task: tokio::task::JoinHandle<()>,
}

impl InfrastructureRouter {
    async fn stop(mut self) {
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

    pub(crate) async fn supervise(
        mut self,
        stages: Vec<SupervisionStage>,
        board: BoardBackend,
        options: SupervisorOptions,
    ) -> Result<SupervisorOutcome> {
        let token = options.token.clone();
        let supervisor = supervise_until_shutdown(stages, board, options);
        tokio::pin!(supervisor);
        tokio::select! {
            outcome = &mut supervisor => {
                self.stop().await;
                outcome
            }
            status = self.child.wait() => {
                token.cancel();
                let _ = supervisor.await;
                self.stdout_task.abort();
                self.stderr_task.abort();
                let status = status.context("failed to wait for infrastructure router")?;
                bail!("infrastructure router exited while the session was active: {status}")
            }
        }
    }
}

pub(crate) async fn start_infrastructure_router(
    resolved: &ResolvedRobot,
    project_root: &Path,
    ui: &crate::Ui,
) -> Result<(InfrastructureRouter, String)> {
    let binary = locate_tool_binary(resolved, SITE_INFRASTRUCTURE_ROUTER, ui)?
        .context("phoxal-infrastructure-router is not staged; run `phoxal update`")?;
    let mut command = tokio::process::Command::new(binary);
    if let Some(config) = &resolved.robot.router.config {
        let config = resolve_project_path(project_root, config);
        anyhow::ensure!(
            config.is_file(),
            "router.config file {} does not exist",
            config.display()
        );
        command.arg("--config").arg(config);
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
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
    let mut lines = BufReader::new(stdout).lines();
    let readiness = tokio::time::timeout(ROUTER_READY_TIMEOUT, async {
        loop {
            let line = lines
                .next_line()
                .await?
                .context("infrastructure router exited before reporting readiness")?;
            let event: RouterReadyEvent = serde_json::from_str(&line)
                .with_context(|| format!("invalid infrastructure router event: {line}"))?;
            if event.event == "ready" {
                break parse_router_ready(&line);
            }
            tracing::info!(target: "infrastructure_router", "{line}");
        }
    })
    .await
    .context("timed out waiting for infrastructure router readiness")?;
    let endpoint = match readiness {
        Ok(endpoint) => endpoint,
        Err(error) => {
            let _ = tokio::time::timeout(Duration::from_millis(100), async {
                while stderr_tail.lock().is_ok_and(|tail| tail.is_empty()) {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            let tail = stderr_tail
                .lock()
                .map(|tail| tail.clone())
                .unwrap_or_default();
            if tail.is_empty() {
                return Err(error);
            }
            return Err(error.context(format!("infrastructure router stderr:\n{tail}")));
        }
    };
    let stdout_task = tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::info!(target: "infrastructure_router", "{line}");
        }
    });
    Ok((
        InfrastructureRouter {
            child,
            stdout_task,
            stderr_task,
        },
        endpoint,
    ))
}

pub(crate) fn parse_router_ready(line: &str) -> Result<String> {
    let ready: RouterReadyEvent = serde_json::from_str(line)
        .with_context(|| format!("invalid infrastructure router readiness event: {line}"))?;
    anyhow::ensure!(
        ready.event == "ready",
        "unexpected router event {}",
        ready.event
    );
    ready
        .listen
        .first()
        .cloned()
        .context("infrastructure router reported no listener endpoint")
}

pub(crate) fn apply_session_connect(
    plan: &mut LaunchPlan,
    specs: &mut [ParticipantSpec],
    endpoint: &str,
) {
    for robot in &mut plan.robots {
        for participant in &mut robot.participants {
            participant.launch.bus.connect_endpoints = vec![endpoint.to_string()];
        }
    }
    for spec in specs {
        if let Some((_, value)) = spec.env.iter_mut().find(|(key, _)| key == env::CONNECT) {
            *value = endpoint.to_string();
        }
    }
}
