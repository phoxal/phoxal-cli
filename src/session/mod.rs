//! The event-driven session core: the typed lifecycle event stream
//! ([`event`]), the pure session state machine ([`state`]), the immutable
//! output contract ([`output`]), and the [`controller::SessionController`]
//! that owns terminal/cancellation/renderer selection and drives `run`/
//! `simulation run` end to end.
//!
//! [`event`] and [`state`] are pure data plus pure transitions - no terminal,
//! channel, or supervisor wiring lives there. That keeps them independently
//! testable and free of a dependency on the heavier `supervisor`/`tui`
//! modules; [`controller`] is the slice that wires them to a real terminal,
//! a real `CancellationToken`, and the supervisor.

pub mod controller;
pub mod diagnostics;
pub mod event;
pub mod output;
pub mod state;
