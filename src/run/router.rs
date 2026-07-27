//! Router responsibilities for run.

use super::ROUTER_READY_TIMEOUT;
use crate::supervisor::{
    BoardBackend, ManagedChild, ParticipantSpec, SupervisionStage, SupervisorOptions,
    supervise_until_shutdown,
};
use anyhow::{Context, Result, bail};
use phoxal::participant::launch::env;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::tooling::resolve_project_path;
use phoxal_cli_core::session::human;
use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Debug, Clone)]
struct RouterLaunch {
    binary: PathBuf,
    config: Option<PathBuf>,
}

struct RouterProcess {
    child: ManagedChild,
    stdout_task: tokio::task::JoinHandle<()>,
    stderr_task: tokio::task::JoinHandle<()>,
}

struct RouterLaunchAttempt {
    child: Option<ManagedChild>,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Debug, Clone)]
pub(crate) struct RouterRecoveryPolicy {
    pub(crate) restart_delay: Duration,
    pub(crate) start_limit_interval: Duration,
    pub(crate) start_limit_burst: usize,
}

impl Default for RouterRecoveryPolicy {
    fn default() -> Self {
        Self {
            restart_delay: crate::supervisor::RESTART_SEC,
            start_limit_interval: crate::supervisor::START_LIMIT_INTERVAL,
            start_limit_burst: crate::supervisor::START_LIMIT_BURST,
        }
    }
}

#[derive(Debug, Clone)]
struct RouterProbe {
    namespace: String,
    robot_id: String,
    execution: phoxal::bus::ExecutionId,
}

impl RouterProbe {
    fn from_plan(plan: &LaunchPlan, execution: phoxal::bus::ExecutionId) -> Result<Self> {
        let robot = plan
            .robots
            .first()
            .context("launch plan has no robot for router readiness")?;
        Ok(Self {
            namespace: robot.namespace.clone(),
            robot_id: robot.id.clone(),
            execution,
        })
    }

    async fn connect(&self, endpoint: &str) -> Result<()> {
        let bus = phoxal::raw::Bus::open(phoxal::raw::BusConfig {
            namespace: self.namespace.clone(),
            robot_id: self.robot_id.clone(),
            participant: "phoxal-cli-router-readiness".to_string(),
            execution: self.execution,
            producer: phoxal::bus::ProducerId::mint(),
            connect_endpoints: vec![endpoint.to_string()],
        })
        .await
        .context("connect CLI readiness probe to infrastructure router")?;
        bus.close()
            .await
            .context("close CLI router readiness probe")
    }
}

pub(crate) struct InfrastructureRouter {
    process: RouterProcess,
    launch: RouterLaunch,
    participant_endpoint: String,
    readiness_probe: RouterProbe,
    recovery_policy: RouterRecoveryPolicy,
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
    async fn stop(&mut self) {
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

impl InfrastructureRouter {
    pub(crate) async fn supervise(
        mut self,
        stages: Vec<SupervisionStage>,
        board: BoardBackend,
        options: SupervisorOptions,
    ) -> Result<()> {
        let session_token = options.token.clone();
        let (spawned_rows, wait_only_rows) = recovery_rows(&stages);
        let mut failures = VecDeque::new();
        loop {
            let epoch_token = session_token.child_token();
            let mut epoch_options = options.clone();
            epoch_options.token = epoch_token.clone();
            let supervisor = supervise_until_shutdown(stages.clone(), board.clone(), epoch_options);
            tokio::pin!(supervisor);
            let (status, teardown_result) = tokio::select! {
                outcome = &mut supervisor => {
                    self.process.stop().await;
                    return outcome;
                }
                status = self.process.child.wait() => {
                    epoch_token.cancel();
                    let teardown_result = supervisor.await;
                    self.process.stdout_task.abort();
                    self.process.stderr_task.abort();
                    (status.context("failed to wait for infrastructure router")?, teardown_result)
                }
            };
            teardown_result
                .context("failed to tear down the graph after infrastructure router exit")?;
            if session_token.is_cancelled() {
                return Ok(());
            }

            let epoch = board.begin_recovery_epoch(&spawned_rows, &wait_only_rows);
            let fault = format!("infrastructure router exited with {status}");
            board.set_router_status(format!("restarting:{fault}"));
            board.set_state(
                phoxal_cli_core::session::ProcessKey::project("infrastructure-router"),
                crate::supervisor::ParticipantState::Restarting,
                Some(fault.clone()),
            );
            tracing::warn!(recovery_epoch = epoch, %fault, "recreating the complete process graph");
            record_recovery_failure(&self.recovery_policy, &mut failures, &fault)?;

            loop {
                tokio::select! {
                    () = session_token.cancelled() => return Ok(()),
                    () = tokio::time::sleep(self.recovery_policy.restart_delay) => {}
                }
                let restart = launch_router_process(
                    &self.launch,
                    &self.participant_endpoint,
                    &self.readiness_probe,
                );
                let restarted = tokio::select! {
                    () = session_token.cancelled() => return Ok(()),
                    result = restart => result,
                };
                match restarted {
                    Ok(process) => {
                        self.process = process;
                        board.enable_presence_for_recovery();
                        board.set_router_status(format!("ready:{}", self.participant_endpoint));
                        board.set_state(
                            phoxal_cli_core::session::ProcessKey::project("infrastructure-router"),
                            crate::supervisor::ParticipantState::Ready,
                            None,
                        );
                        tracing::info!(recovery_epoch = epoch, endpoint = %self.participant_endpoint, "infrastructure router recovered; recreating staged graph");
                        break;
                    }
                    Err(error) => {
                        let fault =
                            format!("infrastructure router recovery start failed: {error:#}");
                        tracing::warn!(recovery_epoch = epoch, %fault);
                        record_recovery_failure(&self.recovery_policy, &mut failures, &fault)?;
                    }
                }
            }
        }
    }
}

fn record_recovery_failure(
    policy: &RouterRecoveryPolicy,
    failures: &mut VecDeque<Instant>,
    fault: &str,
) -> Result<()> {
    let now = Instant::now();
    failures.retain(|failure| now.duration_since(*failure) <= policy.start_limit_interval);
    failures.push_back(now);
    if failures.len() >= policy.start_limit_burst {
        let interval = human::duration(policy.start_limit_interval);
        bail!(
            "infrastructure router full-stack recovery exhausted after {} failures in {interval}: {fault}",
            policy.start_limit_burst,
        );
    }
    Ok(())
}

fn recovery_rows(
    stages: &[SupervisionStage],
) -> (
    Vec<(phoxal_cli_core::session::ProcessKey, Option<String>)>,
    Vec<phoxal_cli_core::session::ProcessKey>,
) {
    let mut spawned = Vec::new();
    let mut spawned_ids = BTreeSet::new();
    for spec in stages.iter().flat_map(|stage| &stage.specs) {
        if spawned_ids.insert(spec.key.clone()) {
            spawned.push((spec.key.clone(), spec.note.clone()));
        }
    }
    let wait_only = stages
        .iter()
        .flat_map(|stage| stage.ready_ids.iter())
        .filter(|id| !spawned_ids.contains(*id))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    (spawned, wait_only)
}

/// Resolve the router's optional config file against `root`, verifying it
/// exists. `root` is always the staged runtime-layout root - a source run passes
/// `.phoxal/build/<triple>/` and a staged/bundle run passes its layout root -
/// because staging copies `router.config` into the layout under its relative
/// path (#936, finding 4), so every mode resolves the same staged asset and an
/// extracted `build.phoxal` carries its own router config.
pub(crate) fn resolve_router_config(
    robot: &phoxal::model::robot::v0::Robot,
    root: &Path,
) -> Result<Option<PathBuf>> {
    let config = robot
        .router
        .config
        .as_ref()
        .map(|config| resolve_project_path(root, config));
    if let Some(config) = &config {
        anyhow::ensure!(
            config.is_file(),
            "router.config file {} does not exist",
            config.display()
        );
    }
    Ok(config)
}

pub(crate) async fn start_infrastructure_router(
    staged_root: &Path,
    project_root: &Path,
    config: Option<PathBuf>,
    plan: &LaunchPlan,
    execution: phoxal::bus::ExecutionId,
) -> Result<(InfrastructureRouter, String)> {
    let binary = crate::stager::staged_router_binary(staged_root);
    anyhow::ensure!(
        binary.is_file(),
        "phoxal-infrastructure-router is not staged at {}; run `phoxal update`",
        binary.display()
    );
    let launch = RouterLaunch { binary, config };
    let endpoint = project_router_endpoint(project_root);
    let readiness_probe = RouterProbe::from_plan(plan, execution)?;
    std::fs::create_dir_all(
        crate::runtime_paths::RuntimePaths::for_root(project_root).volatile_root,
    )?;
    let process = launch_router_process(&launch, &endpoint, &readiness_probe).await?;
    Ok((
        InfrastructureRouter {
            process,
            launch,
            participant_endpoint: endpoint.clone(),
            readiness_probe,
            recovery_policy: RouterRecoveryPolicy::default(),
        },
        endpoint,
    ))
}

pub(crate) fn project_router_endpoint(project_root: &Path) -> String {
    format!(
        "unixsock-stream/{}",
        crate::runtime_paths::RuntimePaths::for_root(project_root)
            .router_socket()
            .display()
    )
}

async fn launch_router_process(
    launch: &RouterLaunch,
    endpoint: &str,
    readiness_probe: &RouterProbe,
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
        readiness_probe,
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

async fn wait_for_router_connection(
    child: &mut ManagedChild,
    endpoint: &str,
    readiness_probe: &RouterProbe,
    stderr_tail: &std::sync::Arc<std::sync::Mutex<String>>,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + ROUTER_READY_TIMEOUT;
    let error = loop {
        if let Some(status) = child.try_wait()? {
            return Err(router_start_error(
                anyhow::anyhow!("infrastructure router exited before the CLI connected ({status})"),
                stderr_tail,
            ));
        }
        let error = match tokio::time::timeout(
            Duration::from_millis(250),
            readiness_probe.connect(endpoint),
        )
        .await
        {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(error)) => error,
            Err(_) => anyhow::anyhow!("CLI readiness connection attempt timed out"),
        };
        if tokio::time::Instant::now() >= deadline {
            break error;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    Err(router_start_error(
        error.context("timed out waiting for the CLI to connect to the infrastructure router"),
        stderr_tail,
    ))
}

fn router_start_error(
    error: anyhow::Error,
    stderr_tail: &std::sync::Arc<std::sync::Mutex<String>>,
) -> anyhow::Error {
    let tail = stderr_tail
        .lock()
        .map(|tail| tail.clone())
        .unwrap_or_default();
    if tail.is_empty() {
        error
    } else {
        error.context(format!("infrastructure router stderr:\n{tail}"))
    }
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
