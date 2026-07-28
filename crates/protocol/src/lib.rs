//! Resident-socket protocol ownership for the Phoxal CLI.
//!
//! This crate will own wire DTOs, frame limits, pure codecs, and thin I/O
//! adapters. It must not own sockets, listeners, reconnect loops, resident
//! state, command execution, or UI projections.

#![allow(clippy::module_name_repetitions)]
