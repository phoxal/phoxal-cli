//! Immutable, renderer-neutral observations produced by an attachment client.
//!
//! This crate deliberately contains no stores, tasks, channels, transports,
//! reconciliation, commands, or rendering.
//!
//! Every remote fact an observation carries is named by its canonical
//! `phoxal` path - `phoxal::supervisor::api`, `phoxal::runtime::api`,
//! `phoxal::identity` - and is carried whole rather than re-aliased here. What
//! this crate owns is the composition: projections, derived summaries,
//! sanitization, windows, and the local facts a client knows about the
//! runtimes it launched itself.

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
    GraphSplit, LocalRuntime, LocalRuntimeState, LocalRuntimes, ProcessObservation, ProcessTable,
};
pub use revision::{ObservationQuery, ObservationWindow, QueryToken, StoreChanged, StoreRevision};
pub use runtimes::{
    RuntimeFeedStatus, RuntimePerformanceSample, RuntimePerformanceSummary, RuntimeQuery,
    RuntimeRead, RuntimeRow, RuntimeWindow,
};
pub use source_health::{ObservationSource, SourceHealth, SourceStatus};
pub use supervisor::SupervisorObservation;
