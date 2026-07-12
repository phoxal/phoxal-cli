use std::process::{Child, Command, ExitStatus};

use anyhow::{Context, Result};

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
    /// `error` is deliberately excluded from this gate - a fatal failure is
    /// not decoration, and main.rs's top-level handler relies on it being
    /// the one diagnostic that always reaches the user, JSON mode or not.
    fn should_print_decoration(&self) -> bool {
        !crate::progress::current_mode().is_json()
    }

    pub fn step<T>(
        &self,
        title: impl AsRef<str>,
        callback: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let title_ref = title.as_ref();
        if self.should_print_decoration() {
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
        let theme = self.theme();
        eprintln!("{} {}", theme.bold(&theme.steel("info")), message.as_ref());
    }

    pub fn success(&self, message: impl AsRef<str>) {
        if !self.should_print_decoration() {
            return;
        }
        let theme = self.theme();
        eprintln!("{} {}", theme.bold(&theme.success("ok")), message.as_ref());
    }

    pub fn warn(&self, message: impl AsRef<str>) {
        if !self.should_print_decoration() {
            return;
        }
        let theme = self.theme();
        eprintln!("{} {}", theme.bold(&theme.warn("warn")), message.as_ref());
    }

    pub fn error(&self, message: impl AsRef<str>) {
        let theme = self.theme();
        eprintln!("{} {}", theme.bold(&theme.error("error")), message.as_ref());
    }

    pub fn command_status(&self, command: &mut Command) -> Result<ExitStatus> {
        command.status().context("failed to run command")
    }

    pub fn command_spawn(&self, command: &mut Command) -> Result<Child> {
        command.spawn().context("failed to spawn command")
    }
}
