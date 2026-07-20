//! Bounded session stores for logs, runtime observations, and telemetry.
//!
//! These stores depend only on terminal-neutral session and project records.
//! Presentation reads their snapshots; bus and process adapters update them.

pub mod log;
pub mod runtime;
pub mod telemetry;
