//! Ordered participant startup and readiness barriers.

use super::participant::RunningParticipant;
use super::spec::SupervisorOptions;
use crate::model::launch::ParticipantSpec;
use crate::model::lifecycle::ProjectLifecycle;
use crate::model::process::{ProcessFailureKind, ProcessKey, ProcessState};
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
pub(crate) fn stages_for_run(
    specs: Vec<ParticipantSpec>,
    timeout: WaitBudget,
) -> Vec<SupervisionStage> {
    vec![SupervisionStage::new(
        SupervisionStageKind::Graph,
        specs,
        timeout,
    )]
}

/// How supervision reports stage progress to whoever owns the startup
/// sequence.
///
/// The daemon owns the rendered bundle, router, and participant steps. This
/// process layer advances only the participant step through this handle, which
/// keeps it independent of the wire contract.
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
pub(crate) struct SupervisionStage {
    kind: SupervisionStageKind,
    specs: Vec<ParticipantSpec>,
    /// Board ids that must be observed `Ready` before the next stage spawns:
    /// every spawned spec's own id that is a bus participant (see
    /// [`Self::new`]). There are no wait-only ids - the embedded router is the
    /// supervisor's own state, not a board row to wait on.
    ready_ids: Vec<ProcessKey>,
    timeout: WaitBudget,
}

impl SupervisionStage {
    #[must_use]
    pub(crate) fn new(
        kind: SupervisionStageKind,
        specs: Vec<ParticipantSpec>,
        timeout: WaitBudget,
    ) -> Self {
        let ready_ids = specs.iter().map(|spec| spec.key.clone()).collect();
        Self {
            kind,
            specs,
            ready_ids,
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
            board.register_planned(&spec.key);
            continue;
        }
        let key = spec.key.clone();
        // Normal planning pre-registers every expected participant before the
        // observer starts. Keep this authoritative, idempotent registration at
        // the stage boundary for direct stage tests and defensive consistency;
        // unsolicited Liveliness and logs still cannot create board entries.
        board.register_planned(&key);
        match RunningParticipant::spawn_in_stage(spec, board, stage).await {
            Ok(participant) => running.push(participant),
            Err(error) => {
                board.record_failure(
                    &key,
                    ProcessFailureKind::Spawn,
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
    /// `None` means an unbounded wait, so there is
    /// no `Instant` to ever compare against.
    pub(crate) deadline: Option<Instant>,
}

/// Spawn one stage's participants and return its readiness barrier when it has
/// work to await.
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
        let failed = ready_ids
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
            })
            .cloned()
            .collect::<Vec<_>>();
        if missing.is_empty() {
            return Ok(());
        }
        // An unbounded wait has no deadline; a missing participant remains
        // pending until the operator ends the session.
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            let waited = format_duration(started.elapsed());
            for key in &missing {
                board.record_failure(
                    key,
                    ProcessFailureKind::ReadinessTimeout,
                    None,
                    format!("stage readiness timed out after {waited}: never observed ready"),
                );
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

/// Publish the state owner's Ready startup outcome when no phase remains to
/// spawn or await.
pub(crate) async fn maybe_publish_startup_outcome(
    board: &SupervisorState,
    options: &SupervisorOptions,
    pending_stage: &Option<PendingStage>,
) {
    if options.publishes_running_on_startup_complete && pending_stage.is_none() {
        board.set_lifecycle(ProjectLifecycle::Ready);
    }
}

#[must_use]
pub(crate) fn format_duration(value: Duration) -> String {
    if value < Duration::from_secs(1) {
        return format!("{}ms", value.as_millis());
    }
    if value < Duration::from_secs(60) {
        return format!("{:.1}s", value.as_secs_f64());
    }
    let seconds = value.as_secs();
    if seconds < 60 * 60 {
        return format!("{}m {:02}s", seconds / 60, seconds % 60);
    }
    format!("{}h {:02}m", seconds / (60 * 60), (seconds / 60) % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::participant::ParticipantKind;
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

    #[tokio::test]
    async fn every_ready_id_failure_aborts_the_startup_barrier() {
        let board = SupervisorState::new();
        let failed = spec("drive", ParticipantKind::Driver).key;
        board.register_planned(&failed);
        board.record_failure(&failed, ProcessFailureKind::Spawn, None, "spawn failed");

        let error = await_stage_ready(
            &board,
            &[failed],
            WaitBudget::Unbounded,
            Duration::from_millis(1),
        )
        .await
        .expect_err("a failed ready participant aborts startup");
        assert!(error.to_string().contains("drive"), "{error:#}");
    }

    #[test]
    fn formats_elapsed_time_at_useful_precision() {
        assert_eq!(format_duration(Duration::from_millis(250)), "250ms");
        assert_eq!(format_duration(Duration::from_millis(1500)), "1.5s");
        assert_eq!(format_duration(Duration::from_secs(65)), "1m 05s");
        assert_eq!(format_duration(Duration::from_secs(3_720)), "1h 02m");
    }
}
