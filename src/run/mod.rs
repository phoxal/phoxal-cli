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
    InfrastructureRouter, apply_session_connect, project_router_endpoint, resolve_router_config,
    start_infrastructure_router,
};
mod command;
pub(crate) use command::{
    AbortTasks, PreparedRun, Readiness, connect_to_detached_resident, required_readiness,
    run_resident_supervision, run_webots_resident_supervision, wait_for_required_readiness,
};
pub use command::{DriversMode, Run, RunOptions};
mod stages;
pub(crate) use stages::stages_for_run;
mod telemetry;
pub(crate) use telemetry::{RobotFeedTarget, start_telemetry_feeds_at};
mod prepare;
pub(crate) use prepare::{prepare_layout_run_on_board, prepare_run_on_board, refresh_staging};
mod report;
pub(crate) use report::{
    DriverPolicy, driven_instances, report_excluded_drivers, report_launch_commands,
    report_undeclared_runtimes,
};
mod participants;
pub(crate) use participants::{
    DriverDecision, build_layout_specs, prepare_robot_participants, source_cwd,
    source_dirs_by_participant, stage_complete_bin_store,
};
mod build;
pub(crate) use build::{
    StagingBuild, build_source_binary, device_missing_note, missing_device_path,
};

#[cfg(test)]
mod tests;
