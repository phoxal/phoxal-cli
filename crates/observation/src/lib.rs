//! Renderer-neutral observations produced by disposable CLI clients.
//!
//! This crate owns immutable observation DTOs and query/window envelopes. It
//! must not own mutable stores, tasks, channels, sockets, Zenoh, reconciliation,
//! rendering, or command execution.

#![allow(clippy::module_name_repetitions)]
