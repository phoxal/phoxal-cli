//! Router responsibilities for run.

mod process;
mod readiness;

use crate::{
    SupervisionStage, SupervisorOptions, SupervisorState, process::RouterRecoveryPolicy,
    supervise_until_shutdown,
};
use anyhow::{Context, Result, bail};
use std::collections::{BTreeSet, VecDeque};
use std::time::Instant;

use process::{RouterLaunch, RouterProcess, launch_router_process};
use readiness::unixsock_stream_path;

pub struct InfrastructureRouter {
    process: RouterProcess,
    launch: RouterLaunch,
    participant_endpoint: String,
    recovery_policy: RouterRecoveryPolicy,
}

impl InfrastructureRouter {
    pub async fn supervise(
        mut self,
        stages: Vec<SupervisionStage>,
        board: SupervisorState,
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
            board.set_state(
                phoxal_cli_core::runtime::ProcessKey::project("infrastructure-router"),
                crate::ProcessState::Restarting,
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
                        board.set_state(
                            phoxal_cli_core::runtime::ProcessKey::project("infrastructure-router"),
                            crate::ProcessState::Ready,
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
        let interval = crate::format_duration(policy.start_limit_interval);
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
    Vec<(phoxal_cli_core::runtime::ProcessKey, Option<String>)>,
    Vec<phoxal_cli_core::runtime::ProcessKey>,
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
pub async fn start_infrastructure_router(
    binary: std::path::PathBuf,
    config: Option<std::path::PathBuf>,
    endpoint: String,
) -> Result<InfrastructureRouter> {
    anyhow::ensure!(
        binary.is_file(),
        "phoxal-infrastructure-router is not staged at {}; the staged runtime layout is \
         incomplete. Run `phoxal run` from the source project so staging refreshes it, or \
         rebuild the bundle with `phoxal build` if you are running from an extracted archive",
        binary.display()
    );
    let launch = RouterLaunch { binary, config };
    let socket = unixsock_stream_path(&endpoint)?;
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let process = launch_router_process(&launch, &endpoint).await?;
    Ok(InfrastructureRouter {
        process,
        launch,
        participant_endpoint: endpoint,
        recovery_policy: RouterRecoveryPolicy::default(),
    })
}
