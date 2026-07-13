//! The event-driven session core: the typed lifecycle event stream
//! ([`event`]) and the pure session state machine ([`state`]) that a later
//! `SessionController` slice drives.
//!
//! Both modules are pure data plus pure transitions - no terminal, channel,
//! or supervisor wiring lives here. That keeps them independently testable
//! and free of a dependency on the heavier `supervisor`/`tui` modules.

pub mod event;
pub mod state;
