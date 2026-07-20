//! Main supervision loop, actions, and orderly shutdown.

use super::{
    BoardBackend, HEARTBEAT_STALE_TIMEOUT, ParticipantState, RequestedStop, RunningParticipant,
    SupervisionStage, SupervisorAction, SupervisorOptions, SupervisorOutcome, await_stage_ready,
    emit_event, join_reader, maybe_emit_staged_startup_complete, send_process_group_terminate,
    send_terminate, spawn_until_pending, stop_child,
};
use crate::session::output::WaitBudget;
use anyhow::Context;
use anyhow::Result;
use std::collections::VecDeque;
use std::time::Duration;
use std::time::Instant;
use tokio::process::Child;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

pub async fn supervise_until_shutdown(
    stages: Vec<SupervisionStage>,
    board: BoardBackend,
    mut options: SupervisorOptions,
) -> Result<SupervisorOutcome> {
    let mut running = Vec::new();
    let mut stage_queue: VecDeque<SupervisionStage> = stages.into();
    let token = options.token.clone();
    let events_tx = options.events.take();

    // Spawn every leading stage that has nothing to wait for back-to-back
    // (uncommon in practice - every real stage waits on at least the router
    // or a heartbeat - but keeps a zero-wait stage from stalling a whole
    // startup on an empty `select!` branch), then park on the first stage
    // that actually gates the next one.
    let mut pending_stage =
        spawn_until_pending(&mut running, &board, events_tx.as_ref(), &mut stage_queue).await;
    maybe_emit_staged_startup_complete(&options, events_tx.as_ref(), &pending_stage).await;

    let mut ticker = tokio::time::interval(Duration::from_millis(500));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut action_rx = options.action_rx.take();
    let mut supervisor_error = None;
    'supervision: loop {
        tokio::select! {
            () = token.cancelled() => {
                break;
            }
            action = recv_action(&mut action_rx) => {
                if let Some(action) = action
                    && let Err(error) = handle_action(&mut running, &board, action).await
                {
                    board.append_log(
                        "supervisor",
                        format!("supervisor: action failed; shutting down graph: {error:#}"),
                    );
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
                pending_stage.as_ref().map_or(WaitBudget::Unbounded, |stage| match stage.deadline {
                    Some(deadline) => WaitBudget::Bounded(deadline.saturating_duration_since(Instant::now())),
                    None => WaitBudget::Unbounded,
                }),
                Duration::from_millis(200),
            ), if pending_stage.is_some() => {
                let stage = pending_stage.take().expect("guarded by is_some");
                match result {
                    Ok(()) => {
                        board.append_log("supervisor", format!("supervisor: stage '{}' ready", stage.label));
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
                        maybe_emit_staged_startup_complete(&options, events_tx.as_ref(), &pending_stage).await;
                    }
                    Err(error) => {
                        let reason = format!("stage '{}' stalled: {error:#}", stage.label);
                        board.append_log(
                            "supervisor",
                            format!("supervisor: {reason}"),
                        );
                        emit_event(events_tx.as_ref(), phoxal_cli_core::session::event::SessionEvent::PhaseFinished {
                            id: phoxal_cli_core::session::event::PhaseId::new(stage.label.clone()),
                            outcome: phoxal_cli_core::session::event::PhaseOutcome::Failed { error: format!("{error:#}") },
                            elapsed: stage.started.elapsed(),
                        }).await;
                        pending_stage = spawn_until_pending(
                            &mut running,
                            &board,
                            events_tx.as_ref(),
                            &mut stage_queue,
                        ).await;
                        maybe_emit_staged_startup_complete(
                            &options,
                            events_tx.as_ref(),
                            &pending_stage,
                        ).await;
                    }
                }
            }
            _ = ticker.tick() => {
                for participant in &mut running {
                    if let Err(error) = participant.poll(&board, &options.restart_policy).await {
                        board.append_log(
                            "supervisor",
                            format!(
                                "supervisor: participant poll failed; shutting down graph: {error:#}"
                            ),
                        );
                        supervisor_error = Some(error);
                        break 'supervision;
                    }
                }
                board.mark_stale_heartbeats(HEARTBEAT_STALE_TIMEOUT);
            }
        }
    }

    if let Some(requested_stop) = options.requested_stop.take() {
        request_participant_stop(&mut running, &board, requested_stop).await;
    }
    shutdown_all(&mut running, &board).await;
    if let Some(error) = supervisor_error {
        return Err(error).context("supervisor failed after shutting down the process graph");
    }
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
        .find(|participant| participant.spec.id == requested_stop.participant_id)
    else {
        return;
    };
    if participant.child.is_none() {
        if participant.restart_at.take().is_some() {
            participant.failed = true;
            board.set_state(
                &participant.spec.id,
                ParticipantState::Failed,
                Some("crashed before requested stop while restart was pending".to_string()),
            );
        }
        return;
    }

    board.append_log(
        &participant.spec.id,
        "supervisor: sending SIGTERM for requested stop",
    );
    let Some(pid) = participant.child.as_ref().and_then(Child::id) else {
        board.set_state(
            &participant.spec.id,
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
                &participant.spec.id,
                "supervisor: SIGTERM sent; waiting for child exit",
            );
            true
        }
        Err(error) => {
            board.append_log(
                &participant.spec.id,
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
                    &participant.spec.id,
                    ParticipantState::Failed,
                    Some(format!("process-group SIGKILL failed: {error:#}")),
                );
            }
        }
        Err(error) => {
            board.set_state(
                &participant.spec.id,
                ParticipantState::Failed,
                Some(format!("requested-stop wait failed: {error:#}")),
            );
        }
    }
}

pub(crate) async fn recv_action(
    action_rx: &mut Option<mpsc::Receiver<SupervisorAction>>,
) -> Option<SupervisorAction> {
    let action = match action_rx {
        Some(action_rx) => action_rx.recv().await,
        None => return std::future::pending().await,
    };
    if action.is_none() {
        action_rx.take();
    }
    action
}

pub(crate) async fn handle_action(
    running: &mut [RunningParticipant],
    board: &BoardBackend,
    action: SupervisorAction,
) -> Result<()> {
    match action {
        SupervisorAction::Swap { id, spec, note } => {
            let Some(participant) = running
                .iter_mut()
                .find(|participant| participant.spec.id == id)
            else {
                board.append_log(&id, "supervisor: swap requested for unknown participant");
                return Ok(());
            };
            participant.swap(spec, board, note).await
        }
        SupervisorAction::Restart { id } => {
            let Some(participant) = running
                .iter_mut()
                .find(|participant| participant.spec.id == id)
            else {
                board.append_log(&id, "supervisor: restart requested for unknown participant");
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
    for participant in running.iter_mut().rev() {
        if let Some(mut child) = participant.child.take() {
            board.append_log(&participant.spec.id, "supervisor: stopping");
            if let Err(error) = stop_child(
                &mut child,
                participant.spec.shutdown_grace,
                participant.spec.process_group,
            )
            .await
            {
                board.set_state(
                    &participant.spec.id,
                    ParticipantState::Failed,
                    Some(format!("failed to stop: {error:#}")),
                );
            }
            board.set_process_details(&participant.spec.id, None, None);
        }
        join_reader(participant.stdout_task.take()).await;
        join_reader(participant.stderr_task.take()).await;
    }
}
