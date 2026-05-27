use std::process::{Child, Command, ExitStatus};

use anyhow::{Context, Result};
use console::style;

#[derive(Debug, Clone, Copy, Default)]
pub struct Ui;

impl Ui {
    pub fn new() -> Self {
        Self
    }

    pub fn step<T>(
        &self,
        title: impl AsRef<str>,
        callback: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let title_ref = title.as_ref();
        eprintln!("{} {}", style(">").cyan().bold(), style(title_ref).bold());

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
        eprintln!("{} {}", style("info").blue().bold(), message.as_ref());
    }

    pub fn success(&self, message: impl AsRef<str>) {
        eprintln!("{} {}", style("ok").green().bold(), message.as_ref());
    }

    pub fn warn(&self, message: impl AsRef<str>) {
        eprintln!("{} {}", style("warn").yellow().bold(), message.as_ref());
    }

    pub fn error(&self, message: impl AsRef<str>) {
        eprintln!("{} {}", style("error").red().bold(), message.as_ref());
    }

    pub fn command_status(&self, command: &mut Command) -> Result<ExitStatus> {
        command.status().context("failed to run command")
    }

    pub fn command_spawn(&self, command: &mut Command) -> Result<Child> {
        command.spawn().context("failed to spawn command")
    }
}
