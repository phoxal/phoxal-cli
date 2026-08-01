pub(crate) mod participants;
pub(crate) mod prepare;
pub(crate) mod resolve;
mod use_case;
pub(crate) mod webots;

pub(crate) use use_case::prepare_simulation;

pub(crate) use participants::{
    ensure_exactly_one_simulator, official_simulator_participants, remap_simulator_participant_ids,
    sim_checked_participants,
};
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SimulateOptions {
    pub(crate) world: String,
    pub(crate) offline: bool,
}

#[derive(Debug)]
pub(crate) struct ResolvedSimulation {
    pub(crate) project_root: std::path::PathBuf,
    pub(crate) world_path: std::path::PathBuf,
    pub(crate) resolved: phoxal_cli_core::project::resolver::BundlePlan,
}
