//! Immutable, renderer-neutral observations produced by an attachment client.
//!
//! This crate deliberately contains no stores, tasks, channels, transports,
//! reconciliation, commands, or rendering.

pub mod epoch;
pub mod event;
pub mod input;
pub mod logs;
pub mod processes;
pub mod revision;
pub mod robot;
pub mod runtimes;
pub mod source_health;
pub mod supervisor;

pub use epoch::AttachmentEpoch;
pub use event::{
    AttachmentEvent, ConnectionObservation, DiagnosticLevel, DiagnosticSource, Freshness,
    FreshnessSet, PhaseId, PhaseOutcome, RuntimeEvent,
};
pub use input::{
    InputObservation, JoypadDevice, JoypadDeviceStatus, JoypadDevicesSample, MotionObservation,
    MotionSample,
};
pub use logs::{
    LogAnchor, LogFilters, LogQuery, LogRead, LogRow, LogScope, LogSeverity, LogSource, LogWindow,
    WindowDirection, bounded_log_text, sanitize_terminal_text,
};
pub use processes::{ProcessObservation, ProcessTable};
pub use revision::{ObservationQuery, ObservationWindow, QueryToken, StoreChanged, StoreRevision};
pub use robot::RobotScope;
pub use runtimes::{
    RuntimeBufferKind, RuntimeDirection, RuntimeFeedStatus, RuntimePerformanceSample,
    RuntimePerformanceSummary, RuntimeQuery, RuntimeRead, RuntimeRow, RuntimeStepSample,
    RuntimeTopicSample, RuntimeWindow,
};
pub use source_health::{SourceHealth, SourceStatus};
pub use supervisor::SupervisorObservation;
