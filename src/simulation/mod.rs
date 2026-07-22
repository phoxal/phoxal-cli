#[cfg(test)]
use phoxal::bus::ContractBody;
#[cfg(test)]
use phoxal_api::v0_1::simulation::{SpawnRequest, SpawnSet};

/// Bound on each non-interactive simulate participant-readiness stage. The
/// simulation clock is telemetry and never participates in this budget.
/// Generous enough to cover a first-run Webots GUI launch plus every
/// participant clearing its own `#[setup]` on a loaded host; a healthy
/// session reaches barrier success in a few seconds in practice.
const SIMULATE_READINESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

mod command;
pub(crate) use command::ResolvedSimulation;
pub use command::{
    SimPlan, SimulateMode, SimulateOptions, Simulation, SimulationJoin, SimulationRun,
    SimulationSubcommand, run,
};
mod setup;
pub(crate) use setup::{LiveSimSetup, live_simulate_setup};
mod prepare;
pub use prepare::prepare;
pub(crate) use prepare::prepare_with_mode;
mod resolve;
pub(crate) use resolve::{build_checked_sim_launch_plan, resolve_project};
mod participants;
pub(crate) use participants::{
    driver_metadata_unavailable, official_simulator_participants, remap_simulator_participant_ids,
    remap_simulator_surface_ids, sim_checked_participants, sim_source_participants,
    simulated_component_records, simulator_participant_id_for_resolved_artifact,
};
mod report;
pub(crate) use report::{report_plan_only, simulation_managed_participant_ids, webots_world};
mod stages;
pub(crate) use stages::{
    WEBOTS_SITE_ID, native_tool_labels_from_plan, prepare_substitution_notes, stages_for_simulate,
    substitution_lines,
};
mod webots;
pub(crate) use webots::{stage_and_prepare_webots_spec, start_spawn_responder};
mod controllers;
pub(crate) use controllers::{
    require_absolute_symlink_target, stage_simulator_controller_binaries,
};
mod staging;
pub(crate) use staging::stage_simulation_for_robot;

#[cfg(test)]
mod tests;
