//! Disposable attachment client for a resident Phoxal runtime.

mod attachment;
pub mod finite;
mod ports;
pub mod reconcile;
mod sources;
mod state;
pub mod supervisor;

pub use attachment::{
    Attachment, AttachmentPorts, AttachmentRuntime, attach_with_supervisor,
    validate_requested_entry,
};
pub use ports::{AttachmentEvents, BusReader, InputCommands, LogReader, RuntimeReader};
pub use supervisor::{
    ConnectionState, SupervisorCommands, SupervisorFeed, is_connection_unavailable,
};
