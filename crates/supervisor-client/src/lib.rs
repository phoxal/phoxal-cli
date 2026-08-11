//! Typed attachment behavior for one running Phoxal execution.
//!
//! [`Attachment`] uniquely owns the transport and its background work. Cloneable
//! [`AttachmentPort`] values borrow that attachment's authority without gaining
//! the ability to close it. Reattaching always performs a fresh handshake; an
//! execution-id change is a new run, never a resumed session.
//!
//! The supervisor surface carries exactly one negotiated identity: the
//! framework train each binary was built from, exchanged over the frozen
//! `supervisor/connect` bootstrap and compared by compatibility line. Every
//! other endpoint is reached only once that comparison agrees, so no ordinary
//! reply ever has to carry a version of its own.

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
