//! Disposable attachment client for a resident Phoxal runtime.

mod attachment;
mod ports;
pub mod reconcile;
mod sources;
mod state;
pub mod supervisor;

pub use attachment::{
    Attachment, AttachmentPorts, AttachmentRuntime, attach, attach_with_supervisor,
};
pub use ports::{AttachmentEvents, BusReader, InputCommands, LogReader, RuntimeReader};
pub use supervisor::{
    ConnectionState, SupervisorCommands, SupervisorFeed, is_connection_unavailable,
};
