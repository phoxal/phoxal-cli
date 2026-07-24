//! Report responsibilities for run.

use super::{DriverDecision, DriversMode, RunOptions};
use crate::supervisor::ParticipantSpec;
use anyhow::Result;
use anyhow::bail;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::launch_plan::ParticipantExecution;
use phoxal_cli_core::project::launch_plan::ParticipantLaunchRecord;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

#[derive(Debug)]
pub(crate) struct LaunchCommandReport {
    pub(crate) participants: Vec<LaunchCommandEntry>,
}

#[derive(Debug)]
pub(crate) struct LaunchCommandEntry {
    pub(crate) id: String,
    pub(crate) kind: &'static str,
    pub(crate) command_line: String,
}

/// The pre-staged-startup launch-report `kind` string. The board's
/// own `ParticipantKind` is the finer-grained shared
/// `Tool`/`Service`/`Driver`/`Simulator` split plus a `local` bit (Part 1) -
/// see `phoxal_cli_core::session::participant_kind`'s module docs. A robot
/// tool launch (the router, the
/// joypad, the Webots app in `simulate`) has no `ParticipantExecution` of
/// its own and is always `"robot-tool"`; everything else follows the
/// compact operator-facing mapping.
pub(crate) fn launch_kind_label(execution: Option<&ParticipantExecution>) -> &'static str {
    match execution {
        None => "robot-tool",
        Some(
            ParticipantExecution::OfficialArtifact { .. }
            | ParticipantExecution::OfficialTool { .. }
            | ParticipantExecution::SourceArtifact { .. },
        ) => "official",
        Some(ParticipantExecution::UserService { .. }) => "user-service",
        Some(ParticipantExecution::ComponentDriver { .. }) => "driver",
    }
}

pub(crate) fn report_launch_commands(
    plan: &LaunchPlan,
    specs: &[ParticipantSpec],
    ui: &crate::Ui,
) -> Result<()> {
    let executions_by_id = plan
        .robots
        .iter()
        .flat_map(|robot| &robot.participants)
        .map(|participant| {
            (
                participant.launch.participant_id.as_str(),
                &participant.execution,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let output = LaunchCommandReport {
        participants: specs
            .iter()
            .map(|spec| {
                let launch = spec.launch_command();
                LaunchCommandEntry {
                    id: spec.id.clone(),
                    kind: launch_kind_label(executions_by_id.get(spec.id.as_str()).copied()),
                    command_line: launch.command_line,
                }
            })
            .collect(),
    };
    report_launch_commands_human(&output, ui)
}

pub(crate) fn report_launch_commands_human(
    output: &LaunchCommandReport,
    ui: &crate::Ui,
) -> Result<()> {
    // A live session already owns stdout/stderr, so every human line must
    // enter its diagnostic stream instead of racing the TUI's alternate-
    // screen redraw. `Ui` retains the ordinary raw-mode fallback when this
    // helper is ever used outside a session.
    ui.info("resolved launch participants:");
    for participant in &output.participants {
        ui.info(format!(
            "  - {} ({}) -> {}",
            participant.id, participant.kind, participant.command_line
        ));
    }
    ui.info(
        "motion guarantees: e-stop, source freshness, finite values, and robot-authored limits; autonomous motion also requires fresh typed safety constraints",
    );
    Ok(())
}

#[derive(Debug)]
pub(crate) struct DriverPolicy {
    mode: DriversMode,
    subset: BTreeSet<String>,
}

impl DriverPolicy {
    pub(crate) fn drivers_off_for_sim() -> Self {
        Self {
            mode: DriversMode::Off,
            subset: BTreeSet::new(),
        }
    }

    pub(crate) fn from_options(options: &RunOptions, plan: &LaunchPlan) -> Result<Self> {
        let available = plan
            .robots
            .iter()
            .flat_map(|robot| &robot.participants)
            .filter(|participant| {
                matches!(
                    participant.execution,
                    ParticipantExecution::ComponentDriver { .. }
                )
            })
            .map(|participant| participant.launch.participant_id.clone())
            .collect::<BTreeSet<_>>();
        let subset = options
            .drivers_subset
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let unknown = subset.difference(&available).cloned().collect::<Vec<_>>();
        if !unknown.is_empty() {
            let available = available.into_iter().collect::<Vec<_>>().join(", ");
            bail!(
                "unknown driver id(s): {}; available drivers: {}",
                unknown.join(", "),
                if available.is_empty() {
                    "<none>".to_string()
                } else {
                    available
                }
            );
        }
        Ok(Self {
            mode: options.drivers,
            subset,
        })
    }

    pub(crate) fn decision(&self, id: &str) -> DriverDecision {
        match self.mode {
            DriversMode::Off => DriverDecision::Degraded("drivers off".to_string()),
            DriversMode::On if !self.subset.is_empty() && !self.subset.contains(id) => {
                DriverDecision::Degraded("not selected by --driver".to_string())
            }
            DriversMode::On => DriverDecision::Launch,
        }
    }

    pub(crate) fn launches(&self, participant: &ParticipantLaunchRecord) -> bool {
        !matches!(
            participant.execution,
            ParticipantExecution::ComponentDriver { .. }
        ) || self.decision(&participant.launch.participant_id) == DriverDecision::Launch
    }
}
