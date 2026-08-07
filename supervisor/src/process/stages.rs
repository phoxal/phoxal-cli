//! Ordered participant startup and readiness barriers.

use super::{ParticipantSpec, RunningParticipant, SupervisorOptions, SupervisorState};
use anyhow::Result;
use anyhow::bail;
use phoxal_cli_core::runtime::{ProcessKey, ProcessState, ProjectLifecycle, StartupRequirement};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio::time::MissedTickBehavior;

/// Build the ordered resident startup stages for a physical run.
pub fn stages_for_run(
    specs: Vec<ParticipantSpec>,
    timeout: crate::WaitBudget,
) -> Vec<SupervisionStage> {
    let mut infrastructure = Vec::new();
    let mut graph = Vec::new();
    for spec in specs {
        match spec.kind {
            phoxal_cli_core::runtime::ParticipantKind::Host => infrastructure.push(spec),
            phoxal_cli_core::runtime::ParticipantKind::Brain
            | phoxal_cli_core::runtime::ParticipantKind::Driver
            | phoxal_cli_core::runtime::ParticipantKind::Service
            | phoxal_cli_core::runtime::ParticipantKind::Simulator => graph.push(spec),
        }
    }
    vec![
        SupervisionStage::new("starting project infrastructure", infrastructure, timeout),
        SupervisionStage::new("starting robot graph", graph, timeout),
    ]
}

/// How supervision reports stage progress to whoever owns the startup
/// sequence.
///
/// The store is not that owner: the startup sequence a client renders is the
/// daemon's own (bundle, requirements, binaries, router, participants), and
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
#[derive(Debug, Clone)]
pub struct SupervisionStage {
    /// Human-readable name for this stage, used in stalled-stage errors and
    /// process spawn diagnostics. The typed startup step stays separate.
    pub label: String,
    pub specs: Vec<ParticipantSpec>,
    /// Board ids that must be observed `Ready` before the next stage spawns:
    /// every spawned spec's own id that is a bus participant (see
    /// [`Self::new`]). There are no wait-only ids - the embedded router is the
    /// supervisor's own state, not a board row to wait on.
    pub ready_ids: Vec<ProcessKey>,
    /// Spawned processes whose terminal failure aborts this stage.
    pub failure_ids: Vec<ProcessKey>,
    pub optional_ids: Vec<ProcessKey>,
    pub timeout: crate::WaitBudget,
}

impl SupervisionStage {
    #[must_use]
    pub fn new(
        label: impl Into<String>,
        specs: Vec<ParticipantSpec>,
        timeout: crate::WaitBudget,
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
}

pub(crate) async fn spawn_participants_in_stage(
    running: &mut Vec<RunningParticipant>,
    board: &SupervisorState,
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
                    ProcessState::Failed,
                    Some(format!("spawn failed: {error:#}")),
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
    /// `None` for an unbounded wait (Product decision 6/finding D2) - there is
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
        label,
        specs,
        ready_ids,
        failure_ids,
        optional_ids,
        timeout: stage_timeout,
    } = stage;
    progress.started(&label);
    if specs.is_empty() && ready_ids.is_empty() {
        return None;
    }
    spawn_participants_in_stage(running, board, &label, specs).await;
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
    budget: crate::WaitBudget,
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
            let waited = crate::format_duration(started.elapsed());
            for key in &missing {
                board.set_state(
                    key,
                    ProcessState::Failed,
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
    use phoxal_cli_core::runtime::{
        ParticipantKind, ReadinessPolicy, RuntimeFailurePolicy, StartupRequirement,
    };
    use std::path::PathBuf;

    fn spec(id: &str, kind: ParticipantKind) -> ParticipantSpec {
        ParticipantSpec {
            key: ProcessKey::project(id),
            id: id.to_string(),
            kind,
            executable: PathBuf::from(id),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            shutdown_grace: Duration::from_secs(1),
            process_group: false,
            note: None,
            bus_participant: true,
            readiness: ReadinessPolicy::ProcessSpawned,
            startup_requirement: StartupRequirement::Required,
            runtime_failure: RuntimeFailurePolicy::StopProject,
            restart_policy: Default::default(),
        }
    }

    /// The stage split is by kind: a host process is project infrastructure
    /// and every graph participant - the mandatory root brain included
    /// - waits behind it.
    #[test]
    fn run_assigns_host_processes_to_infrastructure_and_every_participant_to_the_graph() {
        let stages = stages_for_run(
            vec![
                spec("webots", ParticipantKind::Host),
                spec("brain", ParticipantKind::Brain),
                spec("service", ParticipantKind::Service),
            ],
            crate::WaitBudget::Unbounded,
        );
        assert_eq!(stages[0].label, "starting project infrastructure");
        assert_eq!(
            stages[0]
                .specs
                .iter()
                .map(|spec| spec.id.as_str())
                .collect::<Vec<_>>(),
            ["webots"]
        );
        assert_eq!(stages[1].label, "starting robot graph");
        assert_eq!(
            stages[1]
                .specs
                .iter()
                .map(|spec| spec.id.as_str())
                .collect::<Vec<_>>(),
            ["brain", "service"]
        );
    }
}
