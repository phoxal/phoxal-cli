//! Building and validating a project.
//!
//! Both are the client's: the daemon never invokes Cargo, resolves a registry,
//! compiles source, or mutates a project.

use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::cli::context::AppContext;
use crate::lock::{
    ProjectLock, ProjectLockIdentity, ProjectOperation, refuse_while_execution_is_live,
};

pub(crate) struct BuildRequest {
    pub(crate) project: Option<PathBuf>,
    pub(crate) backend: phoxal_cli_project::BuildBackend,
    pub(crate) output: Option<PathBuf>,
}

pub(crate) async fn build_command(
    app: &AppContext,
    request: BuildRequest,
) -> Result<phoxal_cli_project::BuiltBundle> {
    let target =
        phoxal_cli_project::resolve_target(request.project.as_deref(), app.project.root())?;
    let _lock = ProjectLock::acquire(ProjectLockIdentity::resolve(
        &target.logical_root,
        ProjectOperation::Build,
    ))
    .context("failed to acquire the project lock for build")?;
    // Publishing replaces `.phoxal/release/` in place, so it is refused while a
    // daemon is executing out of it. The build lock does not answer this: it
    // only serializes this tool against itself.
    refuse_while_execution_is_live(&target.logical_root)?;
    let (reporter, signal_task) =
        crate::cli::output::progress::cancellable_preparation_reporter(app.ui);
    let offline = app.offline;
    let built = tokio::task::spawn_blocking(move || {
        phoxal_cli_project::build_bundle(phoxal_cli_project::BuildBundleRequest {
            target,
            backend: request.backend,
            executor: crate::pair::PairExecutors::shared(),
            output: request.output,
            publish: true,
            offline,
            reporter,
        })
    })
    .await?;
    signal_task.abort();
    built
}

pub(crate) async fn validate_command(
    app: &AppContext,
) -> Result<phoxal_cli_project::ValidationReport> {
    let target = phoxal_cli_project::resolve_target(None, app.project.root())?;
    let (reporter, signal_task) =
        crate::cli::output::progress::cancellable_preparation_reporter(app.ui);
    let offline = app.offline;
    let report = tokio::task::spawn_blocking(move || {
        phoxal_cli_project::validate(phoxal_cli_project::ValidateRequest {
            source: phoxal_cli_project::ValidationSource::Project(target),
            offline,
            reporter,
        })
    })
    .await?;
    signal_task.abort();
    report
}
