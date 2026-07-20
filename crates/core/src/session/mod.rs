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
pub mod telemetry;

pub use board::{BoardSnapshot, ParticipantLaunchCommand, ParticipantState, ParticipantStatus};
pub use log::{LogSeverity, LogSource, RoutedLogLine, RoutedLogUpdate};
pub use mode::SessionMode;
pub use participant_kind::ParticipantKind;
pub use telemetry::{
    ClockObservation, ClockSample, DiskSample, HostSample, JoypadCommand, JoypadDevice,
    JoypadDeviceStatus, JoypadDevicesSample, RouterMetricsSample, TelemetrySnapshot, TopicMetric,
};
