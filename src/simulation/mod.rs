/// Bound on each non-interactive simulate participant-readiness stage. The
/// simulation clock is telemetry and never participates in this budget.
/// Generous enough to cover a first-run Webots GUI launch plus every
/// participant clearing its own `#[setup]` on a loaded host; a healthy
/// session reaches barrier success in a few seconds in practice.
const SIMULATE_READINESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

mod command;
pub(crate) use command::SimulateOptions;
pub use command::{
    Simulation, SimulationRun, SimulationSubcommand, SimulationWebots, WebotsSubcommand,
};
mod setup;
pub(crate) use setup::live_simulate_setup;
