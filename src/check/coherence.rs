//! Coherence responsibilities for check.

use super::run;
use super::{CheckCmd, CheckOptions, ensure_check_outcome_ok};
use crate::AppContext;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use phoxal::check as graph_check;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RobotCoherenceDiagnostic {
    pub robot_id: String,
    pub mismatches: Vec<CoherenceMismatchDiagnostic>,
}

pub(crate) struct RobotContractSurfaces {
    pub robot_id: String,
    pub surfaces: Vec<graph_check::ParticipantContractSurface>,
}

pub(crate) fn robot_contract_surfaces(
    robot_id: &str,
    surfaces: &[graph_check::ParticipantContractSurface],
) -> RobotContractSurfaces {
    RobotContractSurfaces {
        robot_id: robot_id.to_string(),
        surfaces: surfaces.to_vec(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CoherenceMismatchDiagnostic {
    PubSubDisjoint {
        participant_id: String,
        contract: String,
        subscribed: Vec<String>,
        published: Vec<String>,
        remedy: &'static str,
    },
    UnservedAsk {
        participant_id: String,
        contract: String,
        version: String,
        served: Vec<String>,
        remedy: &'static str,
    },
}

pub(super) const COHERENCE_REMEDY: &str = "align the version, mark a genuinely external consumer edge #[phoxal(external)], or update the lagging artifact";

impl CoherenceMismatchDiagnostic {
    fn from_mismatch(mismatch: &graph_check::CoherenceMismatch) -> Self {
        match mismatch {
            graph_check::CoherenceMismatch::PubSubDisjoint {
                participant_id,
                contract,
                subscribed,
                published,
            } => Self::PubSubDisjoint {
                participant_id: participant_id.clone(),
                contract: contract.clone(),
                subscribed: subscribed.iter().cloned().collect(),
                published: published.iter().cloned().collect(),
                remedy: COHERENCE_REMEDY,
            },
            graph_check::CoherenceMismatch::UnservedAsk {
                participant_id,
                contract,
                version,
                served,
            } => Self::UnservedAsk {
                participant_id: participant_id.clone(),
                contract: contract.clone(),
                version: version.clone(),
                served: served.iter().cloned().collect(),
                remedy: COHERENCE_REMEDY,
            },
        }
    }

    fn human_line(&self) -> String {
        match self {
            Self::PubSubDisjoint {
                participant_id,
                contract,
                subscribed,
                published,
                remedy,
            } => format!(
                "participant {participant_id} subscribes to {contract} at [{}], but the in-set publishers use [{}]; remedy: {remedy}",
                subscribed.join(", "),
                published.join(", ")
            ),
            Self::UnservedAsk {
                participant_id,
                contract,
                version,
                served,
                remedy,
            } => {
                let served = if served.is_empty() {
                    "none".to_string()
                } else {
                    served.join(", ")
                };
                format!(
                    "participant {participant_id} asks {contract} at {version}, but the in-set servers provide [{served}]; remedy: {remedy}"
                )
            }
        }
    }
}

pub(crate) fn evaluate_robot_coherence(
    robot_id: &str,
    surfaces: &[graph_check::ParticipantContractSurface],
) -> RobotCoherenceDiagnostic {
    let report = graph_check::check_coherence(surfaces);
    RobotCoherenceDiagnostic {
        robot_id: robot_id.to_string(),
        mismatches: report
            .mismatches
            .iter()
            .map(CoherenceMismatchDiagnostic::from_mismatch)
            .collect(),
    }
}

pub(crate) fn coherence_for_launch_plan(
    plan: &phoxal_cli_core::project::launch_plan::LaunchPlan,
    graphs: &[RobotContractSurfaces],
) -> Result<Vec<RobotCoherenceDiagnostic>> {
    plan.robots
        .iter()
        .map(|robot| {
            let graph = graphs
                .iter()
                .find(|graph| graph.robot_id == robot.id)
                .ok_or_else(|| anyhow!("robot {} has no checked contract graph", robot.id))?;
            let ids = robot
                .participants
                .iter()
                .map(|participant| participant.launch.participant_id.as_str())
                .collect::<std::collections::BTreeSet<_>>();
            let graph_surfaces = graph
                .surfaces
                .iter()
                .filter(|surface| ids.contains(surface.participant_id.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            Ok(evaluate_robot_coherence(&robot.id, &graph_surfaces))
        })
        .collect()
}

pub(super) fn coherence_is_ok(diagnostics: &[RobotCoherenceDiagnostic]) -> bool {
    diagnostics
        .iter()
        .all(|diagnostic| diagnostic.mismatches.is_empty())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CoherenceVerb {
    Check,
    Deploy,
    Run,
    Simulate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CoherenceDisposition {
    Pass,
    Warning,
    Failure,
}

pub(crate) fn coherence_disposition(
    verb: CoherenceVerb,
    strict: bool,
    diagnostics: &[RobotCoherenceDiagnostic],
) -> CoherenceDisposition {
    if coherence_is_ok(diagnostics) {
        CoherenceDisposition::Pass
    } else if verb == CoherenceVerb::Check && !strict {
        CoherenceDisposition::Warning
    } else {
        CoherenceDisposition::Failure
    }
}

pub(super) fn format_coherence_error(diagnostics: &[RobotCoherenceDiagnostic]) -> String {
    let mut message = String::from("participant contract coherence check failed:");
    for diagnostic in diagnostics
        .iter()
        .filter(|diagnostic| !diagnostic.mismatches.is_empty())
    {
        message.push_str("\n  robot ");
        message.push_str(&diagnostic.robot_id);
        message.push(':');
        for mismatch in &diagnostic.mismatches {
            message.push_str("\n    - ");
            message.push_str(&mismatch.human_line());
        }
    }
    message
}

pub(crate) fn enforce_coherence(
    verb: CoherenceVerb,
    diagnostics: &[RobotCoherenceDiagnostic],
) -> Result<()> {
    if coherence_disposition(verb, true, diagnostics) == CoherenceDisposition::Pass {
        return Ok(());
    }
    bail!("{}", format_coherence_error(diagnostics))
}

impl CheckCmd {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let project_root = app.project.root().to_path_buf();
        let options = CheckOptions {
            suite_source: app.suite_source.clone(),
            target: self.target.clone(),
            strict: self.strict,
        };
        let ui = app.ui;
        let result = tokio::task::spawn_blocking(move || run(&project_root, options, &ui))
            .await
            .context("check worker failed")??;

        eprintln!(
            "warning: v0 is pre-stable: artifacts built at different times may not interoperate"
        );

        ensure_check_outcome_ok(&result.train, &result.outcome)?;
        match coherence_disposition(CoherenceVerb::Check, result.strict, &result.coherence) {
            CoherenceDisposition::Pass => {}
            CoherenceDisposition::Warning => {
                eprintln!("warning: {}", format_coherence_error(&result.coherence));
            }
            CoherenceDisposition::Failure => {
                enforce_coherence(CoherenceVerb::Check, &result.coherence)?;
            }
        }
        println!(
            "ok: {} participants validated (framework train {})",
            result.participant_count, result.train
        );
        Ok(())
    }
}
