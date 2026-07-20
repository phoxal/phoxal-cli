//! Ordered participant startup and readiness barriers.

use super::{
    BoardBackend, ParticipantSpec, ParticipantState, READER_JOIN_BUDGET, RunningParticipant,
    SupervisorOptions, failed_expected_participants, missing_ready_participants,
};
use anyhow::Result;
use anyhow::bail;
use phoxal_cli_core::session::human;
use std::collections::VecDeque;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

/// A startup barrier containing process specs and board ids that must become
/// ready before the next stage begins. Simulation clock is not a stage.
#[derive(Debug, Clone, Default)]
pub struct SupervisionStage {
    /// Human-readable name for this stage, used in the stalled-stage error
    /// message and as the `PhaseId`/label of the `SessionEvent::PhaseStarted`/
    /// `PhaseFinished` pair this stage emits (see `spawn_stage_emitting`).
    pub label: String,
    pub specs: Vec<ParticipantSpec>,
    /// Board ids that must be observed `Ready` before the next stage spawns.
    /// Defaults to every spawned spec's own id that is a bus participant
    /// (see [`Self::new`]); extend with [`Self::with_extra_ready_ids`] for a
    /// wait-only id that has no `ParticipantSpec` of its own.
    pub ready_ids: Vec<String>,
    /// Spawned processes whose terminal failure aborts this stage.
    pub failure_ids: Vec<String>,
    pub timeout: crate::session::output::WaitBudget,
}

/// Maximum time the controller should allow the supervisor to stop every
/// launched process sequentially after the first cancel request. Each
/// participant gets its authored grace, the one-second process-group reap
/// allowance used by [`kill_child_process_group`], and two bounded reader
/// joins for stdout and stderr. A second cancel still forces an immediate exit.
#[must_use]
pub fn orderly_shutdown_budget(stages: &[SupervisionStage]) -> Duration {
    stages
        .iter()
        .flat_map(|stage| &stage.specs)
        .fold(Duration::from_secs(1), |budget, spec| {
            budget
                .saturating_add(spec.shutdown_grace)
                .saturating_add(Duration::from_secs(1))
                .saturating_add(READER_JOIN_BUDGET.saturating_mul(2))
        })
}

impl SupervisionStage {
    #[must_use]
    pub fn new(
        label: impl Into<String>,
        specs: Vec<ParticipantSpec>,
        timeout: crate::session::output::WaitBudget,
    ) -> Self {
        let ready_ids = specs
            .iter()
            .filter(|spec| spec.bus_participant)
            .map(|spec| spec.id.clone())
            .collect();
        let failure_ids = specs.iter().map(|spec| spec.id.clone()).collect();
        Self {
            label: label.into(),
            specs,
            ready_ids,
            failure_ids,
            timeout,
        }
    }

    #[must_use]
    pub fn with_extra_ready_ids(mut self, ids: impl IntoIterator<Item = String>) -> Self {
        self.ready_ids.extend(ids);
        self
    }
}

pub(crate) async fn spawn_stage(
    running: &mut Vec<RunningParticipant>,
    board: &BoardBackend,
    specs: Vec<ParticipantSpec>,
) {
    for spec in specs {
        let id = spec.id.clone();
        // A stage is an authoritative, locally constructed launch plan. It
        // may therefore register its own row when the stage becomes active,
        // while unsolicited wire heartbeats and logs remain unable to create
        // board entries through `record_heartbeat`/`route_log`.
        board.register_planned(&id, spec.kind);
        match RunningParticipant::spawn(spec, board).await {
            Ok(participant) => running.push(participant),
            Err(error) => {
                board.set_state(
                    &id,
                    ParticipantState::Failed,
                    Some(format!("spawn failed: {error:#}")),
                );
            }
        }
    }
}

/// A stage currently being waited on: its expected ready ids, its deadline,
/// and when it started (so the eventual `PhaseFinished` can report real
/// elapsed time) - the loop's replacement for a raw
/// `(String, Vec<String>, Instant)` tuple, which the finished-elapsed
/// bookkeeping outgrew.
pub(crate) struct PendingStage {
    pub(crate) label: String,
    pub(crate) ready_ids: Vec<String>,
    pub(crate) failure_ids: Vec<String>,
    /// `None` for an unbounded wait (Product decision 6/finding D2) - there is
    /// no `Instant` to ever compare against.
    pub(crate) deadline: Option<Instant>,
    pub(crate) started: Instant,
}

/// Send `event` to `events`, if a `SessionController` is listening. Awaits
/// the send so a lifecycle/control transition (a phase or session-state
/// change) can never be silently dropped under channel backpressure (finding
/// B5) - only [`crate::session::diagnostics`]'s much higher-volume, lower-
/// severity telemetry/log routing keeps a non-blocking `try_send`. The
/// channel is generously bounded (`EVENT_CHANNEL_CAPACITY`) and the
/// controller's own select loop drains it continuously, so this resolves
/// promptly under normal operation; a truly gone/closed receiver (the
/// controller already tore down) makes the send fail immediately rather than
/// hang, which is why the result is still discarded here.
pub(crate) async fn emit_event(
    events: Option<&mpsc::Sender<phoxal_cli_core::session::event::SessionEvent>>,
    event: phoxal_cli_core::session::event::SessionEvent,
) {
    if let Some(sender) = events {
        let _ = sender.send(event).await;
    }
}

/// Spawn one stage's participants (if it has any work at all - Product
/// decision 3: never emit a phase for a stage with nothing to start) and, when
/// `events` is wired, emit its `PhaseStarted` immediately and `PhaseFinished`
/// too IF the stage has nothing further to wait for (`ready_ids` empty and no
/// router bus probe). A stage that DOES have readiness work returns its own
/// [`PendingStage`] instead; the caller's own loop emits its `PhaseFinished`
/// once `await_participants_ready` resolves.
pub(crate) async fn spawn_stage_emitting(
    running: &mut Vec<RunningParticipant>,
    board: &BoardBackend,
    events: Option<&mpsc::Sender<phoxal_cli_core::session::event::SessionEvent>>,
    stage: SupervisionStage,
) -> Option<PendingStage> {
    let SupervisionStage {
        label,
        specs,
        ready_ids,
        failure_ids,
        timeout: stage_timeout,
    } = stage;
    if specs.is_empty() && ready_ids.is_empty() {
        return None;
    }
    let started = Instant::now();
    emit_event(
        events,
        phoxal_cli_core::session::event::SessionEvent::PhaseStarted {
            id: phoxal_cli_core::session::event::PhaseId::new(label.clone()),
            label: label.clone(),
        },
    )
    .await;
    spawn_stage(running, board, specs).await;
    if ready_ids.is_empty() {
        emit_event(
            events,
            phoxal_cli_core::session::event::SessionEvent::PhaseFinished {
                id: phoxal_cli_core::session::event::PhaseId::new(label.clone()),
                outcome: phoxal_cli_core::session::event::PhaseOutcome::Succeeded,
                elapsed: started.elapsed(),
            },
        )
        .await;
        return None;
    }
    Some(PendingStage {
        label,
        ready_ids,
        failure_ids,
        deadline: stage_timeout.deadline_from(Instant::now()),
        started,
    })
}

pub(crate) async fn spawn_until_pending(
    running: &mut Vec<RunningParticipant>,
    board: &BoardBackend,
    events: Option<&mpsc::Sender<phoxal_cli_core::session::event::SessionEvent>>,
    stage_queue: &mut VecDeque<SupervisionStage>,
) -> Option<PendingStage> {
    while let Some(stage) = stage_queue.pop_front() {
        if let Some(pending) = spawn_stage_emitting(running, board, events, stage).await {
            return Some(pending);
        }
    }
    None
}

/// Wait until every id in `stage_ids` has been OBSERVED `Ready` on the
/// board, or `timeout` elapses. Shared by host and simulation staged startup;
/// returns `Ok(())` on
/// success with no side effects; on an explicit terminal `Failed` readiness
/// it returns `Err` immediately (never waits out the timeout for a graph
/// that already ended unhealthy); on timeout it marks every still-missing id
/// `Failed` on the board (so `SupervisorOutcome::graph_healthy` reflects the
/// stall) and returns `Err` naming exactly what never showed up.
#[cfg(test)]
pub async fn await_participants_ready(
    board: &BoardBackend,
    stage_ids: &[String],
    budget: crate::session::output::WaitBudget,
    poll_interval: Duration,
) -> Result<()> {
    await_stage_ready(board, stage_ids, stage_ids, budget, poll_interval).await
}

pub(crate) async fn await_stage_ready(
    board: &BoardBackend,
    ready_ids: &[String],
    failure_ids: &[String],
    budget: crate::session::output::WaitBudget,
    poll_interval: Duration,
) -> Result<()> {
    if ready_ids.is_empty() {
        return Ok(());
    }
    let started = Instant::now();
    let deadline = budget.deadline_from(started);
    let mut interval = tokio::time::interval(poll_interval);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let snapshot = board.snapshot();
        let failed = failed_expected_participants(&snapshot, failure_ids);
        if !failed.is_empty() {
            bail!(
                "stage ended unhealthy; failed participants: {}",
                failed.join(", ")
            );
        }
        let missing = missing_ready_participants(&snapshot, ready_ids);
        if missing.is_empty() {
            return Ok(());
        }
        // `deadline` is `None` for an unbounded wait (Product decision 6) -
        // there is nothing to ever compare `Instant::now()` against, so a
        // missing participant simply keeps waiting for as long as the
        // operator leaves the session open.
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            let waited = human::duration(started.elapsed());
            for id in &missing {
                board.set_state(
                    id,
                    ParticipantState::Failed,
                    Some(format!(
                        "stage readiness timed out after {waited}: never observed ready"
                    )),
                );
            }
            bail!(
                "stage readiness timed out after {waited}: participant(s) never observed ready: {}",
                missing.join(", ")
            );
        }
    }
}

/// Emit `SessionEvent::StagedStartupComplete` exactly when there is truly
/// nothing left to spawn or wait for (finding B3) - `pending_stage.is_none()`
/// after a stage-draining loop means the loop only stopped because the queue
/// was genuinely empty, not because it parked on a stage awaiting readiness
/// (a park always sets `pending_stage`). Only fires for a session that opted
/// in via `emits_running_on_startup_complete`.
pub(crate) async fn maybe_emit_staged_startup_complete(
    options: &SupervisorOptions,
    events: Option<&mpsc::Sender<phoxal_cli_core::session::event::SessionEvent>>,
    pending_stage: &Option<PendingStage>,
) {
    if options.emits_running_on_startup_complete && pending_stage.is_none() {
        emit_event(
            events,
            phoxal_cli_core::session::event::SessionEvent::StagedStartupComplete,
        )
        .await;
    }
}
