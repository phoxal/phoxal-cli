//! Finite command progress adapter.

use super::plain::Ui;
use phoxal_cli_core::runtime::{PhaseOutcome, StartupStepKind};
use phoxal_cli_supervisor::SupervisorState;

pub(crate) struct PreparationReporter {
    ui: Ui,
    cancellation: tokio_util::sync::CancellationToken,
}

impl PreparationReporter {
    pub(crate) fn new(ui: Ui, cancellation: tokio_util::sync::CancellationToken) -> Self {
        Self { ui, cancellation }
    }
}

pub(crate) fn cancellable_preparation_reporter(
    ui: Ui,
) -> (
    std::sync::Arc<dyn phoxal_cli_project::Reporter>,
    tokio::task::JoinHandle<()>,
) {
    let cancellation = tokio_util::sync::CancellationToken::new();
    let signal = cancellation.clone();
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal.cancel();
        }
    });
    (
        std::sync::Arc::new(PreparationReporter::new(ui, cancellation)),
        signal_task,
    )
}

impl phoxal_cli_project::Reporter for PreparationReporter {
    fn report(&self, event: phoxal_cli_project::PreparationEvent) {
        phoxal_cli_project::Reporter::report(&self.ui, event);
    }

    fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
}

/// Mirrors preparation progress onto the resident snapshot while preserving
/// the ordinary stderr/log presentation for every event.
pub(crate) struct BoardReporter {
    plain: PreparationReporter,
    board: SupervisorState,
}

impl BoardReporter {
    pub(crate) fn new(
        ui: Ui,
        cancellation: tokio_util::sync::CancellationToken,
        board: SupervisorState,
    ) -> Self {
        Self {
            plain: PreparationReporter::new(ui, cancellation),
            board,
        }
    }

    fn preparation_detail(id: &str, label: &str) -> Option<String> {
        match id {
            "validate" if label == "Opening staged layout" => {
                Some("opening staged layout".to_string())
            }
            "validate" => Some("validating robot.yaml".to_string()),
            "build" => Some("building workspace crates".to_string()),
            "check" => Some("checking source graph".to_string()),
            "publish" => Some("publishing runtime layout".to_string()),
            id if id.starts_with("materialize-") => Some(format!(
                "materializing {}",
                id.trim_start_matches("materialize-")
            )),
            _ => None,
        }
    }
}

impl phoxal_cli_project::Reporter for BoardReporter {
    fn report(&self, event: phoxal_cli_project::PreparationEvent) {
        match &event {
            phoxal_cli_project::PreparationEvent::PhaseStarted { id, label } => {
                let id = id.to_string();
                if id == "validate" {
                    let project_done =
                        self.board
                            .supervisor_snapshot()
                            .startup
                            .steps
                            .iter()
                            .any(|step| {
                                step.kind == StartupStepKind::Project
                                    && step.state
                                        == phoxal_cli_core::runtime::StartupStepState::Done
                            });
                    if !project_done {
                        self.board
                            .step_detail(StartupStepKind::Project, "resolving robot.yaml");
                    }
                }
                if let Some(detail) = Self::preparation_detail(&id, label) {
                    self.board
                        .step_detail(StartupStepKind::PrepareRuntime, detail);
                }
            }
            phoxal_cli_project::PreparationEvent::ProjectResolved { train } => {
                self.board.step_detail(
                    StartupStepKind::Project,
                    format!("robot.yaml · framework {train}"),
                );
                self.board.step_done(StartupStepKind::Project);
                self.board.step_active(StartupStepKind::PrepareRuntime);
            }
            phoxal_cli_project::PreparationEvent::SourceBuildPlanned { groups, artifacts } => {
                self.board.step_detail(
                    StartupStepKind::PrepareRuntime,
                    format!("Cargo: {groups} batch(es), {artifacts} selected artifacts"),
                );
            }
            phoxal_cli_project::PreparationEvent::SourceBuildGroupStarted {
                current,
                total,
                artifacts,
            } => {
                self.board.step_detail(
                    StartupStepKind::PrepareRuntime,
                    format!("Cargo batch {current}/{total}: 0/{artifacts} artifacts"),
                );
            }
            phoxal_cli_project::PreparationEvent::SourceBuildArtifactCompleted {
                completed,
                total,
            } => {
                self.board.step_detail(
                    StartupStepKind::PrepareRuntime,
                    format!("Cargo artifacts: {completed}/{total}"),
                );
            }
            phoxal_cli_project::PreparationEvent::SourceBuildGroupFinished {
                current,
                total,
                success,
            } => {
                self.board.step_detail(
                    StartupStepKind::PrepareRuntime,
                    format!(
                        "Cargo batch {current}/{total} {}",
                        if *success { "complete" } else { "failed" }
                    ),
                );
            }
            phoxal_cli_project::PreparationEvent::RegistryInstallGroupStarted {
                current,
                total,
                packages,
            } => {
                self.board.step_detail(
                    StartupStepKind::PrepareRuntime,
                    format!("Cargo install {current}/{total}: {packages} exact packages"),
                );
            }
            phoxal_cli_project::PreparationEvent::RegistryInstallGroupFinished {
                current,
                total,
                success,
            } => {
                self.board.step_detail(
                    StartupStepKind::PrepareRuntime,
                    format!(
                        "Cargo install {current}/{total}: {}",
                        if *success { "complete" } else { "failed" }
                    ),
                );
            }
            phoxal_cli_project::PreparationEvent::PhaseFinished {
                outcome: PhaseOutcome::Failed { error },
                ..
            } => {
                self.board.fail_active_step(error);
            }
            _ => {}
        }
        phoxal_cli_project::Reporter::report(&self.plain, event);
    }

    fn is_cancelled(&self) -> bool {
        phoxal_cli_project::Reporter::is_cancelled(&self.plain)
    }
}

#[cfg(test)]
mod tests {
    use super::BoardReporter;
    use phoxal_cli_core::runtime::{PhaseId, PhaseOutcome, StartupStepKind, StartupStepState};
    use phoxal_cli_project::{PreparationEvent, Reporter};
    use phoxal_cli_supervisor::SupervisorState;
    use std::time::Duration;

    fn prepare(
        snapshot: &phoxal_cli_protocol::SupervisorSnapshotV0,
    ) -> &phoxal_cli_core::runtime::StartupStep {
        snapshot
            .startup
            .steps
            .iter()
            .find(|step| step.kind == StartupStepKind::PrepareRuntime)
            .unwrap()
    }

    #[test]
    fn phase_ids_have_stable_operator_details() {
        assert_eq!(
            BoardReporter::preparation_detail("validate", "Validating robot.yaml").as_deref(),
            Some("validating robot.yaml")
        );
        assert_eq!(
            BoardReporter::preparation_detail("materialize-phoxal-runtime-contract", "ignored")
                .as_deref(),
            Some("materializing phoxal-runtime-contract")
        );
        assert_eq!(
            BoardReporter::preparation_detail("unknown", "ignored"),
            None
        );
    }

    #[test]
    fn reporter_maps_repeats_and_failures_without_reopening_done_prepare() {
        let board = SupervisorState::new();
        board.plan_startup_steps();
        let reporter = BoardReporter::new(
            super::Ui::new(false, false),
            tokio_util::sync::CancellationToken::new(),
            board.clone(),
        );
        reporter.report(PreparationEvent::PhaseStarted {
            id: PhaseId::new("validate"),
            label: "Validating robot.yaml".to_string(),
        });
        reporter.report(PreparationEvent::ProjectResolved {
            train: "0.45.2".to_string(),
        });
        for label in ["Building", "Building again"] {
            reporter.report(PreparationEvent::PhaseStarted {
                id: PhaseId::new("build"),
                label: label.to_string(),
            });
        }
        let snapshot = board.supervisor_snapshot();
        let project = snapshot
            .startup
            .steps
            .iter()
            .find(|step| step.kind == StartupStepKind::Project)
            .unwrap();
        assert_eq!(project.state, StartupStepState::Done);
        assert_eq!(
            project.detail.as_deref(),
            Some("robot.yaml · framework 0.45.2")
        );
        assert_eq!(prepare(&snapshot).state, StartupStepState::Active);
        assert_eq!(
            prepare(&snapshot).detail.as_deref(),
            Some("building workspace crates")
        );

        board.step_done(StartupStepKind::PrepareRuntime);
        reporter.report(PreparationEvent::PhaseStarted {
            id: PhaseId::new("validate"),
            label: "Opening staged layout".to_string(),
        });
        assert_eq!(
            prepare(&board.supervisor_snapshot()).state,
            StartupStepState::Done
        );

        board.step_active(StartupStepKind::Infrastructure);
        reporter.report(PreparationEvent::PhaseFinished {
            id: PhaseId::new("publish"),
            outcome: PhaseOutcome::Failed {
                error: "publish failed".to_string(),
            },
            elapsed: Duration::from_millis(1),
        });
        let snapshot = board.supervisor_snapshot();
        assert_eq!(
            snapshot
                .startup
                .steps
                .iter()
                .find(|step| step.kind == StartupStepKind::Infrastructure)
                .unwrap()
                .state,
            StartupStepState::Failed
        );
    }

    #[test]
    fn unsupported_train_finishes_project_then_fails_prepare_with_typed_train() {
        let board = SupervisorState::new();
        board.plan_startup_steps();
        let reporter = BoardReporter::new(
            super::Ui::new(false, false),
            tokio_util::sync::CancellationToken::new(),
            board.clone(),
        );
        reporter.report(PreparationEvent::PhaseStarted {
            id: PhaseId::new("validate"),
            label: "Validating robot.yaml".to_string(),
        });
        reporter.report(PreparationEvent::ProjectResolved {
            train: "0.41.2".to_string(),
        });
        reporter.report(PreparationEvent::PhaseFinished {
            id: PhaseId::new("validate"),
            outcome: PhaseOutcome::Failed {
                error: "framework 0.41.2 is too old\nfix: run `cargo update -p phoxal`".to_string(),
            },
            elapsed: Duration::from_millis(1),
        });
        let snapshot = board.supervisor_snapshot();
        let project = snapshot
            .startup
            .steps
            .iter()
            .find(|step| step.kind == StartupStepKind::Project)
            .unwrap();
        assert_eq!(project.state, StartupStepState::Done);
        assert_eq!(
            project.detail.as_deref(),
            Some("robot.yaml · framework 0.41.2")
        );
        assert_eq!(prepare(&snapshot).state, StartupStepState::Failed);
    }

    #[test]
    fn staged_layout_carries_opening_detail_into_active_prepare_step() {
        let board = SupervisorState::new();
        board.plan_startup_steps();
        let reporter = BoardReporter::new(
            super::Ui::new(false, false),
            tokio_util::sync::CancellationToken::new(),
            board.clone(),
        );
        reporter.report(PreparationEvent::PhaseStarted {
            id: PhaseId::new("validate"),
            label: "Opening staged layout".to_string(),
        });
        assert_eq!(
            prepare(&board.supervisor_snapshot()).detail.as_deref(),
            Some("opening staged layout")
        );
        reporter.report(PreparationEvent::ProjectResolved {
            train: "staged".to_string(),
        });
        let snapshot = board.supervisor_snapshot();
        assert_eq!(prepare(&snapshot).state, StartupStepState::Active);
        assert_eq!(
            prepare(&snapshot).detail.as_deref(),
            Some("opening staged layout")
        );
    }
}
