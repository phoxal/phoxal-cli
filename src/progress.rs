//! Append-only status reporting for long-running dependency work.
//!
//! Active sessions route messages through their diagnostics channel so the
//! TUI remains the sole terminal owner. Finite commands emit deterministic
//! stderr lines; there are deliberately no spinners, progress bars, redraws,
//! or cursor-control sequences. Grouped update rows stay silent in batch mode
//! because the update command emits its own deterministic final summary.

use crate::session::diagnostics::{RouteResult, try_route};
use phoxal_cli_core::session::event::{DiagnosticLevel, DiagnosticSource};
use phoxal_cli_ui::{Role, Theme};

/// Status handle used to keep failure reporting on the same output route as
/// the initial line.
pub enum Handle {
    /// The start line was written directly to stderr.
    Plain,
    /// An active session absorbed the start message.
    Routed,
}

/// A group of stable, append-only completion rows.
pub struct Rows {
    visible: bool,
    theme: Theme,
}

/// One worker-owned completion row.
pub enum Row {
    Visible { theme: Theme },
    Routed,
    Silent,
}

impl Rows {
    #[must_use]
    pub fn new(interactive: bool) -> Self {
        Self {
            visible: interactive,
            theme: Theme::detect_stderr(interactive),
        }
    }

    #[must_use]
    pub fn begin(&self, message: impl Into<String>) -> Row {
        let message = message.into();
        if !matches!(route_progress(&message), RouteResult::NoSession) {
            return Row::Routed;
        }
        if self.visible {
            Row::Visible { theme: self.theme }
        } else {
            Row::Silent
        }
    }

    pub fn completed(&self, message: impl Into<String>) {
        let message = message.into();
        if !matches!(route_progress(&message), RouteResult::NoSession) {
            return;
        }
        if self.visible {
            eprintln!("{} {message}", self.theme.paint(Role::Success, "✓"));
        }
    }
}

impl Row {
    pub fn finish(self, message: impl Into<String>) {
        let message = message.into();
        match self {
            Self::Visible { theme } => {
                eprintln!("{} {message}", theme.paint(Role::Success, "✓"));
            }
            Self::Routed => {
                let _ = try_route(
                    DiagnosticSource::Dependency,
                    DiagnosticLevel::Info,
                    &message,
                );
            }
            Self::Silent => {}
        }
    }

    pub fn abandon(self, message: impl Into<String>) {
        let message = message.into();
        match self {
            Self::Visible { theme } => {
                eprintln!("{} {message}", theme.paint(Role::Error, "✗"));
            }
            Self::Routed => {
                let _ = try_route(
                    DiagnosticSource::Dependency,
                    DiagnosticLevel::Warn,
                    &message,
                );
            }
            Self::Silent => {}
        }
    }
}

fn route_progress(message: &str) -> RouteResult {
    try_route(DiagnosticSource::Dependency, DiagnosticLevel::Info, message)
}

#[must_use]
pub fn status(message: impl Into<String>) -> Handle {
    start(message.into())
}

fn start(message: String) -> Handle {
    if !matches!(route_progress(&message), RouteResult::NoSession) {
        return Handle::Routed;
    }
    eprintln!("{message}");
    Handle::Plain
}

impl Handle {
    pub fn abandon_with_message(self, message: impl Into<String>) {
        let message = message.into();
        match self {
            Self::Plain => eprintln!("{message}"),
            Self::Routed => {
                let _ = try_route(
                    DiagnosticSource::Dependency,
                    DiagnosticLevel::Warn,
                    &message,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::diagnostics::DIAGNOSTICS_TEST_LOCK;

    #[test]
    fn finite_commands_receive_append_only_handles() {
        let _guard = DIAGNOSTICS_TEST_LOCK.blocking_lock();
        assert!(matches!(status("fetching"), Handle::Plain));
    }

    #[test]
    fn an_installed_session_routes_status_messages() {
        let _guard = DIAGNOSTICS_TEST_LOCK.blocking_lock();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        crate::session::diagnostics::install(tx);
        let handle = status("building");
        assert!(matches!(handle, Handle::Routed));
        handle.abandon_with_message("build failed");
        crate::session::diagnostics::uninstall();

        let mut messages = Vec::new();
        while let Ok(phoxal_cli_core::session::event::SessionEvent::Diagnostic {
            message, ..
        }) = rx.try_recv()
        {
            messages.push(message);
        }
        assert_eq!(messages, ["building", "build failed"]);
    }

    #[test]
    fn grouped_rows_remain_silent_for_batch_output() {
        let _guard = DIAGNOSTICS_TEST_LOCK.blocking_lock();
        let rows = Rows::new(false);
        assert!(matches!(rows.begin("downloading"), Row::Silent));
    }
}
