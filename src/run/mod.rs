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
pub(crate) use router::{
    InfrastructureRouter, apply_session_connect, project_router_endpoint,
    start_infrastructure_router,
};
mod command;
pub(crate) use command::{AbortTasks, PreparedRun};
pub use command::{DriversMode, Run, RunOptions};
mod stages;
pub(crate) use stages::stages_for_run;
mod telemetry;
pub(crate) use telemetry::{RobotFeedTarget, start_telemetry_feeds_at};
mod prepare;
pub(crate) use prepare::prepare_run_on_board;
mod report;
pub(crate) use report::{DriverPolicy, report_launch_commands};
mod participants;
pub(crate) use participants::{
    DriverDecision, locate_tool_binary, prepare_robot_participants, spec_from_launch_record,
};
mod environment;
pub(crate) use environment::{
    env_path_override, native_pending_official_note, native_pending_tool_note,
};
mod build;
pub(crate) use build::{build_source_binary, device_missing_note};

#[cfg(test)]
mod tests;
