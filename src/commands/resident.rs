use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Args;
use phoxal_cli_core::session::{CommandAction, ProjectLifecycle};

use crate::AppContext;
use crate::resident::supervisor_socket_path;
use crate::run::{RobotFeedTarget, project_router_endpoint, start_telemetry_feeds_at};
use crate::supervisor::{
    BoardBackend, ProjectLock, ProjectLockStatus, ProjectOperation, start_bus_log_subscriber,
    start_liveliness_observer,
};

#[derive(Debug, Args)]
pub struct Attach {
    #[arg(value_name = "PROJECT_OR_ENTRY")]
    target: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct Stop {
    #[arg(value_name = "PROJECT_OR_ENTRY")]
    target: Option<PathBuf>,
}

impl Attach {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let target = resolve_target(self.target.as_deref(), app.project.root())?;
        let client = connect_running(&target).await?;
        drive_tui(app, &target, client, false).await.map(|_| ())
    }
}

pub(crate) async fn drive_tui(
    app: &AppContext,
    target: &ProjectTarget,
    client: phoxal_cli_client::SupervisorClient,
    shutdown_on_quit: bool,
) -> Result<crate::session::controller::AttachmentOutcome> {
    let initial = client.snapshots().current();
    validate_entry(target, &initial.entry)?;
    let board = BoardBackend::new();
    board.replace_from_supervisor(initial.clone());
    let telemetry = crate::telemetry::TelemetryBackend::new();
    let connect = project_router_endpoint(&target.project);
    let targets = RobotFeedTarget::from_snapshot(&initial);
    let mut tasks = Vec::new();
    tasks.extend(targets.iter().map(|target| {
        start_bus_log_subscriber(
            target.scope.namespace.clone(),
            target.scope.robot_id.clone(),
            connect.clone(),
            board.clone(),
        )
    }));
    tasks.extend(targets.iter().map(|target| {
        start_liveliness_observer(
            target.scope.namespace.clone(),
            target.scope.robot_id.clone(),
            connect.clone(),
            board.clone(),
        )
    }));
    tasks.extend(start_telemetry_feeds_at(
        &targets,
        &telemetry,
        &connect,
        board.recovery_epoch_receiver(),
    ));
    if initial.execution == "simulation:webots"
        && let Some(first) = targets.first()
    {
        let (clock_rx, clock_task) = crate::supervisor::start_clock_feed(
            first.scope.namespace.clone(),
            first.scope.robot_id.clone(),
            connect.clone(),
        );
        telemetry.set_clock_feed(clock_rx);
        tasks.push(clock_task);
    }
    let mut controller = crate::session::controller::SessionController::new_attachment(
        app.output,
        if initial.execution == "simulation:webots" {
            phoxal_cli_core::session::SessionMode::Simulation
        } else {
            phoxal_cli_core::session::SessionMode::Run
        },
        &target.project,
        &initial,
    )?;
    controller.set_bus_endpoint(connect);
    let result = controller
        .drive_attachment(board, telemetry, client, shutdown_on_quit)
        .await;
    for task in tasks {
        task.abort();
    }
    result
}

impl Stop {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let target = resolve_target(self.target.as_deref(), app.project.root())?;
        let client = connect_running(&target).await?;
        validate_entry(&target, &client.snapshots().current().entry)?;
        let reply = client.command(CommandAction::Shutdown).await?;
        anyhow::ensure!(
            reply.accepted,
            "supervisor rejected shutdown: {:?}",
            reply.error
        );
        let mut snapshots = client.snapshots().subscribe();
        loop {
            let snapshot = snapshots.borrow_and_update().clone();
            if matches!(
                snapshot.lifecycle,
                ProjectLifecycle::Stopped | ProjectLifecycle::Failed
            ) {
                return if snapshot.lifecycle == ProjectLifecycle::Stopped {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!(
                        "resident supervisor failed during shutdown"
                    ))
                };
            }
            snapshots
                .changed()
                .await
                .context("resident disconnected before terminal snapshot")?;
        }
    }
}

#[derive(Debug)]
pub(crate) struct ProjectTarget {
    pub(crate) project: PathBuf,
    requested_entry: Option<PathBuf>,
}

pub(crate) fn resolve_target(explicit: Option<&Path>, fallback: &Path) -> Result<ProjectTarget> {
    let selected = explicit.unwrap_or(fallback);
    let preserve_installed_identity =
        selected == Path::new(crate::runtime_paths::ACTIVE_RUNTIME_ROOT);
    let selected = if preserve_installed_identity {
        selected.to_path_buf()
    } else {
        selected
            .canonicalize()
            .with_context(|| format!("failed to resolve {}", selected.display()))?
    };
    let (project, requested_entry) = if selected.is_file() {
        (
            selected
                .parent()
                .context("entry file has no parent project")?
                .to_path_buf(),
            Some(selected),
        )
    } else {
        let entry = phoxal_cli_core::project::resolver::discover_robot_yaml(&selected)
            .with_context(|| format!("failed to find robot.yaml from {}", selected.display()))?;
        (
            if preserve_installed_identity {
                selected
            } else {
                entry
                    .parent()
                    .context("robot.yaml has no parent project")?
                    .canonicalize()?
            },
            None,
        )
    };
    // Validate the platform path limit before inspecting ownership so attach,
    // stop, and run all report the same actionable diagnostic.
    let _ = supervisor_socket_path(&project)?;
    Ok(ProjectTarget {
        project,
        requested_entry,
    })
}

async fn connect_running(target: &ProjectTarget) -> Result<phoxal_cli_client::SupervisorClient> {
    loop {
        match ProjectLock::inspect(&target.project)? {
            ProjectLockStatus::Free => {
                bail!("project is not running: {}", target.project.display())
            }
            ProjectLockStatus::Held(identity) if identity.operation != ProjectOperation::Run => {
                bail!(
                    "project is held by `{}` (pid {}), not a resident run",
                    identity.operation,
                    identity.pid
                )
            }
            ProjectLockStatus::Held(_) => {}
        }
        let path = supervisor_socket_path(&target.project)?;
        match phoxal_cli_client::SupervisorClient::connect(&path).await {
            Ok(client) => return Ok(client),
            Err(_) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => bail!("cancelled while waiting for the resident supervisor to finish startup"),
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                }
            }
        }
    }
}

fn validate_entry(target: &ProjectTarget, running_entry: &str) -> Result<()> {
    let Some(requested) = &target.requested_entry else {
        return Ok(());
    };
    let running = Path::new(running_entry)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(running_entry));
    anyhow::ensure!(
        &running == requested,
        "entry mismatch: requested {}, but the running entry is {}",
        requested.display(),
        running.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// `attach`/`stop` are representation-agnostic: a staged runtime layout root
    /// (an extracted `build.phoxal` or `.phoxal/build/<triple>/`) resolves to
    /// itself as the project, so their socket/lock lookups reach
    /// `<layout_root>/.phoxal/supervisor.sock` - the exact path a layout run's
    /// resident binds (#936).
    #[test]
    fn resolve_target_reaches_a_layout_root_socket() -> Result<()> {
        let layout = tempfile::tempdir()?;
        fs::write(layout.path().join("robot.yaml"), "schema: robot/v0\n")?;
        fs::create_dir_all(layout.path().join("bin"))?;

        let target = resolve_target(Some(layout.path()), layout.path())?;
        assert_eq!(target.project, layout.path().canonicalize()?);
        assert!(target.requested_entry.is_none());
        assert_eq!(
            supervisor_socket_path(&target.project)?,
            target.project.join(".phoxal/supervisor.sock")
        );
        Ok(())
    }
}
