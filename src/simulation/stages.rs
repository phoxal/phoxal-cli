//! Supervision stages and substitution diagnostics for simulation.

use super::{SIMULATE_READINESS_TIMEOUT, simulation_managed_participant_ids};
use crate::session::output::OutputContext;
use crate::supervisor::BoardBackend;
use crate::supervisor::ParticipantSpec;
use crate::supervisor::ParticipantState;
use crate::supervisor::ParticipantStatus;
use crate::supervisor::SupervisionStage;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::launch_plan::SIMULATOR_SUPERVISOR_PROVIDER_ID;
use phoxal_cli_core::project::launch_plan::SubstitutionRecord;
use phoxal_cli_core::session::ParticipantKind;

/// Partition process specs and simulation-managed participants into ordered
/// readiness stages. Clock samples remain independent telemetry.
pub(crate) fn stages_for_simulate(
    specs: Vec<ParticipantSpec>,
    plan: &LaunchPlan,
    output: OutputContext,
) -> Vec<SupervisionStage> {
    let mut tools = Vec::new();
    let mut webots = Vec::new();
    let mut services = Vec::new();
    for spec in specs {
        if spec.id == WEBOTS_SITE_ID {
            webots.push(spec);
        } else if spec.kind == ParticipantKind::Tool {
            tools.push(spec);
        } else {
            services.push(spec);
        }
    }
    let (supervisor_ids, controller_ids): (Vec<String>, Vec<String>) =
        simulation_managed_participant_ids(plan)
            .into_iter()
            .partition(|id| id == SIMULATOR_SUPERVISOR_PROVIDER_ID);
    // Product decision 6: no unconditional 60s teardown for an interactive
    // session - see `OutputContext::wait_budget`.
    let timeout = output.wait_budget(SIMULATE_READINESS_TIMEOUT);
    vec![
        SupervisionStage::new("starting tools", tools, timeout),
        SupervisionStage::new("starting Webots", webots, timeout),
        SupervisionStage::new("waiting for the simulation supervisor", Vec::new(), timeout)
            .with_extra_ready_ids(supervisor_ids),
        SupervisionStage::new("starting services", services, timeout),
        SupervisionStage::new("waiting for robot controllers", Vec::new(), timeout)
            .with_extra_ready_ids(controller_ids),
    ]
}

pub(crate) fn prepare_substitution_notes(plan: &LaunchPlan, board: &BoardBackend) {
    for robot in &plan.robots {
        let scope = phoxal_cli_core::session::stores::telemetry::RobotScope {
            namespace: robot.namespace.clone(),
            robot_id: robot.id.clone(),
        };
        for substitution in &robot.substitutions {
            let mut status = ParticipantStatus::new(
                &substitution.component_instance,
                ParticipantKind::Driver,
                ParticipantState::Ready,
            )
            .with_scope(scope.clone());
            status.note = Some(substitution_note(substitution));
            board.upsert(status);
        }
    }
}

pub(crate) fn substitution_lines(plan: &LaunchPlan) -> Vec<String> {
    plan.robots
        .iter()
        .flat_map(|robot| robot.substitutions.iter().map(render_substitution))
        .collect()
}

pub(crate) fn render_substitution(substitution: &SubstitutionRecord) -> String {
    format!(
        "{} : satisfied by {} ({})",
        substitution_topic_summary(substitution),
        substitution.provider_participant_id,
        substitution.provider_artifact_id
    )
}

pub(crate) fn substitution_note(substitution: &SubstitutionRecord) -> String {
    format!(
        "simulated by {} ({})",
        substitution.provider_participant_id, substitution.provider_artifact_id
    )
}

/// A driver's own contract report now carries only `family` per contract (no
/// `schema_id`/`topic`, D1), so there is no per-topic wire detail left to
/// summarize here - this collapses to the component instance's wildcard,
/// same as the old "every contract is under this component" shortcut.
pub(crate) fn substitution_topic_summary(substitution: &SubstitutionRecord) -> String {
    format!("component/{}/*", substitution.component_instance)
}

/// Site tool labels are derived straight from the resolved `LaunchPlan` (Part
/// 3/6): `tool-joypad` is the standard, hard-required site tool in every mode
/// including Webots (product decision 9),
/// so they always appear here alongside the Webots app itself. This function
/// never needs `options` - it replaces the old `SimulatePlan::native_tools`
/// stored field.
pub(crate) fn native_tool_labels_from_plan(plan: &LaunchPlan) -> Vec<String> {
    let mut labels = plan
        .site
        .iter()
        .map(|site| site.id.clone())
        .collect::<Vec<_>>();
    labels.push(WEBOTS_SITE_ID.to_string());
    labels
}

/// The board id + `ParticipantKind` the CLI registers the Webots app under.
/// Webots is the CLI's only simulator-side child (Part 5): the CLI launches
/// it pointed at the staged world; everything downstream (the supervisor,
/// each robot's controller) is Webots-spawned and SIMULATION-MANAGED instead.
pub(crate) const WEBOTS_SITE_ID: &str = "webots";
