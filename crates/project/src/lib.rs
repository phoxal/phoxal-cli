//! Project loading, validation, build, materialization, and staging ownership.
//!
//! This crate will turn authored project inputs into validated runtime plans and
//! resolve logical roots into shared runtime targets. It must not parse CLI
//! arguments, render terminal output, or own resident/client lifecycle.

#![allow(clippy::module_name_repetitions)]
