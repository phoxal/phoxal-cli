//! Terminal presentation primitives for the Phoxal CLI.
//!
//! This crate owns terminal rendering and interaction state. It must not own
//! session transport, sockets, bus sessions, or command execution. The remote
//! facts it renders arrive as observations, and the framework values inside
//! them - identities, participant kinds, supervisor contract types - are named
//! by their canonical `phoxal` paths, because a second spelling of a value the
//! renderer only displays could only drift.

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
pub mod world;

pub use app::{AttachmentOutcome, Effect, EffectSenders, SessionInput, UiOptions, run};
pub use theme::{ColorCapability, Role, Theme};
pub use world::{WorldInput, WorldOutcome, WorldUiOptions, run as run_world};
