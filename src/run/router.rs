//! Router responsibilities for run.

use super::{ROUTER_READY_TIMEOUT, locate_tool_binary};
use crate::supervisor::BoardBackend;
use crate::supervisor::ManagedChild;
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
use phoxal_cli_core::session::human;
use std::collections::{BTreeSet, VecDeque};
use std::os::fd::FromRawFd;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;
use tokio::io::BufReader;
use tokio::io::{AsyncBufReadExt, AsyncReadExt};

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

pub(crate) struct InfrastructureRouter {
    process: RouterProcess,
    launch: RouterLaunch,
    listeners: Vec<String>,
    participant_endpoint: String,
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
    ) -> Result<SupervisorOutcome> {
        let session_token = options.token.clone();
        let (spawned_rows, wait_only_rows) = recovery_rows(&stages);
        let mut failures = VecDeque::new();
        loop {
            let epoch_token = session_token.child_token();
            let mut epoch_options = options.clone();
            epoch_options.token = epoch_token.clone();
            let supervisor = supervise_until_shutdown(stages.clone(), board.clone(), epoch_options);
            tokio::pin!(supervisor);
            let (status, teardown_outcome) = tokio::select! {
                outcome = &mut supervisor => {
                    self.process.stop().await;
                    return outcome;
                }
                status = self.process.child.wait() => {
                    epoch_token.cancel();
                    let teardown_outcome = supervisor.await;
                    self.process.stdout_task.abort();
                    self.process.stderr_task.abort();
                    (status.context("failed to wait for infrastructure router")?, teardown_outcome)
                }
            };
            let teardown_outcome = teardown_outcome
                .context("failed to tear down the graph after infrastructure router exit")?;
            if session_token.is_cancelled() {
                return Ok(teardown_outcome);
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
                    () = session_token.cancelled() => return Ok(teardown_outcome),
                    () = tokio::time::sleep(self.recovery_policy.restart_delay) => {}
                }
                let restart = launch_router_process(&self.launch, &self.participant_endpoint);
                let restarted = tokio::select! {
                    () = session_token.cancelled() => return Ok(teardown_outcome),
                    result = restart => result,
                };
                match restarted {
                    Ok((process, listeners)) => {
                        self.process = process;
                        self.listeners = listeners;
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

    #[cfg(test)]
    pub(crate) fn set_recovery_policy(&mut self, policy: RouterRecoveryPolicy) {
        self.recovery_policy = policy;
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

pub(crate) async fn start_infrastructure_router(
    resolved: &ResolvedRobot,
    project_root: &Path,
    ui: &crate::Ui,
) -> Result<(InfrastructureRouter, String)> {
    let binary = locate_tool_binary(resolved, SITE_INFRASTRUCTURE_ROUTER, ui)?
        .context("phoxal-infrastructure-router is not staged; run `phoxal update`")?;
    let config = resolved
        .robot
        .router
        .config
        .as_ref()
        .map(|config| resolve_project_path(project_root, config));
    if let Some(config) = &config {
        anyhow::ensure!(
            config.is_file(),
            "router.config file {} does not exist",
            config.display()
        );
    }
    let launch = RouterLaunch { binary, config };
    std::fs::create_dir_all(project_root.join(".phoxal"))?;
    let endpoint = format!(
        "unixsock-stream/{}",
        project_root.join(".phoxal/zenoh.sock").display()
    );
    let (process, listeners) = launch_router_process(&launch, &endpoint).await?;
    Ok((
        InfrastructureRouter {
            process,
            launch,
            listeners,
            participant_endpoint: endpoint.clone(),
            recovery_policy: RouterRecoveryPolicy::default(),
        },
        endpoint,
    ))
}

async fn launch_router_process(
    launch: &RouterLaunch,
    required_endpoint: &str,
) -> Result<(RouterProcess, Vec<String>)> {
    let mut readiness_fds = [0_i32; 2];
    // SAFETY: valid pointer to storage for two pipe descriptors.
    if unsafe { libc::pipe(readiness_fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("create router readiness pipe");
    }
    let (read_fd, ready_fd) = (readiness_fds[0], readiness_fds[1]);
    // SAFETY: valid parent-owned read descriptor; only the write end is a
    // router bootstrap capability.
    if unsafe { libc::fcntl(read_fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("mark router readiness read end close-on-exec");
    }
    let mut command = tokio::process::Command::new(&launch.binary);
    if let Some(config) = &launch.config {
        command.arg("--config").arg(config);
    }
    command.arg("--local-endpoint").arg(required_endpoint);
    command.arg("--ready-fd").arg(ready_fd.to_string());
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = ManagedChild::spawn(&mut command, true, &[])
        .context("failed to launch phoxal-infrastructure-router")?;
    // SAFETY: the child inherited this valid descriptor; parent no longer owns it.
    unsafe { libc::close(ready_fd) };
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
    let attempt = RouterLaunchAttempt::new(child, stderr_task);
    // SAFETY: read_fd is the unique parent-owned read side.
    let readiness_file = unsafe { std::fs::File::from_raw_fd(read_fd) };
    let mut readiness_reader = tokio::fs::File::from_std(readiness_file);
    let readiness: Result<Vec<String>> = async {
        let readiness = tokio::time::timeout(ROUTER_READY_TIMEOUT, async {
            let mut bytes = Vec::new();
            readiness_reader.read_to_end(&mut bytes).await?;
            let event: phoxal::infrastructure::router::RouterReadyV0 =
                serde_json::from_slice(&bytes).context("invalid router readiness FD payload")?;
            match event {
                phoxal::infrastructure::router::RouterReadyV0::Ready {
                    local_endpoint,
                    listeners,
                } => {
                    anyhow::ensure!(
                        local_endpoint == required_endpoint,
                        "router returned endpoint {local_endpoint}, expected {required_endpoint}"
                    );
                    Ok(listeners)
                }
                phoxal::infrastructure::router::RouterReadyV0::Failed { message } => {
                    bail!("{message}")
                }
            }
        })
        .await
        .context("timed out waiting for infrastructure router readiness")?;
        let listeners = match readiness {
            Ok(listeners) => listeners,
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
        anyhow::ensure!(
            listeners
                .first()
                .is_some_and(|listener| listener == required_endpoint),
            "router did not report mandatory local endpoint first: {listeners:?}"
        );
        Ok(listeners)
    }
    .await;
    let listeners = match readiness {
        Ok(listeners) => listeners,
        Err(error) => {
            attempt.stop_failed().await;
            return Err(error);
        }
    };
    let mut lines = BufReader::new(stdout).lines();
    let stdout_task = tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::info!(target: "infrastructure_router", "{line}");
        }
    });
    Ok((attempt.finish(stdout_task), listeners))
}

#[cfg(test)]
pub(crate) fn parse_router_ready(line: &str) -> Result<String> {
    let event: phoxal::infrastructure::router::RouterReadyV0 = serde_json::from_str(line)?;
    match event {
        phoxal::infrastructure::router::RouterReadyV0::Ready { local_endpoint, .. } => {
            Ok(local_endpoint)
        }
        phoxal::infrastructure::router::RouterReadyV0::Failed { message } => bail!("{message}"),
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

#[cfg(all(test, unix))]
mod recovery_tests {
    use super::*;
    use crate::session::output::WaitBudget;
    use crate::supervisor::{ParticipantState, ParticipantStatus};
    use phoxal_cli_core::session::ParticipantKind;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    const TEST_LISTENER: &str = "unixsock-stream//tmp/phoxal-router-test.sock";
    const TEST_EXTRA_LISTENER: &str = "tcp/127.0.0.1:48000";

    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
    }

    fn write_executable(path: &Path, contents: &str) -> Result<()> {
        fs::write(path, contents)?;
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)?;
        Ok(())
    }

    fn write_fake_router(
        root: &Path,
        failures_before_stable: usize,
        add_listener_after_restart: bool,
    ) -> Result<PathBuf> {
        let script = root.join("fake-router");
        let count = shell_quote(&root.join("router-count"));
        let args = shell_quote(&root.join("router-args"));
        let ready = if add_listener_after_restart {
            format!(
                "if [ \"$count\" -gt 1 ]; then\n\
                   ready='{{\"status\":\"ready\",\"local_endpoint\":\"{TEST_LISTENER}\",\"listeners\":[\"{TEST_LISTENER}\",\"{TEST_EXTRA_LISTENER}\"]}}'\n\
                 else\n\
                   ready='{{\"status\":\"ready\",\"local_endpoint\":\"{TEST_LISTENER}\",\"listeners\":[\"{TEST_LISTENER}\"]}}'\n\
                 fi"
            )
        } else {
            format!(
                "ready='{{\"status\":\"ready\",\"local_endpoint\":\"{TEST_LISTENER}\",\"listeners\":[\"{TEST_LISTENER}\"]}}'"
            )
        };
        write_executable(
            &script,
            &format!(
                "#!/bin/sh\n\
                 count=0\n\
                 if [ -f {count} ]; then count=$(sed -n '1p' {count}); fi\n\
                 count=$((count + 1))\n\
                 printf '%s\\n' \"$count\" > {count}\n\
                 printf '%s\\n' \"$*\" >> {args}\n\
                 ready_fd=\n\
                 while [ \"$#\" -gt 0 ]; do\n\
                   if [ \"$1\" = '--ready-fd' ]; then ready_fd=$2; shift 2; else shift; fi\n\
                 done\n\
                 {ready}\n\
                 eval \"printf '%s' '$ready' >&$ready_fd\"\n\
                 eval \"exec $ready_fd>&-\"\n\
                 if [ \"$count\" -le {failures_before_stable} ]; then\n\
                   sleep 0.5\n\
                   exit 17\n\
                 fi\n\
                 trap 'exit 0' TERM INT\n\
                 while :; do sleep 1; done\n"
            ),
        )?;
        Ok(script)
    }

    fn write_fake_webots(root: &Path) -> Result<PathBuf> {
        let script = root.join("fake-webots");
        write_executable(
            &script,
            "#!/bin/sh\ntrap 'exit 0' TERM INT\nwhile :; do sleep 1; done\n",
        )?;
        Ok(script)
    }

    fn process_group_alive(pid: u32) -> bool {
        // SAFETY: signal zero performs only the process-group existence check.
        let result = unsafe { libc::kill(-(pid as i32), 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    fn process_alive(pid: u32) -> bool {
        // SAFETY: signal zero performs only the process existence check.
        let result = unsafe { libc::kill(pid as i32, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    async fn start_fake_router(binary: PathBuf) -> Result<InfrastructureRouter> {
        let launch = RouterLaunch {
            binary,
            config: None,
        };
        let (process, listeners) = launch_router_process(&launch, TEST_LISTENER).await?;
        Ok(InfrastructureRouter {
            process,
            launch,
            listeners,
            participant_endpoint: TEST_LISTENER.to_string(),
            recovery_policy: RouterRecoveryPolicy {
                restart_delay: Duration::from_millis(10),
                start_limit_interval: Duration::from_secs(5),
                start_limit_burst: 5,
            },
        })
    }

    fn webots_spec(executable: PathBuf) -> ParticipantSpec {
        ParticipantSpec {
            key: phoxal_cli_core::session::ProcessKey::project("webots"),
            id: "webots".to_string(),
            kind: ParticipantKind::Tool,
            executable,
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            shutdown_grace: Duration::from_millis(500),
            process_group: true,
            note: Some("CLI-managed Webots application".to_string()),
            bus_participant: false,
            readiness: phoxal_cli_core::session::ReadinessPolicy::ProcessSpawned,
            startup_requirement: phoxal_cli_core::session::StartupRequirement::Required,
            runtime_failure: phoxal_cli_core::session::RuntimeFailurePolicy::StopProject,
            restart_policy: Default::default(),
        }
    }

    #[tokio::test]
    async fn failed_readiness_reaps_the_child_and_stderr_pump_attempt() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let script = temp.path().join("never-ready-router");
        let pid_path = temp.path().join("router-pid");
        write_executable(
            &script,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$$\" > {}\nprintf '%s\\n' 'diagnostic' >&2\nready_fd=\nwhile [ \"$#\" -gt 0 ]; do if [ \"$1\" = '--ready-fd' ]; then ready_fd=$2; shift 2; else shift; fi; done\neval \"printf '%s' 'not-json' >&$ready_fd\"\neval \"exec $ready_fd>&-\"\nsleep 30\n",
                shell_quote(&pid_path),
            ),
        )?;
        let launch = RouterLaunch {
            binary: script,
            config: None,
        };

        let error = match launch_router_process(&launch, TEST_LISTENER).await {
            Ok(_) => panic!("invalid readiness must fail the launch attempt"),
            Err(error) => error,
        };
        assert!(
            format!("{error:#}").contains("invalid router readiness FD payload"),
            "{error:#}"
        );
        let pid = fs::read_to_string(pid_path)?.trim().parse::<u32>()?;
        assert!(
            !process_alive(pid),
            "a failed launch attempt must reap its router child"
        );
        Ok(())
    }

    #[tokio::test]
    async fn router_death_preserves_pinned_endpoint_with_an_extra_listener() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let router_binary = write_fake_router(temp.path(), 1, true)?;
        let webots_binary = write_fake_webots(temp.path())?;
        let router = start_fake_router(router_binary).await?;
        let board = BoardBackend::new();
        let controller_id = "simulator-webots-controller-robot";
        let mut controller = ParticipantStatus::new(
            controller_id,
            ParticipantKind::Simulator,
            ParticipantState::Starting,
        );
        controller.note = Some("SimulationManaged: launched by Webots".to_string());
        board.upsert(controller);
        let stages = vec![
            SupervisionStage::new(
                "starting Webots",
                vec![webots_spec(webots_binary)],
                WaitBudget::Bounded(Duration::from_secs(5)),
            )
            .with_extra_ready_ids([phoxal_cli_core::session::ProcessKey::project(controller_id)]),
        ];
        let token = tokio_util::sync::CancellationToken::new();
        let supervision = tokio::spawn(router.supervise(
            stages,
            board.clone(),
            SupervisorOptions {
                token: token.clone(),
                ..SupervisorOptions::default()
            },
        ));

        let first_pid = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(pid) = board
                    .snapshot()
                    .participants
                    .get("webots")
                    .and_then(|status| status.pid)
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the initial CLI-owned Webots process must spawn");
        let recovered = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if board.recovery_epoch() >= 1
                    && let Some(pid) = board.snapshot().participants["webots"].pid
                    && pid != first_pid
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            recovered.is_ok(),
            "router loss must tear down and recreate the graph; epoch={}, router_count={:?}, board={:?}",
            board.recovery_epoch(),
            fs::read_to_string(temp.path().join("router-count")),
            board.snapshot(),
        );
        let second_pid = recovered.expect("checked above");
        assert!(
            !process_group_alive(first_pid),
            "the first CLI-owned Webots process group must be gone before recreation"
        );

        let args = fs::read_to_string(temp.path().join("router-args"))?;
        assert!(
            args.lines()
                .any(|line| line
                    .starts_with(&format!("--local-endpoint {TEST_LISTENER} --ready-fd "))),
            "replacement router must receive the original listener: {args:?}"
        );
        assert_eq!(
            board.snapshot().participants[controller_id].note.as_deref(),
            Some("SimulationManaged: launched by Webots"),
            "the CLI must retain, but never spawn, the Webots-owned controller row"
        );

        token.cancel();
        let outcome = tokio::time::timeout(Duration::from_secs(5), supervision)
            .await
            .expect("cancellation must stop the recovered graph")
            .expect("router supervisor task panicked")?;
        assert!(outcome.failed_participants.is_empty());
        assert!(
            !process_group_alive(second_pid),
            "cancellation must stop the recovered CLI-owned Webots process group"
        );
        Ok(())
    }

    #[tokio::test]
    async fn repeated_router_death_exits_after_the_bounded_recovery_budget() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let router_binary = write_fake_router(temp.path(), 100, false)?;
        let mut router = start_fake_router(router_binary).await?;
        router.set_recovery_policy(RouterRecoveryPolicy {
            restart_delay: Duration::from_millis(10),
            start_limit_interval: Duration::from_secs(5),
            start_limit_burst: 3,
        });
        let board = BoardBackend::new();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            router.supervise(Vec::new(), board.clone(), SupervisorOptions::default()),
        )
        .await
        .expect("the recovery budget must end a permanently failing session");
        let error = result.expect_err("three router faults must exhaust the recovery budget");
        assert!(
            error
                .to_string()
                .contains("full-stack recovery exhausted after 3 failures"),
            "{error:#}"
        );
        assert!(error.to_string().contains("in 5.0s"), "{error:#}");
        assert_eq!(board.recovery_epoch(), 3);
        assert_eq!(
            fs::read_to_string(temp.path().join("router-count"))?.trim(),
            "3"
        );
        Ok(())
    }
}
