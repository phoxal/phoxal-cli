//! Pure runtime domain values shared by CLI owners.

pub mod launch;
pub mod lifecycle;
pub mod mode;
pub mod participant;
pub mod paths;
pub mod phase;
pub mod process;
mod target;

pub use launch::{
    EncodedParticipantEnv, ParticipantLaunchCommand, ParticipantSpec, RestartPolicy,
    encode_participant_env, encode_tool_env,
};
pub use lifecycle::{
    ParticipantState, ProjectLifecycle, ReadinessPolicy, RuntimeFailurePolicy,
    SimulationSessionInfo, StartupRequirement, StartupStatus,
};
pub use mode::SessionMode;
pub use participant::ParticipantKind;
pub use phase::{PhaseId, PhaseOutcome};
pub use process::{
    BoundedString, DesiredProcessState, ExitDescription, ParticipantInstanceKey, ProcessDescriptor,
    ProcessEntry, ProcessFailure, ProcessFailureKind, ProcessKey, ProcessScope, ProcessState,
    ProcessStatus, RobotKey,
};
pub use target::{ResidentAuthority, RuntimeTarget};

pub const PROJECT_ROOT_ENV: &str = "PHOXAL_PROJECT_ROOT";

/// Domain bounds shared by launch-plan construction and its wire projection.
///
/// These belong to the runtime model because plans must be rejected before a
/// supervisor publishes partial state. The protocol crate aliases them while
/// retaining ownership of encoded frame-size limits.
pub const MAX_SUPERVISED_PROCESSES: usize = 40;
pub const MAX_RUNTIME_ARTIFACT_ID_BYTES: usize = 1024;
pub const MAX_RUNTIME_TEXT_BYTES: usize = 4 * 1024;

/// Process identity of the Webots application managed by the resident.
pub const WEBOTS_PROCESS_ID: &str = "webots";

#[must_use]
pub fn format_duration(value: std::time::Duration) -> String {
    if value < std::time::Duration::from_secs(1) {
        return format!("{}ms", value.as_millis());
    }
    if value < std::time::Duration::from_secs(60) {
        return format!("{:.1}s", value.as_secs_f64());
    }
    let seconds = value.as_secs();
    if seconds < 60 * 60 {
        return format!("{}m {:02}s", seconds / 60, seconds % 60);
    }
    format!("{}h {:02}m", seconds / (60 * 60), (seconds / 60) % 60)
}

#[cfg(test)]
mod tests {
    use super::format_duration;
    use std::time::Duration;

    #[test]
    fn formats_elapsed_time_at_useful_precision() {
        assert_eq!(format_duration(Duration::from_millis(250)), "250ms");
        assert_eq!(format_duration(Duration::from_millis(1500)), "1.5s");
        assert_eq!(format_duration(Duration::from_secs(65)), "1m 05s");
        assert_eq!(format_duration(Duration::from_secs(3_720)), "1h 02m");
    }
}
