//! Terminal-independent session state, events, data, and launch encoding.

pub mod board;
pub mod event;
pub mod human;
pub mod launch_env;
pub mod log;
pub mod mode;
pub mod participant_kind;
pub mod reconcile;
pub mod state;
pub mod stores;
pub mod supervisor;
pub mod telemetry;

pub use board::{BoardSnapshot, ParticipantLaunchCommand, ParticipantState, ParticipantStatus};
pub use log::{LogScope, LogSeverity, LogSource, RoutedLogLine, RoutedLogUpdate};
pub use mode::SessionMode;
pub use participant_kind::ParticipantKind;
pub use supervisor::{
    BoundedString, DesiredProcessState, ExitDescription, IncarnationId, ParticipantInstanceKey,
    ProcessDescriptor, ProcessEntry, ProcessFailure, ProcessFailureKind, ProcessKey, ProcessScope,
    ProcessState, ProjectLifecycle, ReadinessPolicy, RobotKey, RuntimeFailurePolicy,
    StartupRequirement, SupervisorSnapshotV0,
};
pub use telemetry::{
    ClockObservation, ClockSample, DeviceDiskSample, DeviceSample, JoypadCommand, JoypadDevice,
    JoypadDeviceStatus, JoypadDevicesSample, RouterMetricsSample, RuntimeBufferKind,
    RuntimeDirection, RuntimeFeedStatus, RuntimePerformanceSample, RuntimePerformanceSummary,
    RuntimeStepSample, RuntimeTopicSample, TelemetrySnapshot, TopicMetric,
};
