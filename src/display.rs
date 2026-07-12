//! The live session display (Part 2): picks and drives whichever of the
//! full-screen TUI ([`crate::tui`]), the append-only line logger
//! ([`crate::logger`]), or nothing at all applies for the current
//! [`crate::output_mode::OutputMode`] - see the matrix in that module's docs.
//!
//! [`Display`] is driven from inside
//! [`crate::supervisor::supervise_until_shutdown`]'s own `select!` loop (see
//! [`crate::tui`]'s module docs for why), not a separate task - this module
//! is the thin per-mode dispatch layer that loop calls into, so the
//! supervisor itself never has to know which of the three is live.

use std::io;

use crossterm::event::Event;

use crate::identity::IdentitySummary;
use crate::logger::LineLogger;
use crate::output_mode::OutputMode;
use crate::supervisor::BoardSnapshot;
use crate::telemetry::TelemetryBackend;
use crate::theme::Theme;
use crate::tui::{TerminalGuard, TitleInfo, TuiDisplay};

/// What a display's input handling asked the supervisor loop to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayAction {
    /// Purely internal to the display (navigation, tab switch, filter typing
    /// ...) - nothing for the supervisor loop to act on.
    None,
    /// `q`/Ctrl-C from within the TUI - same effect as the loop's own
    /// `tokio::signal::ctrl_c()` branch (which raw mode's disabled `ISIG`
    /// means never actually fires while the TUI owns the terminal).
    Quit,
    /// `r restart` on the selected runtime - the supervisor loop turns this
    /// into a `SupervisorAction::Restart` through the same `handle_action`
    /// every other in-process action (a hot-reload `Swap`, `status
    /// release`/`resume`) already goes through.
    Restart(String),
    /// `↵` on a device row in the joypad Devices tab - publish
    /// `joypad::Connect { id }`. The selection shown afterward comes from the
    /// tool's own next `Devices` publish (the ack), never set locally here.
    JoypadConnect(String),
    /// `r` in the joypad Devices tab - publish `joypad::Rescan {}`.
    JoypadRescan,
}

/// The live display for one supervised session. `Tui` is boxed: it carries a
/// whole ratatui `Terminal` plus the app/log state, dwarfing the other two
/// variants - boxing keeps `Display` (and everything that embeds it, e.g.
/// `SupervisorOptions`) small to move/pass around regardless of which
/// variant is live.
pub enum Display {
    Tui(Box<TuiDisplay>),
    Logger(LineLogger),
    /// `--message-format json`: the output-mode matrix promises stderr
    /// carries nothing at all, so this variant is a pure no-op everywhere.
    None,
}

impl std::fmt::Debug for Display {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tui(tui) => f.debug_tuple("Display::Tui").field(tui).finish(),
            Self::Logger(_) => f.write_str("Display::Logger"),
            Self::None => f.write_str("Display::None"),
        }
    }
}

impl Display {
    /// Choose the display for `mode_label` (`"run"`/`"simulation"`) and the
    /// already-discovered robot identity (reused from the identity line so
    /// the TUI title bar agrees with it - see `crate::identity`).
    ///
    /// Guards on `OutputMode::Rich` **and** a direct `stderr().is_terminal()`
    /// check (belt-and-suspenders even though `Rich` is already only ever
    /// computed for a real TTY - see `OutputMode::compute`): a TUI must never
    /// take over a non-interactive stream.
    #[must_use]
    pub fn for_mode(mode_label: &'static str, identity: Option<IdentitySummary>) -> Self {
        match crate::progress::current_mode() {
            OutputMode::Json => Self::None,
            OutputMode::Rich if TerminalGuard::should_use_terminal(&io::stderr()) => {
                let theme = Theme::detect_stderr();
                let title = TitleInfo {
                    robot: identity
                        .as_ref()
                        .map_or_else(|| "-".to_string(), |summary| summary.robot.clone()),
                    channel: identity
                        .as_ref()
                        .map_or_else(|| "-".to_string(), |summary| summary.channel.clone()),
                    mode: mode_label,
                };
                Self::Tui(Box::new(TuiDisplay::new(theme, title)))
            }
            _ => Self::Logger(LineLogger::new(mode_label)),
        }
    }

    /// Only ever `Display::None`, for callers (`status`, `logs`, any test)
    /// that never construct a live session display.
    #[must_use]
    pub const fn none() -> Self {
        Self::None
    }

    /// Enter the terminal (`Tui`) or otherwise a no-op (`Logger`/`None`).
    pub fn activate(&mut self) -> io::Result<()> {
        match self {
            Self::Tui(tui) => tui.activate(),
            Self::Logger(_) | Self::None => Ok(()),
        }
    }

    pub fn redraw(&mut self, board: &BoardSnapshot, telemetry: &TelemetryBackend) {
        match self {
            Self::Tui(tui) => tui.redraw(board, telemetry),
            Self::Logger(logger) => logger.redraw(board),
            Self::None => {}
        }
    }

    /// Cancel-safe (see `TuiDisplay::next_input`'s docs); `Logger`/`None`
    /// never produce input, so this future simply never resolves for them -
    /// safe to poll repeatedly inside a `select!` loop.
    pub async fn next_input(&mut self) -> Option<Event> {
        match self {
            Self::Tui(tui) => tui.next_input().await,
            Self::Logger(_) | Self::None => std::future::pending().await,
        }
    }

    pub fn handle_input(
        &mut self,
        event: Event,
        board: &BoardSnapshot,
        telemetry: &TelemetryBackend,
    ) -> DisplayAction {
        match self {
            Self::Tui(tui) => tui.handle_input(event, board, telemetry),
            Self::Logger(_) | Self::None => DisplayAction::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_variant_is_a_no_op_display() {
        let mut display = Display::none();
        assert!(display.activate().is_ok());
        display.redraw(&BoardSnapshot::default(), &TelemetryBackend::default());
    }
}
