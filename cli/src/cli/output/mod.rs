//! The explicit output contract for one invocation: whether stderr is an
//! interactive terminal and the theme selected for it.
//!
//! Built once in [`crate::cli::dispatch`] and threaded explicitly into
//! the attachment application, `AppContext::ui`, and helpers
//! that may draw progress. Interactive foreground sessions are admitted only
//! on a real TTY, so the controller itself has exactly one renderer: the TUI.

use std::io::IsTerminal;

use phoxal_cli_ui::Theme;

pub(crate) mod diagnostics;
pub(crate) mod plain;
pub(crate) mod progress;

#[must_use]
pub fn tracing_ansi_enabled() -> bool {
    std::io::stderr().is_terminal()
        && std::env::var("NO_COLOR").map_or(true, |value| value.is_empty())
}

/// The immutable output contract for one `run`/`simulation webots run` invocation.
#[derive(Debug, Clone, Copy)]
pub struct OutputContext {
    pub interactive: bool,
    pub theme: Theme,
}

impl OutputContext {
    #[must_use]
    pub const fn new(interactive: bool, theme: Theme) -> Self {
        Self { interactive, theme }
    }

    /// Whether finite-command presentation may use terminal decoration.
    #[must_use]
    pub const fn decorated(self) -> bool {
        self.interactive
    }

    /// Build from stderr's terminal state. Called once in
    /// [`crate::cli::dispatch`]; other callers should prefer [`Self::new`]
    /// so tests stay deterministic.
    #[must_use]
    pub fn compute(is_tty: bool) -> Self {
        Self::new(is_tty, Theme::detect_stderr(is_tty))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_cli_ui::ColorCapability;

    #[test]
    fn presentation_mode_depends_only_on_stderr_terminal_state() {
        // stdout is intentionally absent from this API: redirecting it cannot
        // change the mode selected from stderr.
        assert!(OutputContext::compute(true).interactive);
        assert!(!OutputContext::compute(false).interactive);
    }

    #[test]
    fn new_is_an_immutable_constructor() {
        let ctx = OutputContext::new(false, Theme::new(ColorCapability::None));
        assert!(!ctx.interactive);
    }
}
