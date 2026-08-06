//! Authoritative resident state and exact spawned-participant readiness.

pub(crate) mod readiness;
mod snapshot;
mod store;

pub use store::SupervisorState;
