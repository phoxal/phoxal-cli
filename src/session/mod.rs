//! Root adapters for terminal, diagnostics, cancellation, and session output.
//!
//! Typed lifecycle events and the pure state machine live in
//! `phoxal_cli_core::session`; this module connects them to the process and TUI
//! adapters used by interactive commands.

pub mod controller;
pub mod diagnostics;
pub mod output;
