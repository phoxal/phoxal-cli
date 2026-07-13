use std::process::{Child, Command, ExitStatus};

use anyhow::{Context, Result};

use crate::output_mode::OutputMode;
use crate::session::diagnostics::try_route;
use crate::session::event::{DiagnosticLevel, DiagnosticSource};
use crate::theme::Theme;

#[derive(Debug, Clone, Copy)]
pub struct Ui {
    mode: OutputMode,
}

impl Ui {
    /// Construct with an explicit, already-known output mode - the preferred
    /// constructor everywhere an `AppContext`/`OutputContext` is in scope
    /// (i.e. everywhere downstream of `commands::dispatch`).
    pub fn new(mode: OutputMode) -> Self {
        Self { mode }
    }

    /// Compute the mode fresh from the live environment. For the narrow set
    /// of call sites with no `AppContext` in scope: `main`'s top-level
    /// pre-dispatch error path, and tests/library callers that do not care
    /// which mode they get.
    #[must_use]
    pub fn from_env() -> Self {
        Self::new(OutputMode::from_env())
    }

    /// This `Ui`'s output mode - for a caller that needs to pass the same
    /// mode on to another mode-aware helper (a spinner, a catalog fetch)
    /// without threading a second, separate parameter alongside `ui`.
    #[must_use]
    pub const fn mode(&self) -> OutputMode {
        self.mode
    }

    fn theme(&self) -> Theme {
        Theme::detect_stderr(self.mode)
    }

    /// `--message-format json` promises stdout carries only the JSON
    /// document and stderr carries nothing (see [`crate::output_mode`]).
    fn should_print_decoration(&self) -> bool {
        !self.mode.is_json()
    }

    pub fn step<T>(
        &self,
        title: impl AsRef<str>,
        callback: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let title_ref = title.as_ref();
        if self.should_print_decoration()
            && !try_route(DiagnosticSource::Cli, DiagnosticLevel::Info, title_ref)
        {
            let theme = self.theme();
            eprintln!("{} {}", theme.accent(">"), theme.bold(title_ref));
        }

        match callback() {
            Ok(val) => {
                self.success(title);
                Ok(val)
            }
            Err(e) => {
                self.error(title);
                Err(e)
            }
        }
    }

    pub fn info(&self, message: impl AsRef<str>) {
        if !self.should_print_decoration() {
            return;
        }
        let message = message.as_ref();
        if try_route(DiagnosticSource::Cli, DiagnosticLevel::Info, message) {
            return;
        }
        let theme = self.theme();
        eprintln!("{} {}", theme.bold(&theme.steel("info")), message);
    }

    pub fn success(&self, message: impl AsRef<str>) {
        if !self.should_print_decoration() {
            return;
        }
        let message = message.as_ref();
        if try_route(DiagnosticSource::Cli, DiagnosticLevel::Info, message) {
            return;
        }
        let theme = self.theme();
        eprintln!("{} {}", theme.bold(&theme.success("ok")), message);
    }

    pub fn warn(&self, message: impl AsRef<str>) {
        if !self.should_print_decoration() {
            return;
        }
        let message = message.as_ref();
        if try_route(DiagnosticSource::Cli, DiagnosticLevel::Warn, message) {
            return;
        }
        let theme = self.theme();
        eprintln!("{} {}", theme.bold(&theme.warn("warn")), message);
    }

    pub fn error(&self, message: impl AsRef<str>) {
        if !self.should_print_decoration() {
            return;
        }
        let message = message.as_ref();
        if try_route(DiagnosticSource::Cli, DiagnosticLevel::Error, message) {
            return;
        }
        let theme = self.theme();
        eprintln!("{} {}", theme.bold(&theme.error("error")), message);
    }

    pub fn command_status(&self, command: &mut Command) -> Result<ExitStatus> {
        command.status().context("failed to run command")
    }

    pub fn command_spawn(&self, command: &mut Command) -> Result<Child> {
        command.spawn().context("failed to spawn command")
    }
}
