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
use tokio::net::UnixStream;

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

/// Prove the router's unix-socket endpoint actually accepts a connection -
/// not merely that its process has started or that the socket path exists on
/// disk. A stale file, or a router still mid-initialization, is
/// indistinguishable from a live one by `Path::exists()` alone; only a real
/// connect attempt tells "nothing is listening yet" (`ENOENT`) apart from
/// "something is listening and refusing" (`ECONNREFUSED`) apart from "ready".
///
/// This intentionally does not go through `phoxal::raw::Bus::open`: Zenoh's
/// default client-mode config sets `connect/timeout_ms` to `0` ("no retry"),
/// under which a single failed connect attempt is swallowed and `open()`
/// still returns `Ok` with zero established transports (see
/// `zenoh-config-*/DEFAULT_CONFIG.json5` and
/// `zenoh-*/src/net/runtime/orchestrator.rs`'s `connect_peers_single_link`).
/// That made the previous Zenoh-session probe succeed immediately regardless
/// of whether the router was actually listening, which is exactly the race
/// this gate exists to close. A bare socket connect has no such escape hatch:
/// the OS refuses to lie about whether a peer is accepting.
async fn probe_router_endpoint(endpoint: &str) -> Result<()> {
    let path = unixsock_stream_path(endpoint)?;
    let stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("connect to infrastructure router at {endpoint}"))?;
    drop(stream);
    Ok(())
}

/// Extract the filesystem path a `unixsock-stream/<path>` router endpoint
/// names, so the probe connects to the exact same socket a real participant
/// (`phoxal::participant::launch::env::CONNECT`) would use - a proxy
/// endpoint would prove nothing about the one dependents actually dial.
fn unixsock_stream_path(endpoint: &str) -> Result<&Path> {
    endpoint
        .strip_prefix("unixsock-stream/")
        .map(Path::new)
        .with_context(|| {
            format!("router endpoint {endpoint} is not a unixsock-stream endpoint understood by the readiness probe")
        })
}

pub(crate) struct InfrastructureRouter {
    process: RouterProcess,
    launch: RouterLaunch,
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
                let restart = launch_router_process(&self.launch, &self.participant_endpoint);
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
/// `.phoxal/bundle/` and a staged/bundle run passes its layout root -
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
) -> Result<(InfrastructureRouter, String)> {
    let binary = crate::stager::staged_router_binary(staged_root);
    anyhow::ensure!(
        binary.is_file(),
        "phoxal-infrastructure-router is not staged at {}; the staged runtime layout is \
         incomplete. Run `phoxal run` from the source project so staging refreshes it, or \
         rebuild the bundle with `phoxal build` if you are running from an extracted archive",
        binary.display()
    );
    let launch = RouterLaunch { binary, config };
    let endpoint = project_router_endpoint(project_root);
    std::fs::create_dir_all(
        crate::runtime_paths::RuntimePaths::for_root(project_root).volatile_root,
    )?;
    let process = launch_router_process(&launch, &endpoint).await?;
    Ok((
        InfrastructureRouter {
            process,
            launch,
            participant_endpoint: endpoint.clone(),
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

async fn launch_router_process(launch: &RouterLaunch, endpoint: &str) -> Result<RouterProcess> {
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

async fn wait_for_router_connection(
    child: &mut ManagedChild,
    endpoint: &str,
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
        let error =
            match tokio::time::timeout(Duration::from_millis(250), probe_router_endpoint(endpoint))
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

#[cfg(test)]
mod readiness_gate_tests {
    use super::*;
    use tokio::net::UnixListener;

    /// `/tmp` directly, not the crate-wide temp dir: a unix socket address is
    /// capped at 104 (macOS) / 108 (Linux) bytes, and the sandboxed test temp
    /// root this crate otherwise runs under is long enough to blow that
    /// budget on its own (see the project-lock/runtime-paths handling of the
    /// same limit).
    fn scratch_dir(label: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("phoxal-router-gate-{label}-"))
            .tempdir_in("/tmp")
            .expect("create a short-path temp dir for the unix-socket probe test")
    }

    /// A missing socket path (the router has not started at all yet) must
    /// fail the probe - this is the "No such file or directory" phase from
    /// the field bug.
    #[tokio::test]
    async fn probe_fails_when_nothing_is_at_the_endpoint() {
        let dir = scratch_dir("missing");
        let endpoint = format!(
            "unixsock-stream/{}",
            dir.path().join("router.sock").display()
        );
        probe_router_endpoint(&endpoint)
            .await
            .expect_err("a socket path nothing has bound must fail the probe");
    }

    /// The critical distinction the bug hinged on: a socket **file existing**
    /// is not the same as something **accepting connections** on it. Binding
    /// then dropping a listener leaves the file on disk with nothing behind
    /// it (`ECONNREFUSED`) - exactly the "Connection refused" phase from the
    /// field bug. A probe that only checked `Path::exists()` would pass this
    /// case; ours must not.
    #[tokio::test]
    async fn probe_fails_when_the_socket_file_exists_but_nothing_is_listening() {
        let dir = scratch_dir("stale");
        let socket_path = dir.path().join("router.sock");
        {
            let listener =
                std::os::unix::net::UnixListener::bind(&socket_path).expect("bind stale listener");
            drop(listener);
        }
        assert!(
            socket_path.exists(),
            "the socket file must still be on disk once its listener is dropped"
        );
        let endpoint = format!("unixsock-stream/{}", socket_path.display());
        probe_router_endpoint(&endpoint)
            .await
            .expect_err("a stale socket file with no listener must not be reported ready");
    }

    /// The positive case: once something is genuinely bound and accepting,
    /// the probe succeeds.
    #[tokio::test]
    async fn probe_succeeds_once_a_listener_is_actually_accepting() {
        let dir = scratch_dir("live");
        let socket_path = dir.path().join("router.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind live listener");
        let accept_task = tokio::spawn(async move {
            let _ = listener.accept().await;
        });
        let endpoint = format!("unixsock-stream/{}", socket_path.display());
        let result = probe_router_endpoint(&endpoint).await;
        accept_task.abort();
        result.expect("a live, accepting listener must pass the probe");
    }

    /// The ordering invariant itself: `wait_for_router_connection` must not
    /// report the router ready merely because its process is alive - only
    /// once its endpoint actually accepts a connection. A stand-in child
    /// process is spawned immediately (proving liveness alone is available
    /// from t=0) while the listener is only bound after a deliberate delay;
    /// if the gate were satisfied by "process started" (or by the socket
    /// path merely existing), this would resolve near-instantly instead of
    /// after the delay. This is the fact a "router starts before its
    /// dependents" test cannot see: the router already starts first today,
    /// but nothing previously proved it was connectable before dependents
    /// were told to proceed.
    #[tokio::test]
    async fn wait_for_router_connection_does_not_report_ready_before_the_listener_accepts() {
        let dir = scratch_dir("ordering");
        let socket_path = dir.path().join("router.sock");
        let endpoint = format!("unixsock-stream/{}", socket_path.display());

        // A harmless long-lived process stands in for the router: what
        // matters is that it is alive well before its listener exists.
        let mut command = tokio::process::Command::new("sleep");
        command
            .arg("5")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child =
            ManagedChild::spawn(&mut command, false, &[]).expect("spawn stand-in child process");

        let stderr_tail = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let listen_delay = Duration::from_millis(500);
        let delayed_listener = {
            let socket_path = socket_path.clone();
            tokio::spawn(async move {
                tokio::time::sleep(listen_delay).await;
                let listener =
                    UnixListener::bind(&socket_path).expect("bind delayed router listener");
                let _ = listener.accept().await;
            })
        };

        let started = Instant::now();
        let result = wait_for_router_connection(&mut child, &endpoint, &stderr_tail).await;
        let elapsed = started.elapsed();

        let _ = child.start_kill();
        delayed_listener.abort();

        result.expect("readiness must succeed once the listener actually accepts");
        assert!(
            elapsed >= listen_delay / 2,
            "readiness resolved after {elapsed:?}, implausibly before the router's listener \
             started accepting at {listen_delay:?} - the gate must prove connectivity, not \
             merely that the router process is alive or that a socket path exists"
        );
    }
}
