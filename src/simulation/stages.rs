//! Ordinary resident supervision stages for the Webots profile.

use super::SIMULATE_READINESS_TIMEOUT;
use crate::session::output::OutputContext;
use crate::supervisor::ParticipantSpec;
use crate::supervisor::SupervisionStage;
use phoxal_cli_core::session::ParticipantKind;
use phoxal_cli_core::session::ProcessKey;
use phoxal_cli_core::session::WEBOTS_PROCESS_ID;

/// Build the same two ordered phases as a host run: all project
/// infrastructure (router, tools, Webots), then the ordinary robot graph.
pub(crate) fn stages_for_simulate(
    specs: Vec<ParticipantSpec>,
    output: OutputContext,
) -> Vec<SupervisionStage> {
    let mut infrastructure = Vec::new();
    let mut graph = Vec::new();
    for spec in specs {
        if spec.id == WEBOTS_PROCESS_ID || spec.kind == ParticipantKind::Tool {
            infrastructure.push(spec);
        } else {
            graph.push(spec);
        }
    }
    // Product decision 6: no unconditional 60s teardown for an interactive
    // session - see `OutputContext::wait_budget`.
    let timeout = output.wait_budget(SIMULATE_READINESS_TIMEOUT);
    vec![
        SupervisionStage::new("starting project infrastructure", infrastructure, timeout)
            .with_extra_ready_ids([ProcessKey::project("infrastructure-router")]),
        SupervisionStage::new("starting robot graph", graph, timeout),
    ]
}
