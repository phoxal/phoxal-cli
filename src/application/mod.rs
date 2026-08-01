use std::time::Duration;

pub(crate) mod attachment;
pub(crate) mod readiness;
pub(crate) mod run;
pub(crate) mod schema;
pub(crate) mod simulation;

pub(crate) use simulation::live_simulate_setup;

const RUN_STAGE_READY_TIMEOUT: Duration = Duration::from_secs(60);
const SIMULATE_READINESS_TIMEOUT: Duration = Duration::from_secs(60);
