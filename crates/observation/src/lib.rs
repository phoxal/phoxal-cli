//! Immutable, renderer-neutral observations produced by an attachment client.
//!
//! This crate deliberately contains no stores, tasks, channels, transports,
//! reconciliation, commands, or rendering.

pub mod bus;
pub mod device;
pub mod epoch;
pub mod event;
pub mod input;
pub mod logs;
pub mod processes;
pub mod revision;
pub mod runtimes;
pub mod source_health;
pub mod supervisor;

pub use bus::{BusQuery, BusRead, BusRow, BusWindow};
pub use device::DeviceObservation;
pub use epoch::AttachmentEpoch;
pub use event::{AttachmentEvent, Freshness, FreshnessSet};
pub use input::{InputObservation, MotionObservation};
pub use logs::{LogAnchor, LogFilters, LogQuery, LogRead, LogRow, LogWindow, WindowDirection};
pub use processes::{ProcessObservation, ProcessTable};
pub use revision::{ObservationQuery, ObservationWindow, QueryToken, StoreChanged, StoreRevision};
pub use runtimes::{RuntimeQuery, RuntimeRead, RuntimeRow, RuntimeWindow};
pub use source_health::{SourceHealth, SourceStatus};
pub use supervisor::SupervisorObservation;
