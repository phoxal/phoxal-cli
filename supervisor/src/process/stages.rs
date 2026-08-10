//! Ordered participant startup and readiness barriers.

use super::participant::RunningParticipant;
use super::spec::SupervisorOptions;
use crate::model::{
    ParticipantSpec, ProcessKey, ProcessState, ProjectLifecycle, StartupRequirement,
};
use crate::state::store::SupervisorState;
use anyhow::Result;
use anyhow::bail;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio::time::MissedTickBehavior;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitBudget {
    Unbounded,
    Bounded(Duration),
}

impl Default for WaitBudget {
    fn default() -> Self {
        Self::Bounded(Duration::default())
    }
}

impl WaitBudget {
    fn deadline_from(self, now: Instant) -> Option<Instant> {
        match self {
            Self::Unbounded => None,
            Self::Bounded(duration) => Some(now + duration),
        }
    }
}

/// Build the participant startup barrier for a physical run.
pub fn stages_for_run(specs: Vec<ParticipantSpec>, timeout: WaitBudget) -> Vec<SupervisionStage> {
    vec![SupervisionStage::new(
        SupervisionStageKind::Graph,
        specs,
        timeout,
    )]
}

/// How supervision reports stage progress to whoever owns the startup
/// sequence.
///
/// The store is not that owner: the startup sequence a client renders is the
/// daemon's own (bundle, router, participants), and
/// this loop only ever advances its last step. Keeping it
/// behind this handle is what lets the process machinery stay ignorant of the
/// wire contract.
pub(crate) trait StageProgress: Send + Sync {
    /// A stage began, named by its human-readable label.
    fn started(&self, label: &str);
    /// A stage completed with an observation worth showing.
    fn detail(&self, detail: String);
    /// Every stage completed.
    fn finished(&self);
    /// A stage failed, and the graph is unwinding.
    fn failed(&self, reason: &str);
}

/// A reporter that discards everything, for a caller with no startup sequence
/// to advance (every direct stage test).
#[cfg(test)]
pub(crate) struct SilentProgress;

#[cfg(test)]
impl SilentProgress {
    pub(crate) fn reporter() -> StageReporter {
        Arc::new(Self)
    }
}

#[cfg(test)]
impl StageProgress for SilentProgress {
    fn started(&self, _label: &str) {}
    fn detail(&self, _detail: String) {}
    fn finished(&self) {}
    fn failed(&self, _reason: &str) {}
}

pub(crate) type StageReporter = Arc<dyn StageProgress>;

/// A startup barrier containing process specs and board ids that must become
/// ready before the next stage begins. Simulation clock is not a stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SupervisionStageKind {
    Graph,
}

impl SupervisionStageKind {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Graph => "starting robot graph",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SupervisionStage {
    pub(crate) kind: SupervisionStageKind,
    pub specs: Vec<ParticipantSpec>,
    /// Board ids that must be observed `Ready` before the next stage spawns:
    /// every spawned spec's own id that is a bus participant (see
    /// [`Self::new`]). There are no wait-only ids - the embedded router is the
    /// supervisor's own state, not a board row to wait on.
    pub ready_ids: Vec<ProcessKey>,
    /// Spawned processes whose terminal failure aborts this stage.
    pub failure_ids: Vec<ProcessKey>,
    pub optional_ids: Vec<ProcessKey>,
    pub timeout: WaitBudget,
}

impl SupervisionStage {
    #[must_use]
    pub fn new(
        kind: SupervisionStageKind,
        specs: Vec<ParticipantSpec>,
        timeout: WaitBudget,
    ) -> Self {
        let ready_ids = specs.iter().map(|spec| spec.key.clone()).collect();
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
            kind,
            specs,
            ready_ids,
            failure_ids,
            optional_ids,
            timeout,
        }
    }
}

pub(crate) async fn spawn_participants_in_stage(
    running: &mut Vec<RunningParticipant>,
    board: &SupervisorState,
    stage: SupervisionStageKind,
    specs: Vec<ParticipantSpec>,
) {
    for spec in specs {
        if !spec.spawn {
            board.register_planned(&spec.key, spec.kind, spec.startup_requirement);
            continue;
        }
        let key = spec.key.clone();
        // Normal planning pre-registers every expected participant before the
        // observer starts. Keep this authoritative, idempotent registration at
        // the stage boundary for direct stage tests and defensive consistency;
        // unsolicited Liveliness and logs still cannot create board entries.
        board.register_planned(&key, spec.kind, spec.startup_requirement);
        match RunningParticipant::spawn_in_stage(spec, board, stage).await {
            Ok(participant) => running.push(participant),
            Err(error) => {
                board.record_failure(
                    &key,
                    crate::model::ProcessFailureKind::Spawn,
                    None,
                    format!("spawn failed: {error:#}"),
                );
            }
        }
    }
}

/// A stage currently being waited on: its expected ready ids, its deadline,
/// and when it started.
pub(crate) struct PendingStage {
    pub(crate) label: String,
    pub(crate) ready_ids: Vec<ProcessKey>,
    pub(crate) failure_ids: Vec<ProcessKey>,
    pub(crate) optional_ids: Vec<ProcessKey>,
    /// `None` means an unbounded wait, so there is
    /// no `Instant` to ever compare against.
    pub(crate) deadline: Option<Instant>,
}

/// Spawn one stage's participants (if it has any work at all - Product
/// decision 3) and return the readiness barrier when it has work to await.
pub(crate) async fn spawn_stage(
    running: &mut Vec<RunningParticipant>,
    board: &SupervisorState,
    progress: &StageReporter,
    stage: SupervisionStage,
) -> Option<PendingStage> {
    let SupervisionStage {
        kind,
        specs,
        ready_ids,
        failure_ids,
        optional_ids,
        timeout: stage_timeout,
    } = stage;
    let label = kind.label().to_string();
    progress.started(&label);
    if specs.is_empty() && ready_ids.is_empty() {
        return None;
    }
    spawn_participants_in_stage(running, board, kind, specs).await;
    if ready_ids.is_empty() {
        return None;
    }
    Some(PendingStage {
        label,
        ready_ids,
        failure_ids,
        optional_ids,
        deadline: stage_timeout.deadline_from(Instant::now()),
    })
}

pub(crate) async fn spawn_until_pending(
    running: &mut Vec<RunningParticipant>,
    board: &SupervisorState,
    progress: &StageReporter,
    stage_queue: &mut VecDeque<SupervisionStage>,
) -> Option<PendingStage> {
    while let Some(stage) = stage_queue.pop_front() {
        if let Some(pending) = spawn_stage(running, board, progress, stage).await {
            return Some(pending);
        }
    }
    if stage_queue.is_empty() {
        progress.finished();
    }
    None
}

pub(crate) async fn await_stage_ready(
    board: &SupervisorState,
    ready_ids: &[ProcessKey],
    failure_ids: &[ProcessKey],
    optional_ids: &[ProcessKey],
    budget: WaitBudget,
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
        // `deadline` is `None` for an unbounded wait, so
        // there is nothing to ever compare `Instant::now()` against, so a
        // missing participant simply keeps waiting for as long as the
        // operator leaves the session open.
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            let waited = crate::model::format_duration(started.elapsed());
            for key in &missing {
                board.record_failure(
                    key,
                    crate::model::ProcessFailureKind::ReadinessTimeout,
                    None,
                    format!("stage readiness timed out after {waited}: never observed ready"),
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
pub(crate) async fn maybe_publish_startup_outcome(
    board: &SupervisorState,
    options: &SupervisorOptions,
    pending_stage: &Option<PendingStage>,
) {
    if options.publishes_running_on_startup_complete && pending_stage.is_none() {
        let snapshot = board.snapshot();
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ParticipantKind, RuntimeFailurePolicy, StartupRequirement};
    use std::path::PathBuf;

    fn spec(id: &str, kind: ParticipantKind) -> ParticipantSpec {
        ParticipantSpec {
            spawn: true,
            key: phoxal_runtime_contract::identity::ParticipantId::new(id)
                .expect("fixture participant")
                .into(),
            kind,
            executable: PathBuf::from(id),
            args: Vec::new(),
            shutdown_grace: Duration::from_secs(1),
            startup_requirement: StartupRequirement::Required,
            runtime_failure: RuntimeFailurePolicy::StopProject,
            restart_policy: Default::default(),
        }
    }

    #[test]
    fn run_places_every_compiled_participant_in_one_graph_barrier() {
        let stages = stages_for_run(
            vec![
                spec("brain", ParticipantKind::Brain),
                spec("service", ParticipantKind::Service),
                spec("webots", ParticipantKind::Simulator),
            ],
            WaitBudget::Unbounded,
        );
        assert_eq!(stages.len(), 1);
        assert_eq!(stages[0].kind.label(), "starting robot graph");
        assert_eq!(
            stages[0]
                .specs
                .iter()
                .map(|spec| spec.key.to_string())
                .collect::<Vec<_>>(),
            ["brain", "service", "webots"]
        );
    }
}
