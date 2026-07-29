//! Terminal-independent session state, events, data, and launch encoding.

pub mod board;
pub mod event;
pub mod human;
pub mod launch_env;
pub mod log;
pub mod mode;
pub mod participant_kind;
pub mod state;
pub mod supervisor;
pub mod telemetry;

pub use board::{BoardSnapshot, ParticipantLaunchCommand, ParticipantState, ParticipantStatus};
pub use log::{
    LogScope, LogSeverity, LogSource, MAX_ROUTED_LOG_TEXT_CHARS, RoutedLogLine, RoutedLogUpdate,
    bounded_log_text, sanitize_terminal_text,
};
pub use mode::SessionMode;
pub use participant_kind::ParticipantKind;
pub use supervisor::{
    BoundedString, DesiredProcessState, ExitDescription, ParticipantInstanceKey, ProcessDescriptor,
    ProcessEntry, ProcessFailure, ProcessFailureKind, ProcessKey, ProcessScope, ProcessState,
    ProcessStatus, ProjectLifecycle, ReadinessPolicy, RobotKey, RuntimeFailurePolicy,
    SimulationSessionInfo, StartupRequirement, StartupStatus,
};
pub use telemetry::{
    ClockObservation, ClockSample, DEFAULT_FRESHNESS_TTL, DeviceDiskSample, DeviceSample,
    JoypadCommand, JoypadDevice, JoypadDeviceStatus, JoypadDevicesSample, MotionSample, RobotScope,
    RouterMetricsSample, RuntimeBufferKind, RuntimeDirection, RuntimeFeedStatus,
    RuntimePerformanceSample, RuntimePerformanceSummary, RuntimeStepSample, RuntimeTopicSample,
    TelemetrySnapshot, Timestamped, TopicMetric,
};

/// Board/process identity of the Webots application managed by the resident.
pub const WEBOTS_PROCESS_ID: &str = "webots";
