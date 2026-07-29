use crate::cli::output::diagnostics::{RouteResult, try_route};
use phoxal_cli_core::session::event::{DiagnosticLevel, DiagnosticSource};
use phoxal_cli_ui::Theme;

#[derive(Debug, Clone, Copy)]
pub struct Ui {
    interactive: bool,
}

impl phoxal_cli_project::Reporter for Ui {
    fn report(&self, event: phoxal_cli_project::PreparationEvent) {
        match event {
            phoxal_cli_project::PreparationEvent::Info(message) => self.info(message),
            phoxal_cli_project::PreparationEvent::Success(message) => self.success(message),
            phoxal_cli_project::PreparationEvent::Warn(message) => self.warn(message),
            phoxal_cli_project::PreparationEvent::Error(message) => self.error(message),
            phoxal_cli_project::PreparationEvent::CommandLine(line) => self.dependency(line),
            phoxal_cli_project::PreparationEvent::PhaseStarted { id, label } => {
                crate::cli::output::diagnostics::phase_started(id, label);
            }
            phoxal_cli_project::PreparationEvent::PhaseFinished {
                id,
                outcome,
                elapsed,
            } => crate::cli::output::diagnostics::phase_finished(id, outcome, elapsed),
        }
    }
}

pub(crate) struct PreparationReporter {
    ui: Ui,
    cancellation: tokio_util::sync::CancellationToken,
}

impl PreparationReporter {
    pub(crate) fn new(ui: Ui, cancellation: tokio_util::sync::CancellationToken) -> Self {
        Self { ui, cancellation }
    }
}

pub(crate) fn cancellable_preparation_reporter(
    ui: Ui,
) -> (
    std::sync::Arc<dyn phoxal_cli_project::Reporter>,
    tokio::task::JoinHandle<()>,
) {
    let cancellation = tokio_util::sync::CancellationToken::new();
    let signal = cancellation.clone();
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal.cancel();
        }
    });
    (
        std::sync::Arc::new(PreparationReporter::new(ui, cancellation)),
        signal_task,
    )
}

impl phoxal_cli_project::Reporter for PreparationReporter {
    fn report(&self, event: phoxal_cli_project::PreparationEvent) {
        self.ui.report(event);
    }

    fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
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

    fn dependency(&self, message: impl AsRef<str>) {
        let message = message.as_ref();
        if !matches!(
            try_route(DiagnosticSource::Dependency, DiagnosticLevel::Info, message),
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_cli_core::session::event::SessionEvent;

    #[test]
    fn captured_command_lines_keep_dependency_diagnostic_ownership() {
        let _guard = crate::cli::output::diagnostics::DIAGNOSTICS_TEST_LOCK.blocking_lock();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        crate::cli::output::diagnostics::install(tx);

        phoxal_cli_project::Reporter::report(
            &Ui::new(true),
            phoxal_cli_project::PreparationEvent::CommandLine(
                "cargo: compiling fixture".to_string(),
            ),
        );
        crate::cli::output::diagnostics::uninstall();

        let event = rx
            .try_recv()
            .expect("captured command output must enter session diagnostics");
        assert!(matches!(
            event,
            SessionEvent::Diagnostic {
                source: DiagnosticSource::Dependency,
                level: DiagnosticLevel::Info,
                ref message,
            } if message == "cargo: compiling fixture"
        ));
    }
}
