//! Ordered participant startup and readiness barriers.

use super::{ParticipantSpec, RunningParticipant, SupervisorOptions, SupervisorState};
use anyhow::Result;
use anyhow::bail;
use phoxal_cli_core::runtime::{
    ProcessKey, ProcessState, ProjectLifecycle, StartupRequirement, StartupStepKind,
};
use std::collections::VecDeque;
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
            phoxal_cli_core::runtime::ParticipantKind::Tool => infrastructure.push(spec),
            phoxal_cli_core::runtime::ParticipantKind::Driver
            | phoxal_cli_core::runtime::ParticipantKind::Service
            | phoxal_cli_core::runtime::ParticipantKind::Simulator => graph.push(spec),
        }
    }
    vec![
        SupervisionStage::new(
            "starting project infrastructure",
            StartupStepKind::Infrastructure,
            infrastructure,
            timeout,
        )
        .with_extra_ready_ids([ProcessKey::project("infrastructure-router")]),
        SupervisionStage::new(
            "starting robot graph",
            StartupStepKind::Graph,
            graph,
            timeout,
        ),
    ]
}

/// Build the ordered resident startup stages for a Webots run.
pub fn stages_for_simulation(
    specs: Vec<ParticipantSpec>,
    timeout: crate::WaitBudget,
) -> Vec<SupervisionStage> {
    let mut infrastructure = Vec::new();
    let mut graph = Vec::new();
    for spec in specs {
        if spec.id == phoxal_cli_core::runtime::WEBOTS_PROCESS_ID
            || spec.kind == phoxal_cli_core::runtime::ParticipantKind::Tool
        {
            infrastructure.push(spec);
        } else {
            graph.push(spec);
        }
    }
    vec![
        SupervisionStage::new(
            "starting project infrastructure",
            StartupStepKind::Infrastructure,
            infrastructure,
            timeout,
        )
        .with_extra_ready_ids([ProcessKey::project("infrastructure-router")]),
        SupervisionStage::new(
            "starting robot graph",
            StartupStepKind::Graph,
            graph,
            timeout,
        ),
    ]
}

/// A startup barrier containing process specs and board ids that must become
/// ready before the next stage begins. Simulation clock is not a stage.
#[derive(Debug, Clone)]
pub struct SupervisionStage {
    /// Human-readable name for this stage, used in stalled-stage errors and
    /// process spawn diagnostics. The typed startup step stays separate.
    pub label: String,
    pub step: StartupStepKind,
    pub specs: Vec<ParticipantSpec>,
    /// Board ids that must be observed `Ready` before the next stage spawns.
    /// Defaults to every spawned spec's own id that is a bus participant
    /// (see [`Self::new`]); extend with [`Self::with_extra_ready_ids`] for a
    /// wait-only id that has no `ParticipantSpec` of its own.
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
        step: StartupStepKind,
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
            step,
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
    pub(crate) step: StartupStepKind,
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
    stage: SupervisionStage,
) -> Option<PendingStage> {
    let SupervisionStage {
        label,
        step,
        specs,
        ready_ids,
        failure_ids,
        optional_ids,
        timeout: stage_timeout,
    } = stage;
    board.step_active(step);
    if specs.is_empty() && ready_ids.is_empty() {
        board.step_done(step);
        return None;
    }
    spawn_participants_in_stage(running, board, &label, specs).await;
    if ready_ids.is_empty() {
        board.step_done(step);
        return None;
    }
    Some(PendingStage {
        label,
        step,
        ready_ids,
        failure_ids,
        optional_ids,
        deadline: stage_timeout.deadline_from(Instant::now()),
    })
}

pub(crate) async fn spawn_until_pending(
    running: &mut Vec<RunningParticipant>,
    board: &SupervisorState,
    stage_queue: &mut VecDeque<SupervisionStage>,
) -> Option<PendingStage> {
    while let Some(stage) = stage_queue.pop_front() {
        if let Some(pending) = spawn_stage(running, board, stage).await {
            return Some(pending);
        }
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_cli_core::runtime::{
        ParticipantKind, ReadinessPolicy, RuntimeFailurePolicy, StartupRequirement,
        WEBOTS_PROCESS_ID,
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

    #[test]
    fn run_assigns_tools_to_infrastructure_and_robot_processes_to_graph() {
        let stages = stages_for_run(
            vec![
                spec("tool", ParticipantKind::Tool),
                spec("service", ParticipantKind::Service),
            ],
            crate::WaitBudget::Unbounded,
        );
        assert_eq!(stages[0].step, StartupStepKind::Infrastructure);
        assert_eq!(stages[0].specs[0].id, "tool");
        assert_eq!(stages[1].step, StartupStepKind::Graph);
        assert_eq!(stages[1].specs[0].id, "service");
    }

    #[test]
    fn simulation_assigns_webots_to_infrastructure_and_robot_to_graph() {
        let stages = stages_for_simulation(
            vec![
                spec(WEBOTS_PROCESS_ID, ParticipantKind::Simulator),
                spec("service", ParticipantKind::Service),
            ],
            crate::WaitBudget::Unbounded,
        );
        assert_eq!(stages[0].step, StartupStepKind::Infrastructure);
        assert_eq!(stages[0].specs[0].id, WEBOTS_PROCESS_ID);
        assert_eq!(stages[1].step, StartupStepKind::Graph);
        assert_eq!(stages[1].specs[0].id, "service");
    }
}
