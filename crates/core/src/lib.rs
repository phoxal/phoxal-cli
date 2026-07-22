//! Terminal-independent project behavior for the Phoxal CLI.
//!
//! The root binary depends on this crate. This crate must not depend on command
//! parsing or terminal-rendering code.

#![allow(clippy::module_name_repetitions)]

pub mod artifacts;
pub mod check;
pub mod deploy;
pub mod project;
pub mod session;
pub mod simulation;
pub mod supervisor_api;

pub use project::Project;
