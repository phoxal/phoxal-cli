//! Finite command progress adapter.

use super::plain::Ui;

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
        phoxal_cli_project::Reporter::report(&self.ui, event);
    }

    fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
}
