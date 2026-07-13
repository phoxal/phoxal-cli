use std::process::{Child, Command, ExitStatus};

use anyhow::{Context, Result};

use crate::session::diagnostics::try_route;
use crate::session::event::{DiagnosticLevel, DiagnosticSource};
use crate::theme::Theme;

#[derive(Debug, Clone, Copy, Default)]
pub struct Ui;

impl Ui {
    pub fn new() -> Self {
        Self
    }

    fn theme(&self) -> Theme {
        Theme::detect_stderr()
    }

    /// `--message-format json` promises stdout carries only the JSON
    /// document and stderr carries nothing (see [`crate::output_mode`]).
    fn should_print_decoration(&self) -> bool {
        !crate::progress::current_mode().is_json()
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
