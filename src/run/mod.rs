use std::time::Duration;

/// How long a `run` staged-startup stage may wait for its members to be
/// OBSERVED ready before the whole run fails naming the stalled stage - see
/// `stages_for_run` and [`phoxal_cli_supervisor::SupervisionStage`]. Every `run`
/// participant is CLI-managed and expected to clear its own `#[setup]`
/// quickly on a loaded host; generous enough to absorb ordinary scheduling
/// jitter without masking a genuinely hung participant.
const RUN_STAGE_READY_TIMEOUT: Duration = Duration::from_secs(60);

mod command;
pub(crate) use command::{
    AbortTasks, Readiness, connect_to_detached_resident, connect_to_detached_resident_feed,
    report_launch_commands, required_readiness, run_resident_supervision,
    run_webots_resident_supervision, wait_for_required_readiness,
};
pub use command::{DriversMode, Run, RunOptions};
#[cfg(test)]
mod tests;
