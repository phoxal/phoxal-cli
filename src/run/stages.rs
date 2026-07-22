//! Stages responsibilities for run.

use super::RUN_STAGE_READY_TIMEOUT;
use crate::supervisor::ParticipantSpec;
use crate::supervisor::SupervisionStage;
use phoxal_cli_core::session::ParticipantKind;

/// Build the two concrete ordered phases: project infrastructure, then the
/// checked robot graph. A DAG adds no value until a real partial-order use
/// case exists.
pub(crate) fn stages_for_run(
    specs: Vec<ParticipantSpec>,
    output: crate::session::output::OutputContext,
) -> Vec<SupervisionStage> {
    let mut tools = Vec::new();
    let mut graph = Vec::new();
    for spec in specs {
        match spec.kind {
            ParticipantKind::Tool => tools.push(spec),
            ParticipantKind::Driver | ParticipantKind::Service | ParticipantKind::Simulator => {
                graph.push(spec);
            }
        }
    }
    // Product decision 6: no unconditional 60s teardown for an interactive
    // session - see `OutputContext::wait_budget`.
    let timeout = output.wait_budget(RUN_STAGE_READY_TIMEOUT);
    vec![
        SupervisionStage::new("starting project infrastructure", tools, timeout)
            .with_extra_ready_ids([phoxal_cli_core::session::ProcessKey::project(
                "infrastructure-router",
            )]),
        SupervisionStage::new("starting robot graph", graph, timeout),
    ]
}
