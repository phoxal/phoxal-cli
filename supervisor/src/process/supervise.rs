//! Main supervision loop, actions, and orderly shutdown.

use super::{
    ProcessState, RunningParticipant, SupervisionStage, SupervisorAction, SupervisorOptions,
    SupervisorState, await_stage_ready, join_reader, maybe_publish_startup_outcome,
    spawn_until_pending, stop_child,
};
use super::stages::StageReporter;
use crate::WaitBudget;
use anyhow::Result;
use phoxal_cli_core::runtime::{ProjectLifecycle, RuntimeFailurePolicy};
use std::collections::VecDeque;
use std::time::Duration;
use std::time::Instant;
use tokio::time::MissedTickBehavior;

pub async fn supervise_until_shutdown(
    stages: Vec<SupervisionStage>,
    board: SupervisorState,
    progress: StageReporter,
    mut options: SupervisorOptions,
) -> Result<()> {
    let failed_required = board
        .snapshot()
        .processes
        .into_iter()
        .filter(|(_, entry)| {
            entry.descriptor.startup_requirement
                == phoxal_cli_core::runtime::StartupRequirement::Required
                && entry.status.actual == phoxal_cli_core::runtime::ProcessState::Failed
        })
        .map(|(key, _)| key.to_string())
        .collect::<Vec<_>>();
    if !failed_required.is_empty() {
        let reason = format!(
            "required process(es) failed before startup: {}",
            failed_required.join(", ")
        );
        board.fail(&reason);
        progress.failed(&reason);
        options.token.cancel();
        anyhow::bail!(reason);
    }
    let mut running = Vec::new();
    let mut stage_queue: VecDeque<SupervisionStage> = stages.into();
    let token = options.token.clone();

    // Spawn every leading stage that has nothing to wait for back-to-back,
    // then park on the first stage that actually gates the next one. A
    // zero-wait stage is uncommon today, since every real stage waits on its
    // participants' Liveliness - but the Infrastructure stage empties as its
    // tools are removed, so this keeps that case from stalling the whole
    // startup on an empty `select!` branch.
    let mut pending_stage =
        spawn_until_pending(&mut running, &board, &progress, &mut stage_queue).await;
    maybe_publish_startup_outcome(&board, &options, &pending_stage).await;

    let mut ticker = tokio::time::interval(Duration::from_millis(500));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let action_rx = options.action_rx.take();
    let mut supervisor_error = None;
    'supervision: loop {
        tokio::select! {
            () = token.cancelled() => {
                break;
            }
            action = recv_action(action_rx.as_ref()) => {
                if let Some(action) = action
                    && let Err(error) = handle_action(&mut running, &board, action).await
                {
                    tracing::error!(error = %error, "supervisor action failed; shutting down graph");
                    supervisor_error = Some(error);
                    break 'supervision;
                }
            }
            // Recreated fresh every loop pass (the same pattern as
            // `recv_action` above): the ONLY state that must survive a
            // cancelled poll is the stage's own deadline, which lives in
            // `pending_stage` outside this future, not inside it - so
            // recreating the await is safe and never resets the timeout.
            result = await_stage_ready(
                &board,
                pending_stage.as_ref().map_or(&[][..], |stage| stage.ready_ids.as_slice()),
                pending_stage.as_ref().map_or(&[][..], |stage| stage.failure_ids.as_slice()),
                pending_stage.as_ref().map_or(&[][..], |stage| stage.optional_ids.as_slice()),
                pending_stage.as_ref().map_or(WaitBudget::Unbounded, |stage| match stage.deadline {
                    Some(deadline) => WaitBudget::Bounded(deadline.saturating_duration_since(Instant::now())),
                    None => WaitBudget::Unbounded,
                }),
                Duration::from_millis(200),
            ), if pending_stage.is_some() => {
                let stage = pending_stage.take().expect("guarded by is_some");
                match result {
                    Ok(()) => {
                        tracing::info!(stage = %stage.label, "supervisor startup phase ready");
                        progress.detail(format!(
                            "{} participants ready in {}",
                            stage.ready_ids.len(),
                            stage.label
                        ));
                        pending_stage = spawn_until_pending(
                            &mut running,
                            &board,
                            &progress,
                            &mut stage_queue,
                        ).await;
                        maybe_publish_startup_outcome(&board, &options, &pending_stage).await;
                    }
                    Err(error) => {
                        let reason = format!("stage '{}' stalled: {error:#}", stage.label);
                        tracing::error!(stage = %stage.label, error = %error, "required startup phase failed");
                        progress.failed(&reason);
                        board.fail(&reason);
                        supervisor_error = Some(anyhow::anyhow!(reason));
                        token.cancel();
                        break 'supervision;
                    }
                }
            }
            _ = ticker.tick() => {
                for participant in &mut running {
                    if let Err(error) = participant.poll(&board, &participant.spec.restart_policy.clone()).await {
                        tracing::error!(error = %error, "participant poll failed; shutting down graph");
                        supervisor_error = Some(error);
                        break 'supervision;
                    }
                    if participant.failed
                        && matches!(board.snapshot().lifecycle, ProjectLifecycle::Ready | ProjectLifecycle::Degraded)
                    {
                        match participant.spec.runtime_failure {
                            RuntimeFailurePolicy::KeepProjectDegraded => {
                                board.set_lifecycle(ProjectLifecycle::Degraded);
                            }
                            RuntimeFailurePolicy::StopProject => {
                                let reason = format!(
                                    "process {} exhausted its restart policy; StopProject",
                                    participant.spec.key
                                );
                                board.fail(&reason);
                                supervisor_error = Some(anyhow::anyhow!(reason));
                                token.cancel();
                                break 'supervision;
                            }
                            RuntimeFailurePolicy::RecreateGraph => {
                                let reason = format!(
                                    "process {} requires graph recreation",
                                    participant.spec.key
                                );
                                board.fail(&reason);
                                supervisor_error = Some(anyhow::anyhow!(reason));
                                break 'supervision;
                            }
                        }
                    }
                }
            }
        }
    }

    if supervisor_error.is_none() {
        board.set_lifecycle(ProjectLifecycle::Stopping);
    }
    shutdown_all(&mut running, &board).await;
    if let Some(error) = supervisor_error {
        return Err(error);
    }
    board.set_lifecycle(ProjectLifecycle::Stopped);
    Ok(())
}

pub(crate) async fn recv_action(
    action_rx: Option<&super::SupervisorActionReceiver>,
) -> Option<SupervisorAction> {
    match action_rx {
        Some(action_rx) => action_rx.recv().await,
        None => std::future::pending().await,
    }
}

pub(crate) async fn handle_action(
    running: &mut [RunningParticipant],
    board: &SupervisorState,
    action: SupervisorAction,
) -> Result<()> {
    match action {
        SupervisorAction::Restart { key } => {
            let Some(participant) = running
                .iter_mut()
                .find(|participant| participant.spec.key == key)
            else {
                tracing::warn!(process = %key, "restart requested for unknown process");
                return Ok(());
            };
            let spec = participant.spec.clone();
            participant
                .swap(spec, board, "manual restart".to_string())
                .await
        }
    }
}

pub(crate) async fn shutdown_all(running: &mut [RunningParticipant], board: &SupervisorState) {
    // Reverse exact startup phase order, concurrency within each phase.
    let mut phases = running
        .iter()
        .map(|participant| participant.shutdown_phase.clone())
        .collect::<Vec<_>>();
    phases.sort_by_key(|phase| phase_rank(phase));
    phases.dedup();
    for phase in phases.into_iter().rev() {
        let mut joins = tokio::task::JoinSet::new();
        for participant in running
            .iter_mut()
            .filter(|participant| participant.shutdown_phase == phase)
        {
            let mut child = participant.child.take();
            let stdout = participant.stdout_task.take();
            let stderr = participant.stderr_task.take();
            let spec = participant.spec.clone();
            let board = board.clone();
            joins.spawn(async move {
                if let Some(child) = child.as_mut() {
                    tracing::debug!(process = %spec.key, "stopping supervised process");
                    if let Err(error) =
                        stop_child(child, spec.shutdown_grace, spec.process_group).await
                    {
                        board.set_state(
                            &spec.key,
                            ProcessState::Failed,
                            Some(format!("failed to stop: {error:#}")),
                        );
                    }
                    board.set_pid(&spec.key, None);
                }
                join_reader(stdout).await;
                join_reader(stderr).await;
            });
        }
        while let Some(result) = joins.join_next().await {
            if let Err(error) = result {
                tracing::warn!(%error, "shutdown worker failed");
            }
        }
    }
}

fn phase_rank(phase: &str) -> u8 {
    match phase {
        "starting project infrastructure" => 0,
        "starting robot graph" => 1,
        _ => 2,
    }
}

#[cfg(test)]
mod tests {
    use phoxal_cli_core::runtime::ProjectLifecycle;

    use super::*;

    #[tokio::test]
    async fn cancellation_publishes_orderly_terminal_state() {
        let state = SupervisorState::new();
        let token = tokio_util::sync::CancellationToken::new();
        token.cancel();
        supervise_until_shutdown(
            Vec::new(),
            state.clone(),
            crate::process::stages::SilentProgress::reporter(),
            SupervisorOptions {
                token,
                ..SupervisorOptions::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            state.snapshot().lifecycle,
            ProjectLifecycle::Stopped
        );
    }
}
