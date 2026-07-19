//! [`DisplayAction`]: what a renderer's input handling asks the session to
//! do. Produced by [`crate::tui::TuiDisplay::handle_input`] and consumed by
//! [`crate::session::controller::SessionController`], which owns terminal
//! acquisition, the TUI, and cancellation directly - the old `Display` enum that used to
//! live here (picking and driving the renderer from inside
//! `crate::supervisor::supervise_until_shutdown`'s own `select!` loop) is
//! superseded by that controller and has been removed; the supervisor no
//! longer knows about ratatui, input, or display activation at all.

/// What a display's input handling asked the session to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayAction {
    /// Purely internal to the display (navigation, tab switch, filter typing
    /// ...) - nothing for the session to act on.
    None,
    /// `q`/Ctrl-C from within the TUI - same effect as the controller's own
    /// `tokio::signal::ctrl_c()` branch (which raw mode's disabled `ISIG`
    /// means never actually fires while the TUI owns the terminal).
    Quit,
    /// `r restart` on the selected runtime - the controller turns this into a
    /// `SupervisorAction::Restart` sent down the same action channel
    /// `--watch` hot-reload already uses (see
    /// `SessionController::set_restart_channel`).
    Restart(String),
    /// Enter on a device row in Input publishes joypad::Select. The selection
    /// shown afterward comes from the
    /// tool's own next `Devices` publish (the ack), never set locally here.
    JoypadSelect(String),
    /// e/x in Input publishes the requested authoritative enable state.
    JoypadSetEnabled(bool),
    /// r in Input publishes joypad::Rescan.
    JoypadRescan,
}
