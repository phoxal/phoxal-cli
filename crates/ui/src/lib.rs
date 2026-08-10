//! Terminal presentation primitives for the Phoxal CLI.
//!
//! This crate owns terminal rendering and interaction state. It must not own
//! supervisor/client transport, sockets, bus sessions, or command execution.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::todo,
        clippy::unimplemented
    )
)]

pub mod app;
pub mod components;
pub mod format;
pub mod terminal;
pub mod theme;

pub use app::{AttachmentOutcome, Effect, EffectSenders, SessionInput, UiOptions, run};
pub use theme::{ColorCapability, Role, Theme};
