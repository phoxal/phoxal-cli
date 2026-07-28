//! Resident process and runtime-authority ownership for the Phoxal CLI.
//!
//! This crate will own locks, child processes, router readiness, systemd
//! integration, resident state, and the protocol server. It must never depend
//! on the disposable client or terminal UI.

#![allow(clippy::module_name_repetitions)]
