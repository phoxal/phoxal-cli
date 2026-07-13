use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::output_mode::OutputMode;
use crate::session::diagnostics::{RouteResult, register_child, try_route, unregister_child};
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
            && matches!(
                try_route(DiagnosticSource::Cli, DiagnosticLevel::Info, title_ref),
                RouteResult::NoSession
            )
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
        if !matches!(
            try_route(DiagnosticSource::Cli, DiagnosticLevel::Info, message),
            RouteResult::NoSession
        ) {
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
        if !matches!(
            try_route(DiagnosticSource::Cli, DiagnosticLevel::Info, message),
            RouteResult::NoSession
        ) {
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
        if !matches!(
            try_route(DiagnosticSource::Cli, DiagnosticLevel::Warn, message),
            RouteResult::NoSession
        ) {
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
        if !matches!(
            try_route(DiagnosticSource::Cli, DiagnosticLevel::Error, message),
            RouteResult::NoSession
        ) {
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
    /// unchanged behavior. While a command is running, both streams are
    /// routine dependency progress and route at `Info`, which keeps successful
    /// Cargo compiler chatter out of a TUI. Stderr is retained and replayed at
    /// `Error` only if the command exits unsuccessfully, so the useful failure
    /// diagnosis remains visible.
    pub fn command_status_captured(&self, command: &mut Command) -> Result<ExitStatus> {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let child = Arc::new(Mutex::new(
            command.spawn().context("failed to spawn command")?,
        ));
        register_child(child.clone());
        let stdout = {
            child
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .stdout
                .take()
                .context("child stdout was not piped")
        };
        let stderr = {
            child
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .stderr
                .take()
                .context("child stderr was not piped")
        };
        let (stdout, stderr) = match (stdout, stderr) {
            (Ok(stdout), Ok(stderr)) => (stdout, stderr),
            (Err(error), _) | (_, Err(error)) => {
                unregister_child(&child);
                return Err(error);
            }
        };
        let mode = self.mode;
        let stdout_thread = std::thread::spawn(move || {
            forward_captured_output(stdout, DiagnosticLevel::Info, false, mode)
        });
        let stderr_thread = std::thread::spawn(move || {
            forward_captured_output(stderr, DiagnosticLevel::Info, true, mode)
        });
        let status = wait_for_captured_child(&child);
        unregister_child(&child);
        // Best-effort: a panicked reader thread must not fail the build
        // itself - the command's own exit status is still authoritative.
        let _ = stdout_thread.join();
        let stderr_lines = stderr_thread.join().unwrap_or_default();
        if status.as_ref().is_ok_and(|status| !status.success()) {
            for line in stderr_lines {
                let _ = try_route(DiagnosticSource::Dependency, DiagnosticLevel::Error, &line);
            }
        }
        status
    }
}

fn wait_for_captured_child(child: &Arc<Mutex<Child>>) -> Result<ExitStatus> {
    loop {
        if let Some(status) = child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .try_wait()
            .context("failed to wait for command")?
        {
            return Ok(status);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Forward every line from a captured child stream as a `Dependency`
/// diagnostic, falling back to a direct write when no session is installed.
fn forward_captured_output(
    reader: impl Read,
    level: DiagnosticLevel,
    is_stderr: bool,
    mode: OutputMode,
) -> Vec<String> {
    let mut captured = Vec::new();
    for line in BufReader::new(reader).lines().map_while(Result::ok) {
        if line.is_empty() {
            continue;
        }
        if may_write_raw(try_route(DiagnosticSource::Dependency, level, &line), mode) {
            if is_stderr {
                eprintln!("{line}");
            } else {
                println!("{line}");
            }
        }
        captured.push(line);
    }
    captured
}

/// Raw output is only permitted when no session owns the terminal and the
/// invocation is not JSON. `Dropped` is intentionally not a fallback case.
fn may_write_raw(route: RouteResult, mode: OutputMode) -> bool {
    matches!(route, RouteResult::NoSession) && !mode.is_json()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_but_full_session_never_falls_back_to_raw_output() {
        assert!(!may_write_raw(RouteResult::Dropped, OutputMode::Rich));
        assert!(!may_write_raw(RouteResult::Dropped, OutputMode::Plain));
        assert!(!may_write_raw(RouteResult::Dropped, OutputMode::Json));
        assert!(!may_write_raw(RouteResult::NoSession, OutputMode::Json));
        assert!(may_write_raw(RouteResult::NoSession, OutputMode::Plain));
    }
}
