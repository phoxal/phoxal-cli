//! Bounded, single-responsibility TUI state stores (Target design part 5).
//!
//! The old `tui::logs::LogRouter` (deleted) conflated three concerns in one
//! struct: the per-runtime log scrollback, the telemetry rolling history, and
//! the router's latest traffic sample - and its telemetry dedup was by equal
//! VALUE, which flattened a genuinely-unchanging-but-still-live series into a
//! single stale point (see `TelemetryStore`'s docs for the fix). This module
//! splits that into three focused stores:
//!
//! - [`log_store::LogStore`] - routed log scrollback + diagnostic events
//!   only.
//! - [`telemetry_store::TelemetryStore`] - timestamped latest samples with
//!   freshness, plus bounded histories, deduped by sample identity/time
//!   rather than by value.
//! - [`runtime_store::RuntimeStore`] - session-only participant metadata
//!   (artifact reference, declared contracts, launch ownership, time-to-
//!   ready) resolved once from the launch plan and its contract-check
//!   outcome. Finding A5: an earlier version of this doc claimed this store
//!   "already exists as `supervisor::BoardBackend`/`BoardSnapshot`" - that
//!   was wrong. The board is the persisted lifecycle record;
//!   none of the above belongs on it, so it stayed `unknown` in the Overview
//!   panel until this sidecar was added. See that module's own docs.
//!
//! [`telemetry_store::TelemetryStore`] is now the live store behind
//! `telemetry::TelemetryBackend` (Wave C2): every `record_*` call from a
//! background feed task lands here at its actual receive time, so
//! `TelemetrySnapshot`'s samples carry real freshness. [`log_store::LogStore`]
//! is now the TUI's log-scrollback ring, replacing the old
//! `tui::logs::LogRouter` (which conflated log lines with the telemetry
//! history/traffic-sample coupling this store split apart) - that module has
//! been deleted.

pub mod log_store;
pub mod runtime_store;
pub mod telemetry_store;
