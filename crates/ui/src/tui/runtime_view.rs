//! Stateless runtime classification used by the current single renderer.
//!
//! Runtime timing and origin are projected by the attachment client directly
//! into each immutable participant observation; the UI retains no process
//! history of its own.

#[derive(Debug, Clone, Default)]
pub struct RuntimeView;

impl RuntimeView {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    pub fn observe_board(&mut self, _board: &phoxal_cli_core::session::BoardSnapshot) {}

    #[must_use]
    pub fn is_user_service(&self, status: &phoxal_cli_core::session::ParticipantStatus) -> bool {
        status.runtime_user_service
    }
}
