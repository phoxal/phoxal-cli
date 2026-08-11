//! Terminal presentation primitives for the Phoxal CLI.
//!
//! This crate owns terminal rendering and interaction state. It must not own
//! supervisor/client transport, sockets, bus sessions, or command execution.
//! The remote facts it renders arrive as `phoxal_client` types; it names no
//! wire crate directly. `phoxal-runtime-contract` stays a direct dependency
//! because identities, participant kinds, and the clock are shared domain
//! primitives this crate also uses for purely local rendering state.

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
