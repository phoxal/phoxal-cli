//! Terminal-independent project behavior for the Phoxal CLI.
//!
//! The root binary depends on this crate. This crate must not depend on command
//! parsing or terminal-rendering code.

#![allow(clippy::module_name_repetitions)]

pub mod advisory;
pub mod check;
pub mod identity;
pub mod project;
pub mod runtime;
pub mod schema;
pub mod simulation;

pub use project::Project;
