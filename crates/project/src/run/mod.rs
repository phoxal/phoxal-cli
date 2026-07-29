pub(crate) mod participants;
pub(crate) mod prepare;
pub(crate) mod report;

pub(crate) use participants::DriverDecision;
pub(crate) use report::DriverPolicy;

pub(crate) type DriversMode = crate::DriverMode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunOptions {
    pub(crate) drivers: DriversMode,
    pub(crate) drivers_subset: Vec<String>,
    pub(crate) offline: bool,
}

#[derive(Debug)]
pub(crate) struct PreparedRun {
    pub(crate) project_root: std::path::PathBuf,
    pub(crate) train: String,
    pub(crate) plan: phoxal_cli_core::project::launch_plan::LaunchPlan,
    pub(crate) participants: Vec<crate::PreparedParticipant>,
    pub(crate) staged_root: std::path::PathBuf,
    pub(crate) router_config: Option<std::path::PathBuf>,
}

#[cfg(test)]
mod tests;
