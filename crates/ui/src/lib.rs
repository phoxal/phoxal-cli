//! Terminal presentation primitives for the Phoxal CLI.
//!
//! This crate owns terminal rendering and interaction state. It must not own
//! resident/client transport, sockets, bus sessions, or command execution.

pub mod ratatui;
pub mod theme;
pub mod tui;

pub use theme::{ColorCapability, Role, Theme};
