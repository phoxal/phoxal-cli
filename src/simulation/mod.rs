/// Bound on each non-interactive simulate participant-readiness stage. The
/// simulation clock is telemetry and never participates in this budget.
/// Generous enough to cover a first-run Webots GUI launch plus every
/// participant clearing its own `#[setup]` on a loaded host; a healthy
/// session reaches barrier success in a few seconds in practice.
const SIMULATE_READINESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

mod command;
pub(crate) use command::ResolvedSimulation;
pub(crate) use command::{SimPlan, SimulateOptions};
pub use command::{
    Simulation, SimulationRun, SimulationSubcommand, SimulationWebots, WebotsSubcommand,
};
mod setup;
pub(crate) use setup::live_simulate_setup;
mod prepare;
pub(crate) use prepare::prepare;
mod resolve;
pub(crate) use resolve::{build_checked_sim_launch_plan, resolve_project};
mod participants;
pub(crate) use participants::{
    driver_metadata_unavailable, official_simulator_participants, remap_simulator_participant_ids,
    sim_checked_participants, sim_source_participants,
};
mod stages;
pub(crate) use stages::stages_for_simulate;
mod webots;
pub(crate) use webots::stage_and_prepare_webots_spec;
mod controllers;
pub(crate) use controllers::stage_simulator_controller_binaries;
mod staging;
pub(crate) use staging::stage_simulation_for_robot;

pub(crate) fn webots_world(
    mode: &phoxal_cli_core::project::launch_plan::LaunchMode,
) -> &std::path::Path {
    match mode {
        phoxal_cli_core::project::launch_plan::LaunchMode::Webots { world } => world,
        phoxal_cli_core::project::launch_plan::LaunchMode::Run => {
            unreachable!("Webots preparation always builds a Webots launch plan")
        }
    }
}
