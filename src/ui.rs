use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, ExitStatus, Stdio};

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

    /// Run `command` with its stdout/stderr CAPTURED and routed line-by-line
    /// as `SessionEvent::Diagnostic`s, instead of inherited straight through
    /// to this process's own stdout/stderr (findings A2/B2). A raw child
    /// write racing an active TUI redraw can corrupt the alternate-screen
    /// frame; under `--message-format json` it would also leak onto a stderr
    /// the contract promises stays empty. Falls back to a direct
    /// `eprintln!`/`println!` for a line that arrives when no session is
    /// installed (`try_route` returns `false`) - identical to `Ui::info`/
    /// `warn`'s own fallback, so a caller with no active session (a bare
    /// `cargo build` outside any `run`/`simulation run` session) sees
    /// unchanged behavior. Stdout is routed at `Info`, stderr at `Warn` - the
    /// same approximation `session::diagnostics::SessionWriter` already
    /// documents for captured `tracing` output, since neither stream states
    /// its own severity per line.
    pub fn command_status_captured(&self, command: &mut Command) -> Result<ExitStatus> {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command.spawn().context("failed to spawn command")?;
        let stdout = child.stdout.take().context("child stdout was not piped")?;
        let stderr = child.stderr.take().context("child stderr was not piped")?;
        let stdout_thread = std::thread::spawn(move || {
            forward_captured_output(stdout, DiagnosticLevel::Info, false);
        });
        let stderr_thread = std::thread::spawn(move || {
            forward_captured_output(stderr, DiagnosticLevel::Warn, true);
        });
        let status = child.wait().context("failed to wait for command")?;
        // Best-effort: a panicked reader thread must not fail the build
        // itself - the command's own exit status is still authoritative.
        let _ = stdout_thread.join();
        let _ = stderr_thread.join();
        Ok(status)
    }
}

/// Forward every line from a captured child stream as a `Dependency`
/// diagnostic, falling back to a direct write when no session is installed.
fn forward_captured_output(reader: impl Read, level: DiagnosticLevel, is_stderr: bool) {
    for line in BufReader::new(reader).lines().map_while(Result::ok) {
        if line.is_empty() {
            continue;
        }
        if try_route(DiagnosticSource::Dependency, level, &line) {
            continue;
        }
        if is_stderr {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    }
}
