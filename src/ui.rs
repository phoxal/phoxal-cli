use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::session::diagnostics::{RouteResult, register_child, try_route, unregister_child};
use phoxal_cli_core::session::event::{DiagnosticLevel, DiagnosticSource};
use phoxal_cli_ui::Theme;

#[derive(Debug, Clone, Copy)]
pub struct Ui {
    interactive: bool,
}

impl Ui {
    /// Construct with an explicit, already-known interactive flag - the preferred
    /// constructor everywhere an `AppContext`/`OutputContext` is in scope
    /// (i.e. everywhere downstream of `commands::dispatch`).
    pub fn new(interactive: bool) -> Self {
        Self { interactive }
    }

    /// Compute the flag fresh from the live environment. For the narrow set
    /// of call sites with no `AppContext` in scope: `main`'s top-level
    /// pre-dispatch error path, and tests/library callers that do not care
    /// which mode they get.
    #[must_use]
    pub fn from_env() -> Self {
        use std::io::IsTerminal;
        Self::new(std::io::stderr().is_terminal())
    }

    /// Whether this `Ui` may draw interactive terminal decoration.
    #[must_use]
    pub const fn interactive(&self) -> bool {
        self.interactive
    }

    fn theme(&self) -> Theme {
        Theme::detect_stderr(self.interactive)
    }

    pub fn info(&self, message: impl AsRef<str>) {
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

    /// Run `command` with its stdout/stderr CAPTURED and routed line-by-line
    /// as `SessionEvent::Diagnostic`s, instead of inherited straight through
    /// to this process's own stdout/stderr (findings A2/B2). A raw child
    /// write racing an active TUI redraw can corrupt the alternate-screen
    /// frame. Falls back to a direct
    /// `eprintln!`/`println!` for a line that arrives when no session is
    /// installed (`try_route` returns `false`) - identical to `Ui::info`/
    /// `warn`'s own fallback, so a caller with no active session (a bare
    /// `cargo build` outside any `run`/`simulation webots run` session) sees
    /// unchanged behavior. While a command is running, both streams are
    /// routine dependency progress and route at `Info`, which keeps successful
    /// Cargo compiler chatter out of a TUI. Stderr is retained and replayed at
    /// `Error` only if the command exits unsuccessfully, so the useful failure
    /// diagnosis remains visible.
    pub fn command_status_captured(&self, command: &mut Command) -> Result<ExitStatus> {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let child = Arc::new(Mutex::new(Some(
            command.spawn().context("failed to spawn command")?,
        )));
        register_child(child.clone());
        let stdout = {
            child
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_mut()
                .context("captured child was already reaped")?
                .stdout
                .take()
                .context("child stdout was not piped")
        };
        let stderr = {
            child
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_mut()
                .context("captured child was already reaped")?
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
        let stdout_thread = std::thread::spawn(move || {
            forward_captured_output(stdout, DiagnosticLevel::Info, false)
        });
        let stderr_thread = std::thread::spawn(move || {
            forward_captured_output(stderr, DiagnosticLevel::Info, true)
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

fn wait_for_captured_child(child: &Arc<Mutex<Option<Child>>>) -> Result<ExitStatus> {
    loop {
        {
            let mut child = child
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let running = child
                .as_mut()
                .context("captured child was already reaped")?;
            if let Some(status) = running.try_wait().context("failed to wait for command")? {
                child.take();
                return Ok(status);
            }
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
) -> Vec<String> {
    const MAX_CAPTURED_ERROR_LINES: usize = 200;
    let mut captured = VecDeque::new();
    for line in BufReader::new(reader).lines().map_while(Result::ok) {
        if line.is_empty() {
            continue;
        }
        if may_write_raw(try_route(DiagnosticSource::Dependency, level, &line)) {
            if is_stderr {
                eprintln!("{line}");
            } else {
                println!("{line}");
            }
        }
        if is_stderr {
            if captured.len() == MAX_CAPTURED_ERROR_LINES {
                captured.pop_front();
            }
            captured.push_back(line);
        }
    }
    captured.into()
}

/// Raw output is only permitted when no session owns the terminal and the
/// `Dropped` is intentionally not a fallback case.
fn may_write_raw(route: RouteResult) -> bool {
    matches!(route, RouteResult::NoSession)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_but_full_session_never_falls_back_to_raw_output() {
        assert!(!may_write_raw(RouteResult::Dropped));
        assert!(may_write_raw(RouteResult::NoSession));
    }

    #[test]
    fn captured_output_retains_only_a_bounded_stderr_tail() {
        let input = (0..250)
            .map(|index| format!("line-{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let stderr = forward_captured_output(input.as_bytes(), DiagnosticLevel::Info, true);
        assert_eq!(stderr.len(), 200);
        assert_eq!(stderr.first().map(String::as_str), Some("line-50"));
        assert_eq!(stderr.last().map(String::as_str), Some("line-249"));

        let stdout = forward_captured_output(input.as_bytes(), DiagnosticLevel::Info, false);
        assert!(stdout.is_empty());
    }
}
