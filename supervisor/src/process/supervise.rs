//! Main supervision loop, actions, and orderly shutdown.

use super::output::join_reader;
use super::participant::RunningParticipant;
use super::signals::stop_child;
use super::spec::{SupervisorAction, SupervisorOptions};
use super::stages::{
    StageReporter, SupervisionStage, WaitBudget, await_stage_ready, maybe_publish_startup_outcome,
    spawn_until_pending,
};
use crate::model::lifecycle::ProjectLifecycle;
use crate::model::process::{BoundedString, ProcessKey};
use crate::state::store::SupervisorState;
use anyhow::Result;
use std::collections::{HashMap, VecDeque};
use std::time::Duration;
use std::time::Instant;
use tokio::time::MissedTickBehavior;

const MAX_TEARDOWN_FAILURES: usize = 40;

pub(crate) async fn supervise_until_shutdown(
    stages: Vec<SupervisionStage>,
    board: SupervisorState,
    progress: StageReporter,
    mut options: SupervisorOptions,
) -> Result<()> {
    let failed_processes = board
        .snapshot()
        .processes
        .into_iter()
        .filter(|(_, entry)| entry.status.actual == crate::model::process::ProcessState::Failed)
        .map(|(key, _)| key.to_string())
        .collect::<Vec<_>>();
    if !failed_processes.is_empty() {
        let reason = format!(
            "process(es) failed before startup: {}",
            failed_processes.join(", ")
        );
        board.fail(&reason);
        progress.failed(&reason);
        options.token.cancel();
        anyhow::bail!(reason);
    }
    let mut running = Vec::new();
    let mut stage_queue: VecDeque<SupervisionStage> = stages.into();
    let token = options.token.clone();

    // Skip empty stages and wait on the first stage with participant readiness
    // work, so an empty stage never leaves the select loop without a branch.
    let mut pending_stage =
        spawn_until_pending(&mut running, &board, &progress, &mut stage_queue).await;
    maybe_publish_startup_outcome(&board, &options, &pending_stage).await;

    let mut ticker = tokio::time::interval(Duration::from_millis(500));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut action_rx = options.action_rx.take();
    let mut supervisor_error = None;
    'supervision: loop {
        tokio::select! {
            () = token.cancelled() => {
                break;
            }
            action = recv_action(action_rx.as_mut()) => {
                if let Some(action) = action
                    && let Err(error) = handle_action(&mut running, &board, action).await
                {
                    tracing::error!(error = %error, "supervisor action failed; shutting down graph");
                    supervisor_error = Some(error);
                    break 'supervision;
                }
            }
            // The deadline lives in `pending_stage`, so recreating this
            // cancellation-safe wait does not reset a stage timeout.
            result = await_stage_ready(
                &board,
                pending_stage.as_ref().map_or(&[][..], |stage| stage.ready_ids.as_slice()),
                pending_stage.as_ref().map_or(WaitBudget::Unbounded, |stage| match stage.deadline {
                    Some(deadline) => WaitBudget::Bounded(deadline.saturating_duration_since(Instant::now())),
                    None => WaitBudget::Unbounded,
                }),
                Duration::from_millis(200),
            ), if pending_stage.is_some() => {
                let Some(stage) = pending_stage.take() else {
                    continue;
                };
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
                        && board.snapshot().lifecycle == ProjectLifecycle::Ready
                    {
                        let reason = format!(
                            "process {} exhausted its restart policy",
                            participant.spec.key
                        );
                        board.fail(&reason);
                        supervisor_error = Some(anyhow::anyhow!(reason));
                        token.cancel();
                        break 'supervision;
                    }
                }
            }
        }
    }

    if supervisor_error.is_none() {
        board.set_lifecycle(ProjectLifecycle::Stopping);
    }
    let teardown = shutdown_all(&mut running, &board).await;
    if let Some(error) = supervisor_error {
        if teardown.is_clean() {
            return Err(error);
        }
        return Err(error.context(teardown.summary()));
    }
    teardown.into_result()?;
    board.set_lifecycle(ProjectLifecycle::Stopped);
    Ok(())
}

pub(crate) async fn recv_action(
    action_rx: Option<&mut tokio::sync::mpsc::Receiver<SupervisorAction>>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum TeardownFailureKind {
    ChildStop,
    WorkerJoin,
}

impl TeardownFailureKind {
    const fn label(self) -> &'static str {
        match self {
            Self::ChildStop => "child stop",
            Self::WorkerJoin => "worker join",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TeardownFailure {
    participant: ProcessKey,
    kind: TeardownFailureKind,
    detail: BoundedString,
}

impl TeardownFailure {
    fn new(participant: ProcessKey, kind: TeardownFailureKind, detail: impl AsRef<str>) -> Self {
        Self {
            participant,
            kind,
            detail: BoundedString::new(detail),
        }
    }

    fn render(&self) -> String {
        format!(
            "{} {}: {}",
            self.participant,
            self.kind.label(),
            self.detail.as_str()
        )
    }
}

#[derive(Debug, Default)]
pub(crate) struct TeardownReport {
    failures: Vec<TeardownFailure>,
}

impl TeardownReport {
    fn push(&mut self, failure: TeardownFailure) {
        if self.failures.len() < MAX_TEARDOWN_FAILURES {
            self.failures.push(failure);
        }
    }

    fn is_clean(&self) -> bool {
        self.failures.is_empty()
    }

    fn summary(&self) -> String {
        let mut failures = self.failures.clone();
        failures.sort_by(|left, right| {
            left.participant
                .cmp(&right.participant)
                .then_with(|| left.kind.cmp(&right.kind))
                .then_with(|| left.detail.as_str().cmp(right.detail.as_str()))
        });
        format!(
            "supervisor teardown failed: {}",
            failures
                .iter()
                .map(TeardownFailure::render)
                .collect::<Vec<_>>()
                .join("; ")
        )
    }

    fn into_result(self) -> Result<()> {
        if self.is_clean() {
            Ok(())
        } else {
            anyhow::bail!(self.summary())
        }
    }
}

fn record_worker_result(
    report: &mut TeardownReport,
    workers: &mut HashMap<tokio::task::Id, ProcessKey>,
    result: std::result::Result<
        (tokio::task::Id, std::result::Result<(), TeardownFailure>),
        tokio::task::JoinError,
    >,
) {
    match result {
        Ok((task_id, result)) => {
            let Some(participant) = workers.remove(&task_id) else {
                tracing::error!(?task_id, "completed teardown worker was not registered");
                return;
            };
            if let Err(failure) = result {
                debug_assert_eq!(failure.participant, participant);
                report.push(failure);
            }
        }
        Err(error) => {
            let task_id = error.id();
            let Some(participant) = workers.remove(&task_id) else {
                tracing::error!(?task_id, "failed teardown worker was not registered");
                return;
            };
            let detail = if error.is_panic() {
                format!("teardown worker panicked: {error}")
            } else if error.is_cancelled() {
                format!("teardown worker was cancelled: {error}")
            } else {
                format!("teardown worker failed: {error}")
            };
            report.push(TeardownFailure::new(
                participant,
                TeardownFailureKind::WorkerJoin,
                detail,
            ));
        }
    }
}

pub(crate) async fn shutdown_all(
    running: &mut [RunningParticipant],
    board: &SupervisorState,
) -> TeardownReport {
    let mut report = TeardownReport::default();
    // Reverse exact startup phase order, concurrency within each phase.
    let mut stages = running
        .iter()
        .map(|participant| participant.shutdown_stage)
        .collect::<Vec<_>>();
    stages.sort();
    stages.dedup();
    for stage in stages.into_iter().rev() {
        let mut joins = tokio::task::JoinSet::new();
        let mut workers = HashMap::new();
        for participant in running
            .iter_mut()
            .filter(|participant| participant.shutdown_stage == stage)
        {
            let mut child = participant.child.take();
            let stdout = participant.stdout_task.take();
            let stderr = participant.stderr_task.take();
            let spec = participant.spec.clone();
            let board = board.clone();
            let key = spec.key.clone();
            let task = joins.spawn(async move {
                if let Some(child) = child.as_mut() {
                    tracing::debug!(process = %spec.key, "stopping supervised process");
                    let failure = if let Err(error) = stop_child(child, spec.shutdown_grace).await {
                        board.record_failure(
                            &spec.key,
                            crate::model::process::ProcessFailureKind::Cleanup,
                            None,
                            format!("failed to stop: {error:#}"),
                        );
                        Some(TeardownFailure::new(
                            spec.key.clone(),
                            TeardownFailureKind::ChildStop,
                            format!("{error:#}"),
                        ))
                    } else {
                        None
                    };
                    board.set_pid(&spec.key, None);
                    join_reader(stdout).await;
                    join_reader(stderr).await;
                    return failure.map_or(Ok(()), Err);
                }
                join_reader(stdout).await;
                join_reader(stderr).await;
                Ok(())
            });
            workers.insert(task.id(), key);
        }
        while let Some(result) = joins.join_next_with_id().await {
            record_worker_result(&mut report, &mut workers, result);
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use crate::model::lifecycle::ProjectLifecycle;
    use crate::model::process::ProcessKey;
    use phoxal_runtime_contract::identity::ParticipantId;

    use super::*;

    fn key(id: &str) -> ProcessKey {
        ParticipantId::new(id).expect("fixture participant").into()
    }

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
        assert_eq!(state.snapshot().lifecycle, ProjectLifecycle::Stopped);
    }

    #[test]
    fn clean_teardown_report_succeeds() {
        let report = TeardownReport::default();
        assert!(report.is_clean());
        report.into_result().expect("a clean report succeeds");
    }

    #[test]
    fn stop_failure_keeps_its_typed_participant_association() {
        let participant = key("drive");
        let mut report = TeardownReport::default();
        report.push(TeardownFailure::new(
            participant.clone(),
            TeardownFailureKind::ChildStop,
            "termination timed out",
        ));

        assert_eq!(report.failures.len(), 1);
        let failure = &report.failures[0];
        assert_eq!(failure.participant, participant);
        assert_eq!(failure.kind, TeardownFailureKind::ChildStop);
        assert_eq!(failure.detail.as_str(), "termination timed out");
    }

    #[tokio::test]
    async fn worker_join_failure_keeps_its_participant_association() {
        let participant = key("vision");
        let mut joins = tokio::task::JoinSet::new();
        let task = joins.spawn(std::future::pending::<Result<(), TeardownFailure>>());
        let mut workers = HashMap::from([(task.id(), participant.clone())]);
        task.abort();
        let result = joins
            .join_next_with_id()
            .await
            .expect("the worker completed");
        let mut report = TeardownReport::default();
        record_worker_result(&mut report, &mut workers, result);

        assert!(workers.is_empty());
        assert_eq!(report.failures.len(), 1);
        let failure = &report.failures[0];
        assert_eq!(failure.participant, participant);
        assert_eq!(failure.kind, TeardownFailureKind::WorkerJoin);
        assert!(
            failure
                .detail
                .as_str()
                .starts_with("teardown worker was cancelled:"),
            "{}",
            failure.detail.as_str()
        );
    }

    #[test]
    fn teardown_failure_count_is_bounded() {
        let mut report = TeardownReport::default();
        for number in 0..=MAX_TEARDOWN_FAILURES {
            report.push(TeardownFailure::new(
                key(&format!("worker{number}")),
                TeardownFailureKind::ChildStop,
                "stop failed",
            ));
        }

        assert_eq!(report.failures.len(), MAX_TEARDOWN_FAILURES);
    }

    #[test]
    fn teardown_failure_detail_is_bounded() {
        let mut report = TeardownReport::default();
        report.push(TeardownFailure::new(
            key("drive"),
            TeardownFailureKind::ChildStop,
            "x".repeat(BoundedString::FAILURE_MAX_BYTES + 1),
        ));

        assert!(report.failures[0].detail.as_str().len() <= BoundedString::FAILURE_MAX_BYTES);
        assert!(report.failures[0].detail.as_str().ends_with('…'));
    }

    #[test]
    fn teardown_summary_is_deterministic() {
        let mut report = TeardownReport::default();
        report.push(TeardownFailure::new(
            key("zeta"),
            TeardownFailureKind::WorkerJoin,
            "z failure",
        ));
        report.push(TeardownFailure::new(
            key("alpha"),
            TeardownFailureKind::ChildStop,
            "a failure",
        ));

        assert_eq!(
            report.summary(),
            "supervisor teardown failed: alpha child stop: a failure; zeta worker join: z failure"
        );
    }
}
