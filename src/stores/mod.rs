//! Bounded, single-responsibility TUI state stores (Target design part 5).
//!
//! Today's `tui::logs::LogRouter` conflates three concerns in one struct: the
//! per-runtime log scrollback, the telemetry rolling history, and the
//! router's latest traffic sample - and its telemetry dedup is by equal
//! VALUE, which flattens a genuinely-unchanging-but-still-live series into a
//! single stale point (see `TelemetryStore`'s docs for the fix). This module
//! splits that into two focused stores:
//!
//! - [`log_store::LogStore`] - routed log scrollback + diagnostic events
//!   only.
//! - [`telemetry_store::TelemetryStore`] - timestamped latest samples with
//!   freshness, plus bounded histories, deduped by sample identity/time
//!   rather than by value.
//!
//! The third store the target design names, `RuntimeStore`, already exists
//! as `supervisor::BoardBackend`/`BoardSnapshot` and is not rebuilt here.
//!
//! This slice is purely additive: `tui::logs::LogRouter` and
//! `telemetry::TelemetryBackend` are untouched, and nothing yet reads from
//! these new stores - a later slice swaps the consumers over and deletes the
//! old coupling.

pub mod log_store;
pub mod telemetry_store;
