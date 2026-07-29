use std::time::Duration;

/// How long a `run` staged-startup stage may wait for its members to be
/// OBSERVED ready before the whole run fails naming the stalled stage - see
/// `stages_for_run` and `supervisor::SupervisionStage`. Every `run`
/// participant is CLI-managed and expected to clear its own `#[setup]`
/// quickly on a loaded host; generous enough to absorb ordinary scheduling
/// jitter without masking a genuinely hung participant.
const RUN_STAGE_READY_TIMEOUT: Duration = Duration::from_secs(60);
const ROUTER_READY_TIMEOUT: Duration = Duration::from_secs(15);

mod router;
pub(crate) use router::{InfrastructureRouter, start_infrastructure_router};
mod command;
pub(crate) use command::{
    AbortTasks, Readiness, connect_to_detached_resident, report_launch_commands,
    required_readiness, run_resident_supervision, run_webots_resident_supervision,
    wait_for_required_readiness,
};
pub use command::{DriversMode, Run, RunOptions};
mod stages;
pub(crate) use stages::stages_for_run;
mod telemetry;
pub(crate) use telemetry::{RobotFeedTarget, start_telemetry_feeds_at};
#[cfg(test)]
mod tests;
