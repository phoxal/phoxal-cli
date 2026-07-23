//! Main supervision loop, actions, and orderly shutdown.

use super::{
    BoardBackend, ParticipantSpec, ParticipantState, RequestedStop, RunningParticipant,
    SupervisionStage, SupervisorAction, SupervisorOptions, SupervisorOutcome, await_stage_ready,
    emit_event, join_reader, maybe_emit_startup_outcome, send_process_group_terminate,
    send_terminate, spawn_until_pending, stop_child,
};
use crate::session::output::WaitBudget;
use anyhow::Result;
use phoxal_cli_core::session::{ProjectLifecycle, RuntimeFailurePolicy};
use std::collections::VecDeque;
use std::time::Duration;
use std::time::Instant;
use tokio::time::MissedTickBehavior;

pub async fn supervise_until_shutdown(
    stages: Vec<SupervisionStage>,
    board: BoardBackend,
    mut options: SupervisorOptions,
) -> Result<SupervisorOutcome> {
    let failed_required = board
        .supervisor_snapshot()
        .processes
        .into_iter()
        .filter(|(_, entry)| {
            entry.descriptor.startup_requirement
                == phoxal_cli_core::session::StartupRequirement::Required
                && entry.status.actual == phoxal_cli_core::session::ProcessState::Failed
        })
        .map(|(key, _)| key.to_string())
        .collect::<Vec<_>>();
    if !failed_required.is_empty() {
        board.set_lifecycle(ProjectLifecycle::Failed);
        options.token.cancel();
        anyhow::bail!(
            "required process(es) failed before startup: {}",
            failed_required.join(", ")
        );
    }
    let mut running = Vec::new();
    let mut stage_queue: VecDeque<SupervisionStage> = stages.into();
    let token = options.token.clone();
    let events_tx = options.events.take();

    // Spawn every leading stage that has nothing to wait for back-to-back
    // (uncommon in practice - every real stage waits on at least the router
    // or Liveliness - but keeps a zero-wait stage from stalling a whole
    // startup on an empty `select!` branch), then park on the first stage
    // that actually gates the next one.
    let mut pending_stage =
        spawn_until_pending(&mut running, &board, events_tx.as_ref(), &mut stage_queue).await;
    maybe_emit_startup_outcome(&board, &options, events_tx.as_ref(), &pending_stage).await;

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
                        board.complete_phase(&stage.label);
                        emit_event(events_tx.as_ref(), phoxal_cli_core::session::event::SessionEvent::PhaseFinished {
                            id: phoxal_cli_core::session::event::PhaseId::new(stage.label.clone()),
                            outcome: phoxal_cli_core::session::event::PhaseOutcome::Succeeded,
                            elapsed: stage.started.elapsed(),
                        }).await;
                        pending_stage = spawn_until_pending(
                            &mut running,
                            &board,
                            events_tx.as_ref(),
                            &mut stage_queue,
                        ).await;
                        maybe_emit_startup_outcome(&board, &options, events_tx.as_ref(), &pending_stage).await;
                    }
                    Err(error) => {
                        let reason = format!("stage '{}' stalled: {error:#}", stage.label);
                        tracing::error!(stage = %stage.label, error = %error, "required startup phase failed");
                        emit_event(events_tx.as_ref(), phoxal_cli_core::session::event::SessionEvent::PhaseFinished {
                            id: phoxal_cli_core::session::event::PhaseId::new(stage.label.clone()),
                            outcome: phoxal_cli_core::session::event::PhaseOutcome::Failed { error: format!("{error:#}") },
                            elapsed: stage.started.elapsed(),
                        }).await;
                        board.set_lifecycle(ProjectLifecycle::Failed);
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
                        && matches!(board.supervisor_snapshot().lifecycle, ProjectLifecycle::Ready | ProjectLifecycle::Degraded)
                    {
                        match participant.spec.runtime_failure {
                            RuntimeFailurePolicy::KeepProjectDegraded => {
                                board.set_lifecycle(ProjectLifecycle::Degraded);
                            }
                            RuntimeFailurePolicy::StopProject => {
                                supervisor_error = Some(anyhow::anyhow!(
                                    "process {} exhausted its restart policy; StopProject",
                                    participant.spec.key
                                ));
                                board.set_lifecycle(ProjectLifecycle::Failed);
                                token.cancel();
                                break 'supervision;
                            }
                            RuntimeFailurePolicy::RecreateGraph => {
                                supervisor_error = Some(anyhow::anyhow!(
                                    "process {} requires graph recreation",
                                    participant.spec.key
                                ));
                                board.set_lifecycle(ProjectLifecycle::Failed);
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
    if let Some(requested_stop) = options.requested_stop.take() {
        request_participant_stop(&mut running, &board, requested_stop).await;
    }
    shutdown_all(&mut running, &board).await;
    if let Some(error) = supervisor_error {
        return Err(error);
    }
    board.set_lifecycle(ProjectLifecycle::Stopped);
    Ok(SupervisorOutcome {
        failed_participants: board.snapshot().failed_participants(),
    })
}

pub(crate) async fn request_participant_stop(
    running: &mut [RunningParticipant],
    board: &BoardBackend,
    requested_stop: RequestedStop,
) {
    let Some(participant) = running
        .iter_mut()
        .find(|participant| participant.spec.key == requested_stop.key)
    else {
        return;
    };
    if participant.child.is_none() {
        if participant.restart_at.take().is_some() {
            participant.failed = true;
            board.set_state(
                &participant.spec.key,
                ParticipantState::Failed,
                Some("crashed before requested stop while restart was pending".to_string()),
            );
        }
        return;
    }

    board.append_log(
        &participant.spec.key,
        "supervisor: sending SIGTERM for requested stop",
    );
    let Some(pid) = participant.child.as_ref().and_then(|child| child.id()) else {
        board.set_state(
            &participant.spec.key,
            ParticipantState::Failed,
            Some("requested-stop child has no pid".to_string()),
        );
        return;
    };
    let terminate_sent = match if participant.spec.process_group {
        send_process_group_terminate(pid)
    } else {
        send_terminate(pid)
    } {
        Ok(()) => {
            board.append_log(
                &participant.spec.key,
                "supervisor: SIGTERM sent; waiting for child exit",
            );
            true
        }
        Err(error) => {
            board.append_log(
                &participant.spec.key,
                format!("supervisor: failed to send SIGTERM; waiting before fallback: {error:#}"),
            );
            false
        }
    };

    match participant
        .wait_for_requested_stop(board, requested_stop.grace, terminate_sent)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            if let Err(error) = participant.kill_process_group_after_timeout(board).await {
                board.set_state(
                    &participant.spec.key,
                    ParticipantState::Failed,
                    Some(format!("process-group SIGKILL failed: {error:#}")),
                );
            }
        }
        Err(error) => {
            board.set_state(
                &participant.spec.key,
                ParticipantState::Failed,
                Some(format!("requested-stop wait failed: {error:#}")),
            );
        }
    }
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
    running: &mut Vec<RunningParticipant>,
    board: &BoardBackend,
    action: SupervisorAction,
) -> Result<()> {
    match action {
        SupervisorAction::ReconcilePlan {
            revision,
            specs,
            remove_ids,
            note,
        } => {
            for remove_id in remove_ids {
                if let Some(index) = running
                    .iter()
                    .position(|participant| participant.spec.id == remove_id)
                {
                    running[index].stop_current(board).await?;
                    running.remove(index);
                }
            }
            for spec in specs {
                let key = spec.key.clone();
                if let Some(index) = running
                    .iter()
                    .position(|participant| participant.spec.key == key)
                {
                    if running[index].spec != spec {
                        running[index].swap(spec, board, note.clone()).await?;
                    }
                } else {
                    let phase = canonical_phase(&spec);
                    let participant =
                        RunningParticipant::spawn_in_phase(spec, board, phase).await?;
                    running.push(participant);
                }
            }
            let accepted_revision = board.activate_next_plan_revision();
            tracing::info!(
                accepted_revision,
                candidate_revision = revision.number,
                candidate_digest = %revision.digest,
                "activated immutable plan revision"
            );
            Ok(())
        }
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

pub(crate) async fn shutdown_all(running: &mut [RunningParticipant], board: &BoardBackend) {
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
                    board.append_log(&spec.key, "supervisor: stopping");
                    if let Err(error) =
                        stop_child(child, spec.shutdown_grace, spec.process_group).await
                    {
                        board.set_state(
                            &spec.key,
                            ParticipantState::Failed,
                            Some(format!("failed to stop: {error:#}")),
                        );
                    }
                    board.set_process_details(&spec.key, None, None);
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

fn canonical_phase(spec: &ParticipantSpec) -> &'static str {
    if spec.kind == phoxal_cli_core::session::ParticipantKind::Tool || spec.id == "webots" {
        "starting project infrastructure"
    } else {
        "starting robot graph"
    }
}

fn phase_rank(phase: &str) -> u8 {
    match phase {
        "starting project infrastructure" => 0,
        "starting robot graph" => 1,
        _ => 2,
    }
}
