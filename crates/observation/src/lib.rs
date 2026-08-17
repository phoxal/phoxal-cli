//! Immutable, renderer-neutral observations produced by an attachment client.
//!
//! This crate deliberately contains no stores, tasks, channels, transports,
//! reconciliation, commands, or rendering.
//!
//! Every remote fact an observation carries comes in as a `phoxal_client`
//! type, so this crate names no wire crate directly.
//! `phoxal-runtime-contract` stays a direct dependency because identities and
//! participant kinds are shared domain primitives that also appear in
//! observations this crate composes locally.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::todo,
        clippy::unimplemented
    )
)]

pub mod epoch;
pub mod event;
pub mod input;
pub mod logs;
pub mod processes;
pub mod revision;
pub mod runtimes;
pub mod source_health;
pub mod supervisor;

pub use epoch::AttachmentEpoch;
pub use event::{AttachmentEvent, ConnectionObservation};
pub use input::{
    InputObservation, JoypadDevice, JoypadDeviceStatus, JoypadDevicesSample, MotionObservation,
    MotionSample,
};
pub use logs::{
    LogAnchor, LogFilters, LogQuery, LogRead, LogRow, LogSeverity, LogSource, LogWindow,
    WindowDirection, bounded_log_text, sanitize_terminal_text,
};
pub use processes::{
    LocalRuntime, LocalRuntimeState, LocalRuntimes, ProcessObservation, ProcessTable,
};
pub use revision::{ObservationQuery, ObservationWindow, QueryToken, StoreChanged, StoreRevision};
pub use runtimes::{
    RuntimeBufferKind, RuntimeDirection, RuntimeFeedStatus, RuntimePerformanceSample,
    RuntimePerformanceSummary, RuntimeQuery, RuntimeRead, RuntimeRow, RuntimeStepSample,
    RuntimeTopicSample, RuntimeWindow,
};
pub use source_health::{ObservationSource, SourceHealth, SourceStatus};
pub use supervisor::SupervisorObservation;
