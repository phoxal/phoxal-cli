//! Ordered participant startup and readiness barriers.

use super::{
    BoardBackend, ParticipantSpec, ParticipantState, READER_JOIN_BUDGET, RunningParticipant,
    SupervisorOptions,
};
use anyhow::Result;
use anyhow::bail;
use phoxal_cli_core::session::human;
use phoxal_cli_core::session::{ProcessKey, ProcessState, ProjectLifecycle, StartupRequirement};
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
    pub ready_ids: Vec<ProcessKey>,
    /// Spawned processes whose terminal failure aborts this stage.
    pub failure_ids: Vec<ProcessKey>,
    pub optional_ids: Vec<ProcessKey>,
    pub timeout: crate::session::output::WaitBudget,
}

/// Maximum time the controller should allow the supervisor to stop every
/// launched process sequentially after the first cancel request. Each
/// participant gets its authored grace, the one-second process-group reap
/// allowance used by [`kill_child_process_group`], and two bounded reader
/// joins for stdout and stderr. A second cancel still forces an immediate exit.
#[must_use]
pub fn orderly_shutdown_budget(stages: &[SupervisionStage]) -> Duration {
    stages.iter().fold(Duration::from_secs(1), |budget, stage| {
        let phase_max = stage.specs.iter().fold(Duration::ZERO, |maximum, spec| {
            maximum.max(
                spec.shutdown_grace
                    .saturating_add(Duration::from_secs(1))
                    .saturating_add(READER_JOIN_BUDGET.saturating_mul(2)),
            )
        });
        budget.saturating_add(phase_max)
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
            .map(|spec| spec.key.clone())
            .collect();
        let failure_ids = specs
            .iter()
            .filter(|spec| spec.startup_requirement == StartupRequirement::Required)
            .map(|spec| spec.key.clone())
            .collect();
        let optional_ids = specs
            .iter()
            .filter(|spec| spec.startup_requirement == StartupRequirement::Optional)
            .map(|spec| spec.key.clone())
            .collect();
        Self {
            label: label.into(),
            specs,
            ready_ids,
            failure_ids,
            optional_ids,
            timeout,
        }
    }

    #[must_use]
    pub fn with_extra_ready_ids(mut self, ids: impl IntoIterator<Item = ProcessKey>) -> Self {
        let ids = ids.into_iter().collect::<Vec<_>>();
        self.ready_ids.extend(ids.iter().cloned());
        self.failure_ids.extend(ids);
        self
    }
}

pub(crate) async fn spawn_stage(
    running: &mut Vec<RunningParticipant>,
    board: &BoardBackend,
    phase: &str,
    specs: Vec<ParticipantSpec>,
) {
    for spec in specs {
        let key = spec.key.clone();
        // Normal planning pre-registers every expected participant before the
        // observer starts. Keep this authoritative, idempotent registration at
        // the stage boundary for direct stage tests and defensive consistency;
        // unsolicited Liveliness and logs still cannot create board entries.
        board.register_planned(&key, spec.kind, spec.startup_requirement);
        match RunningParticipant::spawn_in_phase(spec, board, phase).await {
            Ok(participant) => running.push(participant),
            Err(error) => {
                board.set_state(
                    &key,
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
    pub(crate) ready_ids: Vec<ProcessKey>,
    pub(crate) failure_ids: Vec<ProcessKey>,
    pub(crate) optional_ids: Vec<ProcessKey>,
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
        optional_ids,
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
    board.begin_phase(&label);
    spawn_stage(running, board, &label, specs).await;
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
        optional_ids,
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
/// success with no side effects; a direct process `Failed` state returns
/// `Err` immediately (never waits out the timeout for a graph that already
/// ended unhealthy); on timeout it marks every still-missing id
/// `Failed` on the board (so `SupervisorOutcome::graph_healthy` reflects the
/// stall) and returns `Err` naming exactly what never showed up.
#[cfg(test)]
pub async fn await_participants_ready<T>(
    board: &BoardBackend,
    stage_ids: &[T],
    budget: crate::session::output::WaitBudget,
    poll_interval: Duration,
) -> Result<()>
where
    T: Clone + Into<ProcessKey>,
{
    let stage_ids = stage_ids
        .iter()
        .cloned()
        .map(Into::into)
        .collect::<Vec<_>>();
    await_stage_ready(board, &stage_ids, &stage_ids, &[], budget, poll_interval).await
}

pub(crate) async fn await_stage_ready(
    board: &BoardBackend,
    ready_ids: &[ProcessKey],
    failure_ids: &[ProcessKey],
    optional_ids: &[ProcessKey],
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
        let failed = failure_ids
            .iter()
            .filter(|key| board.process_state(key) == Some(ProcessState::Failed))
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        if !failed.is_empty() {
            bail!(
                "stage ended unhealthy; failed participants: {}",
                failed.join(", ")
            );
        }
        let missing = ready_ids
            .iter()
            .filter(|key| {
                let state = board.process_state(key);
                state != Some(ProcessState::Ready)
                    && !(optional_ids.contains(key) && state == Some(ProcessState::Failed))
            })
            .cloned()
            .collect::<Vec<_>>();
        if missing.is_empty() {
            return Ok(());
        }
        // `deadline` is `None` for an unbounded wait (Product decision 6) -
        // there is nothing to ever compare `Instant::now()` against, so a
        // missing participant simply keeps waiting for as long as the
        // operator leaves the session open.
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            let waited = human::duration(started.elapsed());
            for key in &missing {
                board.set_state(
                    key,
                    ParticipantState::Failed,
                    Some(format!(
                        "stage readiness timed out after {waited}: never observed ready"
                    )),
                );
            }
            if missing.iter().all(|key| optional_ids.contains(key)) {
                return Ok(());
            }
            bail!(
                "stage readiness timed out after {waited}: participant(s) never observed ready: {}",
                missing
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
}

/// Publish the state owner's derived Ready/Degraded startup outcome exactly
/// when no phase remains to spawn or await.
pub(crate) async fn maybe_emit_startup_outcome(
    board: &BoardBackend,
    options: &SupervisorOptions,
    events: Option<&mpsc::Sender<phoxal_cli_core::session::event::SessionEvent>>,
    pending_stage: &Option<PendingStage>,
) {
    if options.emits_running_on_startup_complete && pending_stage.is_none() {
        let snapshot = board.supervisor_snapshot();
        let degraded = snapshot.processes.values().any(|entry| {
            matches!(
                entry.status.actual,
                ProcessState::Failed | ProcessState::Degraded
            )
        });
        board.set_lifecycle(if degraded {
            ProjectLifecycle::Degraded
        } else {
            ProjectLifecycle::Ready
        });
        emit_event(
            events,
            phoxal_cli_core::session::event::SessionEvent::SessionChanged {
                state: phoxal_cli_core::session::state::SessionState::Running,
            },
        )
        .await;
    }
}
