//! Typed attachment behavior for one running Phoxal execution.
//!
//! [`Attachment`] uniquely owns the transport and its background work. Cloneable
//! [`AttachmentPort`] values borrow that attachment's authority without gaining
//! the ability to close it. Reattaching always performs a fresh handshake; an
//! execution-id change is a new run, never a resumed session.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::todo,
        clippy::unimplemented
    )
)]

mod attachment;
mod error;
mod router;

pub use attachment::{Attachment, AttachmentConfig, AttachmentPort, Connected};
pub use error::AttachError;
