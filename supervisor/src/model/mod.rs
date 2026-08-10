//! Supervisor-owned process and lifecycle model.

pub mod launch;
pub mod lifecycle;
pub mod participant;
pub mod process;

pub use launch::{ParticipantSpec, RestartPolicy};
pub use lifecycle::{ProjectLifecycle, RuntimeFailurePolicy, StartupRequirement};
pub use participant::ParticipantKind;
pub use process::{
    BoundedString, ExitDescription, ProcessDescriptor, ProcessEntry, ProcessFailure,
    ProcessFailureKind, ProcessKey, ProcessState, ProcessStatus,
};

/// Maximum teardown failures retained in one bounded diagnostic report.
pub const MAX_TEARDOWN_FAILURES: usize = 40;

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
