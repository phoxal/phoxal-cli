//! Stages responsibilities for run.

use super::RUN_STAGE_READY_TIMEOUT;
use crate::supervisor::ParticipantSpec;
use crate::supervisor::SupervisionStage;
use phoxal_cli_core::session::ParticipantKind;

/// Partition run participants into their dependency-ordered startup stages.
pub(crate) fn stages_for_run(
    specs: Vec<ParticipantSpec>,
    output: crate::session::output::OutputContext,
) -> Vec<SupervisionStage> {
    let mut tools = Vec::new();
    let mut drivers = Vec::new();
    let mut services = Vec::new();
    for spec in specs {
        match spec.kind {
            ParticipantKind::Tool => tools.push(spec),
            ParticipantKind::Driver => drivers.push(spec),
            ParticipantKind::Service | ParticipantKind::Simulator => services.push(spec),
        }
    }
    // Product decision 6: no unconditional 60s teardown for an interactive
    // session - see `OutputContext::wait_budget`.
    let timeout = output.wait_budget(RUN_STAGE_READY_TIMEOUT);
    vec![
        SupervisionStage::new("starting tools", tools, timeout),
        SupervisionStage::new("starting drivers", drivers, timeout),
        SupervisionStage::new("starting services", services, timeout),
    ]
}
