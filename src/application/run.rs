//! Runtime preparation, resident supervision, and finite host-lifecycle use
//! cases composed by the binary package.

use crate::cli::AppContext;
use anyhow::bail;
use anyhow::{Context, Result};
use phoxal_cli_core::identity::ExecutionId;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::launch_plan::RunIdentity;
use phoxal_cli_supervisor::ParticipantSpec;
use phoxal_cli_supervisor::ProjectLock;
use phoxal_cli_supervisor::ProjectLockIdentity;
use phoxal_cli_supervisor::ProjectOperation;
use phoxal_cli_supervisor::SupervisionStage;
use phoxal_cli_supervisor::SupervisorOptions;
use phoxal_cli_supervisor::SupervisorState;
use phoxal_cli_supervisor::start_supervisor_session;
use phoxal_cli_supervisor::{EmbeddedRouter, start_embedded_router, supervise_until_shutdown};
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;

pub(crate) use crate::application::readiness::{Readiness, required_readiness};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RobotFeedTarget {
    pub(crate) scope: phoxal_cli_observation::RobotScope,
}

impl RobotFeedTarget {
    fn from_plan(plan: &LaunchPlan) -> Vec<Self> {
        plan.robots
            .iter()
            .map(|robot| Self {
                scope: phoxal_cli_observation::RobotScope {
                    namespace: robot.namespace.clone(),
                    robot_id: robot.id.clone(),
                },
            })
            .collect()
    }
}

const PREPARATION_CANCEL_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_millis(25)
} else {
    Duration::from_secs(5)
};

pub(crate) fn process_state(
    state: phoxal_cli_core::runtime::ParticipantState,
) -> phoxal_cli_core::runtime::ProcessState {
    match state {
        phoxal_cli_core::runtime::ParticipantState::Starting => {
            phoxal_cli_core::runtime::ProcessState::Starting
        }
        phoxal_cli_core::runtime::ParticipantState::Ready => {
            phoxal_cli_core::runtime::ProcessState::Ready
        }
        phoxal_cli_core::runtime::ParticipantState::Degraded => {
            phoxal_cli_core::runtime::ProcessState::Degraded
        }
        phoxal_cli_core::runtime::ParticipantState::Failed => {
            phoxal_cli_core::runtime::ProcessState::Failed
        }
        phoxal_cli_core::runtime::ParticipantState::Restarting => {
            phoxal_cli_core::runtime::ProcessState::Restarting
        }
        phoxal_cli_core::runtime::ParticipantState::Stopped => {
            phoxal_cli_core::runtime::ProcessState::Stopped
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DriversMode {
    On,
    Off,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOptions {
    pub drivers: DriversMode,
    pub drivers_subset: Vec<String>,
    pub offline: bool,
}

pub(crate) struct BuildRequest {
    pub(crate) project: Option<PathBuf>,
    pub(crate) backend: phoxal_cli_project::BuildBackend,
    pub(crate) output: Option<PathBuf>,
}

#[derive(Debug)]
pub(crate) struct PreparedRun {
    pub(crate) plan: LaunchPlan,
    pub(crate) board: SupervisorState,
    pub(crate) specs: Vec<ParticipantSpec>,
    pub(crate) robot_targets: Vec<RobotFeedTarget>,
    pub(crate) router: phoxal_cli_project::PreparedRouter,
    /// The runtime layout this run was staged into. The supervisor serves
    /// declared assets from below it (organization#978).
    pub(crate) staged_root: std::path::PathBuf,
    /// Whether this robot can be driven by manual input, carried to the client
    /// on the supervisor snapshot (organization#978).
    pub(crate) manual_input: phoxal_cli_protocol::ManualInput,
}

/// Resources assembled after preparation but before the controller enters
/// supervision. Keeping this whole phase behind `drive_setup` means raw-mode
/// Ctrl-C remains polled until the supervisor loop takes ownership.
pub(crate) struct LiveRunSetup {
    pub(crate) router: EmbeddedRouter,
    pub(crate) board: SupervisorState,
    pub(crate) stages: Vec<SupervisionStage>,
    pub(crate) supervisor_options: SupervisorOptions,
    pub(crate) background_tasks: AbortTasks,
}

#[derive(Default)]
pub(crate) struct AbortTasks {
    handles: Vec<tokio::task::JoinHandle<()>>,
}

impl AbortTasks {
    pub(crate) fn push(&mut self, handle: tokio::task::JoinHandle<()>) {
        self.handles.push(handle);
    }

    pub(crate) fn extend(
        &mut self,
        handles: impl IntoIterator<Item = tokio::task::JoinHandle<()>>,
    ) {
        self.handles.extend(handles);
    }
}

impl Drop for AbortTasks {
    fn drop(&mut self) {
        for handle in &self.handles {
            handle.abort();
        }
    }
}

pub(crate) async fn run_command(
    app: &AppContext,
    requested_target: Option<&Path>,
    detach: bool,
    drivers: DriversMode,
    drivers_subset: Vec<String>,
) -> Result<()> {
    let options = RunOptions {
        drivers,
        drivers_subset,
        offline: app.offline,
    };
    let target =
        crate::application::attachment::resolve_target(requested_target, app.project.root())?;
    // SAFETY: no task that reads the project-root environment has been spawned
    // on this path; all later path helpers must agree on the selected root.
    unsafe {
        std::env::set_var(phoxal_cli_project::PROJECT_ROOT_ENV, &target.project);
    }
    if should_run_resident_in_process(
        app.output.interactive,
        detach,
        phoxal_cli_supervisor::resident::has_private_bootstrap(),
    ) {
        // `run` never owns the systemd notify socket; that is `start`'s job.
        return run_resident_supervision(app, target.project, options, None).await;
    }
    launch_client(app, target, detach).await
}

pub(crate) async fn start_command(app: &AppContext, requested_target: Option<&Path>) -> Result<()> {
    let options = RunOptions {
        drivers: DriversMode::On,
        drivers_subset: Vec::new(),
        offline: app.offline,
    };
    let target =
        crate::application::attachment::resolve_target(requested_target, app.project.root())?;
    // SAFETY: no task that reads the project-root environment has been spawned
    // on this path; later helpers and the detached child inherit this root.
    unsafe {
        std::env::set_var(phoxal_cli_project::PROJECT_ROOT_ENV, &target.project);
    }
    if phoxal_cli_supervisor::resident::has_private_bootstrap() {
        return run_resident_supervision(app, target.project, options, None).await;
    }
    if let Some(notify) = phoxal_cli_supervisor::systemd::notify::SdNotify::from_env()? {
        return run_resident_supervision(app, target.project, options, Some(notify)).await;
    }
    let (mut launched, feed, _) =
        connect_to_detached_resident_feed(&target.project, app.offline).await?;
    wait_for_startup(
        app,
        &target,
        &feed,
        &mut launched.child,
        StartupWaitMode::Detached,
    )
    .await?;
    let display = target.project.display();
    app.ui.info(format!(
        "robot instance ready; attach with `phoxal attach {display}` or stop with `phoxal stop {display}`"
    ));
    Ok(())
}

pub(crate) async fn build_command(
    app: &AppContext,
    request: BuildRequest,
) -> Result<phoxal_cli_project::BuiltBundle> {
    let target =
        phoxal_cli_project::resolve_target(request.project.as_deref(), app.project.root())?;
    // SAFETY: no task that reads the project-root environment has been spawned
    // on this command path.
    unsafe {
        std::env::set_var(phoxal_cli_project::PROJECT_ROOT_ENV, &target.logical_root);
    }
    let _lock = ProjectLock::acquire(ProjectLockIdentity::resolve(
        &target.logical_root,
        ProjectOperation::Build,
    ))
    .context("failed to acquire the project lock for build")?;
    remove_obsolete_component_state(&target.logical_root)?;
    let (reporter, signal_task) =
        crate::cli::output::progress::cancellable_preparation_reporter(app.ui);
    let built = phoxal_cli_project::build_bundle(phoxal_cli_project::BuildBundleRequest {
        target,
        backend: request.backend,
        output: request.output,
        publish: true,
        offline: app.offline,
        reporter,
    })
    .await;
    signal_task.abort();
    built
}

pub(crate) async fn validate_command(
    app: &AppContext,
) -> Result<phoxal_cli_project::ValidationReport> {
    let target = phoxal_cli_project::resolve_target(None, app.project.root())?;
    clean_obsolete_component_state_for_validation(&target.logical_root)?;
    let (reporter, signal_task) =
        crate::cli::output::progress::cancellable_preparation_reporter(app.ui);
    let report = phoxal_cli_project::validate(phoxal_cli_project::ValidateRequest {
        source: phoxal_cli_project::ValidationSource::Project(target),
        offline: app.offline,
        reporter,
    })
    .await;
    signal_task.abort();
    report
}

/// Remove retired generated state after the caller holds a project-operation
/// lock. Resolution may populate the verified registry cache, so command
/// adapters own this migration rather than library resolution.
fn remove_obsolete_component_state(project_root: &std::path::Path) -> Result<()> {
    for obsolete in [
        project_root.join(".phoxal/resolve"),
        project_root.join(".phoxal/cache/build-metadata"),
    ] {
        if !obsolete.exists() {
            continue;
        }
        std::fs::remove_dir_all(&obsolete).with_context(|| {
            format!(
                "failed to remove obsolete generated state {}",
                obsolete.display()
            )
        })?;
    }
    Ok(())
}

fn obsolete_component_state_exists(project_root: &std::path::Path) -> bool {
    [".phoxal/resolve", ".phoxal/cache/build-metadata"]
        .iter()
        .any(|relative| project_root.join(relative).exists())
}

/// Validation is read-only after the one-time migration. Only the migration
/// needs exclusive ownership, so a live robot may still be validated once its
/// retired generated directories are gone.
fn clean_obsolete_component_state_for_validation(project_root: &std::path::Path) -> Result<()> {
    if !obsolete_component_state_exists(project_root) {
        return Ok(());
    }
    let _lock = ProjectLock::acquire(ProjectLockIdentity::resolve(
        project_root,
        ProjectOperation::Validate,
    ))
    .context("validation can run during a robot execution, but legacy generated state must be cleaned first; stop the active operation or remove only the retired .phoxal/resolve and .phoxal/cache/build-metadata directories")?;
    remove_obsolete_component_state(project_root)
}

#[cfg(test)]
mod obsolete_component_state_cleanup_tests {
    use super::*;

    #[test]
    fn cleanup_removes_all_obsolete_component_state() -> Result<()> {
        let project = tempfile::tempdir()?;
        let obsolete = project.path().join(".phoxal/resolve");
        let obsolete_metadata = project.path().join(".phoxal/cache/build-metadata");
        let neighbor = project.path().join(".phoxal/cache/registry");
        std::fs::create_dir_all(&obsolete)?;
        std::fs::create_dir_all(&obsolete_metadata)?;
        std::fs::create_dir_all(&neighbor)?;
        std::fs::write(obsolete.join("old"), "obsolete")?;
        std::fs::write(obsolete_metadata.join("old"), "obsolete")?;
        std::fs::write(neighbor.join("keep"), "cached")?;

        remove_obsolete_component_state(project.path())?;

        assert!(!obsolete.exists());
        assert!(!obsolete_metadata.exists());
        assert!(!obsolete_component_state_exists(project.path()));
        assert_eq!(std::fs::read_to_string(neighbor.join("keep"))?, "cached");
        Ok(())
    }

    #[test]
    fn validation_migration_does_not_lock_a_running_project_without_obsolete_state() -> Result<()> {
        let project = tempfile::tempdir()?;
        let _run = ProjectLock::acquire(ProjectLockIdentity::resolve(
            project.path(),
            ProjectOperation::Run,
        ))?;
        clean_obsolete_component_state_for_validation(project.path())?;

        std::fs::create_dir_all(project.path().join(".phoxal/resolve"))?;
        let error = clean_obsolete_component_state_for_validation(project.path())
            .expect_err("the one-time cleanup must not race a live run");
        assert!(
            error.to_string().contains("legacy generated state"),
            "{error:#}"
        );
        assert!(
            error.to_string().contains("stop the active operation"),
            "{error:#}"
        );
        Ok(())
    }
}

async fn launch_client(
    app: &AppContext,
    target: crate::application::attachment::ProjectTarget,
    detach: bool,
) -> Result<()> {
    if detach {
        let (mut launched, feed, _) =
            connect_to_detached_resident_feed(&target.project, app.offline).await?;
        return wait_for_startup(
            app,
            &target,
            &feed,
            &mut launched.child,
            StartupWaitMode::Detached,
        )
        .await;
    }
    launch_foreground_client(app, target).await
}

pub(crate) async fn launch_foreground_client(
    app: &AppContext,
    target: crate::application::attachment::ProjectTarget,
) -> Result<()> {
    let (mut launched, feed, socket) =
        connect_to_detached_resident_feed(&target.project, app.offline).await?;
    wait_for_startup(
        app,
        &target,
        &feed,
        &mut launched.child,
        StartupWaitMode::ForegroundOwned,
    )
    .await?;
    let commands = phoxal_cli_client::SupervisorCommands::connect(socket)
        .await
        .map_err(|error| resident_connect_failure(&target.project, &feed.current(), error))?;
    let result = crate::application::attachment::run(app, &target, feed, commands, true).await;
    match result? {
        phoxal_cli_ui::AttachmentOutcome::ResidentFailed { reason } => {
            let _status = tokio::task::spawn_blocking(move || launched.child.wait()).await??;
            anyhow::bail!(crate::application::attachment::resident_failure_message(
                &target.project,
                reason.as_deref()
            ))
        }
        phoxal_cli_ui::AttachmentOutcome::ResidentStopped => {
            let status = tokio::task::spawn_blocking(move || launched.child.wait()).await??;
            anyhow::ensure!(status.success(), "resident supervisor exited with {status}");
            Ok(())
        }
        phoxal_cli_ui::AttachmentOutcome::Detached => Ok(()),
    }
}

/// Prefer the resident's own terminal-failure reason (if the feed - which
/// connects first, see `connect_to_detached_resident_feed` - already
/// observed one) over a raw transport error from this second, independent
/// connection attempt, which merely raced the resident's exit and lost.
fn resident_connect_failure(
    project: &Path,
    snapshot: &phoxal_cli_protocol::SupervisorSnapshotV0,
    error: anyhow::Error,
) -> anyhow::Error {
    if snapshot.lifecycle == phoxal_cli_core::runtime::ProjectLifecycle::Failed {
        anyhow::anyhow!(crate::application::attachment::resident_failure_message(
            project,
            snapshot.failure.as_deref()
        ))
    } else {
        error.context(crate::application::attachment::resident_failure_message(
            project, None,
        ))
    }
}

pub(crate) async fn connect_to_detached_resident_feed(
    project: &Path,
    offline: bool,
) -> Result<(
    phoxal_cli_supervisor::resident::LaunchedResident,
    phoxal_cli_client::SupervisorFeed,
    PathBuf,
)> {
    let launched = phoxal_cli_supervisor::resident::launch_detached(offline)?;
    let generation = match &launched.result {
        phoxal_cli_protocol::BootstrapResult::Bound {
            supervisor_generation,
            ..
        } => *supervisor_generation,
        phoxal_cli_protocol::BootstrapResult::Rejected { error } => {
            bail!("{error}; use `phoxal attach` or `phoxal stop` if another run owns the project")
        }
    };
    let socket = phoxal_cli_supervisor::resident::supervisor_socket_path(project)?;
    // This connect races the resident's own startup; if it fails before any
    // client ever attaches, the fallback shape below (no reason known yet)
    // is the best a client can do - `resident_failure_message` with
    // `reason: None` produces exactly that "may have already failed; see
    // the log" pointer.
    let feed = phoxal_cli_client::SupervisorFeed::connect(socket.clone())
        .await
        .with_context(|| crate::application::attachment::resident_failure_message(project, None))?;
    anyhow::ensure!(
        feed.current().supervisor_generation == generation,
        "resident generation did not match private bootstrap; see {} for the exact error",
        phoxal_cli_core::runtime::paths::RuntimePaths::for_root(project)
            .supervisor_log()
            .display()
    );
    Ok((launched, feed, socket))
}

/// Drive a resident supervisor in this process: acquire the project lock, bind
/// the resident socket, prepare and supervise the graph, and return when it stops
/// or a shutdown signal arrives. `notify` is `Some` only for `phoxal start` under
/// systemd - the in-process foreground resident that owns `sd_notify`; `run` and
/// the detached-child `start` always pass `None`. A private-bootstrap failure is
/// reported back to the launcher so it never hangs waiting for the bootstrap
/// frame (#936).
pub(crate) async fn run_resident_supervision(
    app: &AppContext,
    project_root: PathBuf,
    options: RunOptions,
    notify: Option<phoxal_cli_supervisor::systemd::notify::SdNotify>,
) -> Result<()> {
    run_resident_supervision_mode(app, project_root, ResidentMode::Run(options), notify).await
}

pub(crate) async fn run_webots_resident_supervision(
    app: &AppContext,
    project_root: PathBuf,
    options: crate::application::simulation::SimulateOptions,
) -> Result<()> {
    run_resident_supervision_mode(app, project_root, ResidentMode::Webots(options), None).await
}

enum ResidentMode {
    Run(RunOptions),
    Webots(crate::application::simulation::SimulateOptions),
}

async fn run_resident_supervision_mode(
    app: &AppContext,
    project_root: PathBuf,
    mode: ResidentMode,
    notify: Option<phoxal_cli_supervisor::systemd::notify::SdNotify>,
) -> Result<()> {
    let execution = phoxal_cli_supervisor::resident::private_bootstrap_execution()?;
    match resident_supervision_inner(app, project_root, mode, execution, notify).await {
        Ok(()) => Ok(()),
        Err(error) => {
            if execution.is_some() {
                let _ = phoxal_cli_supervisor::resident::report_private_bootstrap(
                    &phoxal_cli_protocol::BootstrapResult::Rejected {
                        error: format!("{error:#}"),
                    },
                );
            }
            Err(error)
        }
    }
}

async fn resident_supervision_inner(
    app: &AppContext,
    project_root: PathBuf,
    mode: ResidentMode,
    bootstrap_execution: Option<ExecutionId>,
    notify: Option<phoxal_cli_supervisor::systemd::notify::SdNotify>,
) -> Result<()> {
    // One supervised run, one execution identity (#952 section B). A privately
    // launched resident adopts the one its launcher minted; a foreground run
    // mints its own. Recording it on the project lock is what lets an ad hoc
    // inspector join the running execution rather than an empty root of its own.
    let run =
        phoxal_cli_core::project::launch_plan::RunIdentity::mint_or_adopt(bootstrap_execution);
    let identity = ProjectLockIdentity::resolve(&project_root, ProjectOperation::Run)
        .in_execution(run.execution());
    let _lock = ProjectLock::acquire(identity)?;
    remove_obsolete_component_state(&project_root)?;
    let runtime_target = phoxal_cli_project::resolve_target(Some(&project_root), &project_root)?;
    let board = SupervisorState::new();
    board.configure(
        project_root.display().to_string(),
        "resolving",
        run.execution(),
        runtime_target.zenoh_endpoint.clone(),
    );
    board.plan_startup_steps();
    board.step_detail(
        phoxal_cli_core::runtime::StartupStepKind::Project,
        "robot.yaml",
    );
    let mut console_task =
        if bootstrap_execution.is_none() && notify.is_none() && !app.output.interactive {
            Some(spawn_board_console(app, &project_root, board.clone()))
        } else {
            None
        };
    let token = tokio_util::sync::CancellationToken::new();
    let (action_tx, action_rx) = mpsc::channel(16);
    let socket = phoxal_cli_supervisor::resident::ResidentSocket::bind(
        &project_root,
        board.clone(),
        action_tx.clone(),
        token.clone(),
    )?;
    if bootstrap_execution.is_some() {
        phoxal_cli_supervisor::resident::report_private_bootstrap(
            &phoxal_cli_protocol::BootstrapResult::Bound {
                supervisor_generation: board.supervisor_snapshot().supervisor_generation,
                execution: run.execution(),
            },
        )?;
    }

    let prepare_board = board.clone();
    let prepare_token = token.clone();
    let prepare_ui = app.ui;
    let prepare_output = app.output;
    let mut preparation = tokio::spawn(async move {
        match mode {
            ResidentMode::Run(options) => {
                let prepare_options = options.clone();
                let prepared = prepare_run(
                    runtime_target,
                    prepare_options,
                    prepare_ui,
                    prepare_board.clone(),
                    run,
                    prepare_token.clone(),
                )
                .await?;
                prepare_board.step_done(phoxal_cli_core::runtime::StartupStepKind::PrepareRuntime);
                live_run_setup(
                    prepared,
                    prepare_ui,
                    prepare_output,
                    prepare_token,
                    Some((action_tx, action_rx)),
                    run,
                )
                .await
                .map(|setup| ResidentSetup {
                    router: setup.router,
                    board: setup.board,
                    stages: setup.stages,
                    supervisor_options: setup.supervisor_options,
                    background_tasks: setup.background_tasks,
                })
            }
            ResidentMode::Webots(options) => {
                prepare_board.step_detail(
                    phoxal_cli_core::runtime::StartupStepKind::Project,
                    format!("robot.yaml · framework {}", options.train),
                );
                prepare_board.step_done(phoxal_cli_core::runtime::StartupStepKind::Project);
                prepare_board
                    .step_active(phoxal_cli_core::runtime::StartupStepKind::PrepareRuntime);
                prepare_board.step_detail(
                    phoxal_cli_core::runtime::StartupStepKind::PrepareRuntime,
                    "checking Webots installation",
                );
                let webots_host = (|| -> Result<_> {
                    phoxal_cli_project::host::doctor::preflight()
                        .map_err(|error| anyhow::anyhow!("{error}"))
                        .context(
                            "Webots preflight failed; live simulate cannot launch the simulator",
                        )?;
                    let executable = phoxal_cli_project::host::doctor::webots_executable_path()
                        .map_err(|error| anyhow::anyhow!("{error}"))?;
                    let home = phoxal_cli_project::host::doctor::webots_home_path()
                        .map_err(|error| anyhow::anyhow!("{error}"))?;
                    Ok((executable, home))
                })();
                let (executable, home) = match webots_host {
                    Ok(host) => host,
                    Err(error) => {
                        prepare_board.step_failed(
                            phoxal_cli_core::runtime::StartupStepKind::PrepareRuntime,
                            format!("{error:#}"),
                        );
                        return Err(error);
                    }
                };
                let offline = options.offline;
                let sim = phoxal_cli_project::prepare_simulation(
                    phoxal_cli_project::PrepareSimulationRequest {
                        target: runtime_target,
                        run,
                        world: options.world,
                        offline,
                        webots: phoxal_cli_project::WebotsHost {
                            executable,
                            home: Some(home),
                        },
                        reporter: Arc::new(crate::cli::output::progress::BoardReporter::new(
                            prepare_ui,
                            prepare_token.clone(),
                            prepare_board.clone(),
                        )),
                    },
                )
                .await?;
                prepare_board.step_done(phoxal_cli_core::runtime::StartupStepKind::PrepareRuntime);
                crate::application::live_simulate_setup(
                    prepare_ui,
                    sim,
                    prepare_board,
                    prepare_token,
                    prepare_output,
                    Some((action_tx, action_rx)),
                    run,
                )
                .await
                .map(|setup| ResidentSetup {
                    router: setup.router,
                    board: setup.board,
                    stages: setup.stages,
                    supervisor_options: setup.supervisor_options,
                    background_tasks: setup.background_tasks,
                })
            }
        }
    });
    let signal_token = token.clone();
    let signal_task = tokio::spawn(async move {
        if let Err(error) = resident_shutdown_signal().await {
            tracing::warn!("resident preparation signal watcher failed: {error:#}");
        }
        signal_token.cancel();
    });
    let prepared_result = tokio::select! {
        result = &mut preparation => Some(result?),
        () = token.cancelled() => None,
    };
    signal_task.abort();
    let Some(prepared_result) = prepared_result else {
        finish_cancelled_preparation(&board, &mut preparation).await;
        socket.close().await;
        let _ = join_board_console(&mut console_task).await;
        return Ok(());
    };
    let prepared = match prepared_result {
        Ok(prepared) => prepared,
        Err(error) => {
            if token.is_cancelled() {
                board.set_lifecycle(phoxal_cli_core::runtime::ProjectLifecycle::Stopped);
                socket.close().await;
                let _ = join_board_console(&mut console_task).await;
                return Ok(());
            }
            board.fail_active_step(format!("{error:#}"));
            board.fail(&format!("{error:#}"));
            socket.close().await;
            let console_reported_failure = join_board_console(&mut console_task).await;
            if console_reported_failure {
                return Err(crate::cli::ReportedExit(1).into());
            }
            return Err(error);
        }
    };
    let ResidentSetup {
        router,
        board,
        stages,
        supervisor_options,
        mut background_tasks,
    } = prepared;
    // Under systemd the foreground resident owns readiness/watchdog signalling:
    // once the supervised graph reaches required readiness send `READY=1`, then
    // ping `WATCHDOG=1` on a timer. The task is a background task, so it is
    // aborted when supervision ends alongside the others.
    if let Some(notify) = notify {
        background_tasks.push(spawn_readiness_notify(notify, board.clone()));
    }
    // The router is this process's own state, so supervision is just the graph
    // now: no outer loop watching a router child, and no recovery epoch to
    // rebuild the graph after one exits (organization#978). Hold the router
    // across supervision and close it afterwards, so participants lose their
    // links to a router that is already done with them rather than mid-teardown.
    let mut supervise = tokio::spawn(supervise_until_shutdown(
        stages,
        board.clone(),
        supervisor_options,
    ));
    let outcome = tokio::select! {
        result = &mut supervise => result?,
        signal = resident_shutdown_signal() => {
            signal?;
            token.cancel();
            supervise.await?
        }
    };
    if let Err(error) = router.close().await {
        tracing::warn!("{error:#}");
    }
    // `SupervisorState::fail` is first-cause-wins on its own (it only ever
    // sets `failure` from `None`), so this unconditionally records the
    // outcome without checking the current lifecycle first - the store owns
    // that rule now, not this call site.
    if let Err(error) = &outcome {
        board.fail(&format!("{error:#}"));
    }
    drop(background_tasks);
    socket.close().await;
    let console_reported_failure = join_board_console(&mut console_task).await;
    match outcome {
        Err(_) if console_reported_failure => Err(crate::cli::ReportedExit(1).into()),
        outcome => outcome,
    }
}

struct BoardConsole {
    task: tokio::task::JoinHandle<()>,
    reported_failure: Arc<AtomicBool>,
}

fn spawn_board_console(app: &AppContext, project: &Path, board: SupervisorState) -> BoardConsole {
    let mut presenter =
        crate::cli::output::welcome::presenter(false, app.output.theme, app.ui, project);
    let project = project.to_path_buf();
    let reported_failure = Arc::new(AtomicBool::new(false));
    let task_reported_failure = Arc::clone(&reported_failure);
    let task = tokio::spawn(async move {
        let mut snapshots = board.subscribe();
        loop {
            let snapshot = snapshots.borrow_and_update().clone();
            presenter.snapshot(&snapshot);
            match required_readiness(&snapshot) {
                Readiness::Ready => {
                    presenter.ready();
                    return;
                }
                Readiness::Failed(_) => {
                    let log = phoxal_cli_core::runtime::paths::RuntimePaths::for_root(&project)
                        .supervisor_log();
                    let reason = crate::application::readiness::failure_reason(&snapshot);
                    presenter.failed(reason.as_deref(), &log);
                    task_reported_failure.store(true, Ordering::Release);
                    return;
                }
                Readiness::Pending
                    if snapshot.lifecycle
                        == phoxal_cli_core::runtime::ProjectLifecycle::Stopped =>
                {
                    return;
                }
                Readiness::Pending => {}
            }
            if snapshots.changed().await.is_err() {
                return;
            }
        }
    });
    BoardConsole {
        task,
        reported_failure,
    }
}

async fn join_board_console(console: &mut Option<BoardConsole>) -> bool {
    let Some(mut console) = console.take() else {
        return false;
    };
    if tokio::time::timeout(Duration::from_secs(1), &mut console.task)
        .await
        .is_err()
    {
        console.task.abort();
        let _ = console.task.await;
    }
    console.reported_failure.load(Ordering::Acquire)
}

async fn finish_cancelled_preparation<T>(
    board: &SupervisorState,
    preparation: &mut tokio::task::JoinHandle<T>,
) {
    if tokio::time::timeout(PREPARATION_CANCEL_TIMEOUT, &mut *preparation)
        .await
        .is_err()
    {
        preparation.abort();
        let _ = preparation.await;
    }
    board.set_lifecycle(phoxal_cli_core::runtime::ProjectLifecycle::Stopped);
}

struct ResidentSetup {
    router: EmbeddedRouter,
    board: SupervisorState,
    stages: Vec<SupervisionStage>,
    supervisor_options: SupervisorOptions,
    background_tasks: AbortTasks,
}

/// Wait for the supervised graph to reach required readiness on the board, send
/// systemd `READY=1`, then send `WATCHDOG=1` at the notify socket's cadence until
/// the task is aborted. A graph that fails startup never signals ready - systemd
/// times the start out and marks the unit failed, which is the correct outcome.
fn spawn_readiness_notify(
    notify: phoxal_cli_supervisor::systemd::notify::SdNotify,
    board: SupervisorState,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut snapshots = board.subscribe();
        loop {
            match required_readiness(&snapshots.borrow_and_update().clone()) {
                Readiness::Ready => break,
                Readiness::Failed(_) => return,
                Readiness::Pending => {}
            }
            if snapshots.changed().await.is_err() {
                return;
            }
        }
        if notify.notify_ready().is_err() {
            return;
        }
        let Some(interval) = notify.watchdog_interval() else {
            return;
        };
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if notify.notify_watchdog().is_err() {
                return;
            }
        }
    })
}

/// Prepare a run without classifying "source versus compiled" beyond routing to
/// the right staging step (#936): a buildable source root refreshes staging and
/// runs it; a staged runtime layout runs in place; anything else is a precise
/// error. Both paths end at the same execution: the loader constructs the plan
/// from the staged layout.
async fn prepare_run(
    target: phoxal_cli_core::runtime::RuntimeTarget,
    options: RunOptions,
    ui: crate::cli::Ui,
    board: SupervisorState,
    run: RunIdentity,
    cancellation: tokio_util::sync::CancellationToken,
) -> Result<PreparedRun> {
    let prepared = phoxal_cli_project::prepare_run(phoxal_cli_project::PrepareRunRequest {
        target,
        run,
        drivers: phoxal_cli_project::DriverRequest {
            mode: match options.drivers {
                DriversMode::On => phoxal_cli_project::DriverMode::On,
                DriversMode::Off => phoxal_cli_project::DriverMode::Off,
            },
            subset: options.drivers_subset,
        },
        offline: options.offline,
        reporter: Arc::new(crate::cli::output::progress::BoardReporter::new(
            ui,
            cancellation,
            board.clone(),
        )),
    })
    .await?;
    board.configure(
        prepared.target.logical_root.display().to_string(),
        prepared.train.clone(),
        run.execution(),
        prepared.router.endpoint.clone(),
    );
    for participant in &prepared.participants {
        let state = process_state(participant.initial_state);
        board.upsert_process(
            participant.key.clone(),
            participant.kind,
            state,
            participant.startup_requirement,
        );
        if participant.initial_state != phoxal_cli_core::runtime::ParticipantState::Starting
            || participant.note.is_some()
        {
            board.set_state(participant.key.clone(), state, participant.note.clone());
        }
    }
    let specs = prepared
        .participants
        .iter()
        .filter_map(|participant| participant.launch.clone())
        .collect();
    Ok(PreparedRun {
        robot_targets: RobotFeedTarget::from_plan(&prepared.plan),
        plan: prepared.plan,
        board,
        specs,
        router: prepared.router,
        staged_root: prepared.staged_root,
        manual_input: prepared.manual_input,
    })
}

pub(crate) fn report_launch_commands(
    plan: &LaunchPlan,
    specs: &[ParticipantSpec],
    ui: &crate::cli::Ui,
) -> Result<()> {
    let executions = plan
        .robots
        .iter()
        .flat_map(|robot| &robot.participants)
        .map(|participant| {
            (
                participant.launch.participant_id.as_str(),
                &participant.execution,
            )
        })
        .collect::<BTreeMap<_, _>>();
    ui.info("resolved launch participants:");
    for spec in specs {
        let kind = launch_kind_label(executions.get(spec.id.as_str()).copied());
        ui.info(format!(
            "  - {} ({kind}) -> {}",
            spec.id,
            spec.launch_command().command_line
        ));
    }
    ui.info(
        "motion guarantees: e-stop, source freshness, finite values, and robot-authored limits; autonomous motion also requires fresh typed safety constraints",
    );
    Ok(())
}

fn launch_kind_label(
    execution: Option<&phoxal_cli_core::project::launch_plan::ParticipantExecution>,
) -> &'static str {
    match execution {
        // A CLI-managed host process (the Webots binary) has no plan
        // execution: it is launched by the resident, not resolved from `bin/`.
        None => "host",
        Some(phoxal_cli_core::project::launch_plan::ParticipantExecution::Brain { .. }) => "brain",
        Some(phoxal_cli_core::project::launch_plan::ParticipantExecution::OfficialArtifact {
            ..
        }) => "official",
        Some(phoxal_cli_core::project::launch_plan::ParticipantExecution::UserService {
            ..
        }) => "user-service",
        Some(phoxal_cli_core::project::launch_plan::ParticipantExecution::ComponentDriver {
            ..
        }) => "driver",
    }
}

#[cfg(test)]
mod launch_kind_tests {
    use super::launch_kind_label;
    use phoxal_cli_core::project::launch_plan::ParticipantExecution;

    #[test]
    fn launch_kind_labels_cover_every_execution_variant_and_host_processes() {
        let binary_name = || "fixture".to_string();
        assert_eq!(launch_kind_label(None), "host");
        assert_eq!(
            launch_kind_label(Some(&ParticipantExecution::OfficialArtifact {
                binary_name: binary_name()
            })),
            "official"
        );
        assert_eq!(
            launch_kind_label(Some(&ParticipantExecution::UserService {
                binary_name: binary_name()
            })),
            "user-service"
        );
        assert_eq!(
            launch_kind_label(Some(&ParticipantExecution::ComponentDriver {
                binary_name: binary_name()
            })),
            "driver"
        );
    }
}

/// Keep private/headless residents in process; interactive foreground clients
/// launch a detached resident and attach to its socket.
pub(crate) const fn should_run_resident_in_process(
    interactive: bool,
    detach: bool,
    private_bootstrap: bool,
) -> bool {
    private_bootstrap || (!interactive && !detach)
}

async fn resident_shutdown_signal() -> Result<()> {
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    loop {
        tokio::select! {
            _ = interrupt.recv() => return Ok(()),
            _ = terminate.recv() => return Ok(()),
            _ = hangup.recv() => {
                tracing::debug!("ignored SIGHUP in resident supervisor");
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartupWaitMode {
    ForegroundOwned,
    Detached,
}

impl StartupWaitMode {
    const fn interactive(self, stderr_is_terminal: bool) -> bool {
        matches!(self, Self::ForegroundOwned) && stderr_is_terminal
    }

    fn deadline(self, now: tokio::time::Instant) -> Option<tokio::time::Instant> {
        matches!(self, Self::Detached).then(|| now + Duration::from_secs(5 * 60))
    }

    const fn leaves_resident_on_cancel(self) -> bool {
        matches!(self, Self::Detached)
    }
}

pub(crate) async fn wait_for_startup(
    app: &AppContext,
    target: &crate::application::attachment::ProjectTarget,
    feed: &phoxal_cli_client::SupervisorFeed,
    child: &mut std::process::Child,
    mode: StartupWaitMode,
) -> Result<()> {
    use crate::application::readiness::StartupWait;

    let interactive = mode.interactive(app.output.interactive);
    let mut presenter = crate::cli::output::welcome::presenter(
        interactive,
        app.output.theme,
        app.ui,
        &target.project,
    );
    let deadline = mode.deadline(tokio::time::Instant::now());
    let outcome =
        crate::application::readiness::wait(feed, Some(child), deadline, presenter.as_mut())
            .await?;
    let log =
        phoxal_cli_core::runtime::paths::RuntimePaths::for_root(&target.project).supervisor_log();
    match outcome {
        StartupWait::Ready => {
            let final_snapshot = feed.current();
            match required_readiness(&final_snapshot) {
                Readiness::Ready => presenter.ready(),
                Readiness::Failed(_) => {
                    let reason = crate::application::readiness::failure_reason(&final_snapshot);
                    presenter.failed(reason.as_deref(), &log);
                    return Err(crate::cli::ReportedExit(1).into());
                }
                Readiness::Pending => {
                    presenter.failed(
                        Some("resident left readiness before dashboard handoff"),
                        &log,
                    );
                    return Err(crate::cli::ReportedExit(1).into());
                }
            }
        }
        StartupWait::Failed { reason } => {
            presenter.failed(reason.as_deref(), &log);
            return Err(crate::cli::ReportedExit(1).into());
        }
        StartupWait::ChildExited { status } => {
            presenter.failed(
                Some(&format!("resident exited before readiness with {status}")),
                &log,
            );
            return Err(crate::cli::ReportedExit(1).into());
        }
        StartupWait::FeedLost => {
            presenter.failed(Some("resident disconnected before readiness"), &log);
            return Err(crate::cli::ReportedExit(1).into());
        }
        StartupWait::DeadlineExceeded => {
            presenter.failed(
                Some("timed out waiting for resident startup readiness"),
                &log,
            );
            return Err(crate::cli::ReportedExit(1).into());
        }
        StartupWait::Cancelled => {
            if !mode.leaves_resident_on_cancel() {
                presenter.cancelled();
            }
            drop(presenter);
            return cancel_startup_wait(app, target, child, mode).await;
        }
    }
    drop(presenter);
    Ok(())
}

async fn cancel_startup_wait(
    app: &AppContext,
    target: &crate::application::attachment::ProjectTarget,
    child: &mut std::process::Child,
    mode: StartupWaitMode,
) -> Result<()> {
    if mode.leaves_resident_on_cancel() {
        app.ui.info(format!(
            "resident continues; attach with 'phoxal attach {}', stop with 'phoxal stop {}'",
            target.project.display(),
            target.project.display()
        ));
        return Err(crate::cli::ReportedExit(130).into());
    }

    let socket = phoxal_cli_supervisor::resident::supervisor_socket_path(&target.project)?;
    if let Ok(commands) = phoxal_cli_client::SupervisorCommands::connect(socket).await {
        let _ = commands
            .command(phoxal_cli_protocol::CommandAction::Shutdown)
            .await;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if child.try_wait()?.is_some() {
            return Err(crate::cli::ReportedExit(130).into());
        }
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = tokio::time::sleep_until(deadline) => break,
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
    }
    if let Err(error) = phoxal_cli_supervisor::resident::force_stop(&target.runtime_target()).await
    {
        app.ui.warn(format!(
            "forced shutdown could not be confirmed: {error:#}; check with `phoxal status {}`",
            target.project.display()
        ));
    }
    Err(crate::cli::ReportedExit(130).into())
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn live_run_setup(
    prepared: PreparedRun,
    ui: crate::cli::Ui,
    output: crate::cli::output::OutputContext,
    token: tokio_util::sync::CancellationToken,
    action_channel: Option<(
        mpsc::Sender<phoxal_cli_supervisor::SupervisorAction>,
        mpsc::Receiver<phoxal_cli_supervisor::SupervisorAction>,
    )>,
    run: RunIdentity,
) -> Result<LiveRunSetup> {
    let connect = prepared.router.endpoint.clone();
    prepared
        .board
        .step_active(phoxal_cli_core::runtime::StartupStepKind::Infrastructure);
    prepared.board.step_detail(
        phoxal_cli_core::runtime::StartupStepKind::Infrastructure,
        "starting router",
    );
    let router = match start_embedded_router(
        connect.clone(),
        prepared.router.config.as_deref(),
        prepared.board.clone(),
    )
    .await
    {
        Ok(router) => router,
        Err(error) => {
            prepared.board.step_failed(
                phoxal_cli_core::runtime::StartupStepKind::Infrastructure,
                format!("{error:#}"),
            );
            return Err(error);
        }
    };
    prepared.board.step_detail(
        phoxal_cli_core::runtime::StartupStepKind::Infrastructure,
        format!("router {connect}"),
    );
    // The endpoint is a live fact clients and Webots need. The router itself is
    // no longer a board row: it is this process, so a separate "ready" process
    // state for it could only ever restate that the supervisor is running.
    prepared.board.set_router_endpoint(connect.clone());
    prepared
        .board
        .set_manual_input(prepared.manual_input.clone());
    ui.info(format!(
        "launch plan resolved: {} robot(s)",
        prepared.plan.robots.len()
    ));
    ui.info(format!("router ready on {connect}"));
    report_launch_commands(&prepared.plan, &prepared.specs, &ui)?;

    let execution = run.execution();
    // The supervisor stages this root, so it is the authority for it. Discovery
    // failing is not fatal: a bundle may legitimately declare no assets, and a
    // malformed tree should not stop a robot from running - it makes asset
    // queries answer `Missing` instead.
    let assets = phoxal_model::AssetResolver::discover(
        prepared
            .staged_root
            .join(phoxal_cli_core::project::layout::ASSETS_DIR),
    )
    .unwrap_or_else(|error| {
        tracing::warn!("serving no declared assets: {error}");
        phoxal_model::AssetResolver::default()
    });
    let mut background_tasks = AbortTasks::default();
    background_tasks.extend(prepared.robot_targets.iter().map(|target| {
        start_supervisor_session(
            target.scope.namespace.clone(),
            target.scope.robot_id.clone(),
            connect.clone(),
            execution,
            prepared.board.clone(),
            assets.clone(),
        )
    }));

    let (_action_tx, action_rx) = action_channel.unwrap_or_else(|| mpsc::channel(16));

    let stages = phoxal_cli_supervisor::stages_for_run(
        prepared.specs,
        output.wait_budget(super::RUN_STAGE_READY_TIMEOUT),
    );
    let board = prepared.board;
    let supervisor_options = SupervisorOptions {
        action_rx: Some(phoxal_cli_supervisor::SupervisorActionReceiver::new(
            action_rx,
        )),
        token,
        publishes_running_on_startup_complete: true,
        ..SupervisorOptions::default()
    };

    Ok(LiveRunSetup {
        router,
        board,
        stages,
        supervisor_options,
        background_tasks,
    })
}

#[cfg(test)]
mod resident_connect_failure_tests {
    use super::resident_connect_failure;
    use phoxal_cli_core::runtime::ProjectLifecycle;
    use phoxal_cli_protocol::SupervisorSnapshotV0;
    use std::path::Path;

    #[test]
    fn a_failed_snapshot_reason_wins_over_the_raw_transport_error() {
        let snapshot = SupervisorSnapshotV0 {
            lifecycle: ProjectLifecycle::Failed,
            failure: Some("catalog train floor not supported".to_string()),
            ..SupervisorSnapshotV0::default()
        };
        let error = resident_connect_failure(
            Path::new("/tmp/project"),
            &snapshot,
            anyhow::anyhow!("connection refused"),
        );
        assert_eq!(error.to_string(), "catalog train floor not supported");
    }

    #[test]
    fn a_non_failed_snapshot_keeps_the_transport_error_as_context() {
        let snapshot = SupervisorSnapshotV0 {
            lifecycle: ProjectLifecycle::Starting,
            ..SupervisorSnapshotV0::default()
        };
        let error = resident_connect_failure(
            Path::new("/tmp/project"),
            &snapshot,
            anyhow::anyhow!("connection refused"),
        );
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("resident supervisor failed; see"),
            "{rendered}"
        );
        assert!(rendered.contains("connection refused"), "{rendered}");
    }
}

#[cfg(test)]
mod resident_role_tests {
    use super::should_run_resident_in_process;

    #[test]
    fn noninteractive_detach_still_uses_the_launcher() {
        assert!(!should_run_resident_in_process(false, true, false));
        assert!(should_run_resident_in_process(false, false, false));
        assert!(should_run_resident_in_process(false, true, true));
        assert!(!should_run_resident_in_process(true, true, false));
    }
}

/// The readiness decision `run -d`, interactive `start`, and the systemd
/// readiness-notify task all share (#936): it is what decides when to return
/// after a detached launch and when the foreground resident sends `READY=1`.
#[cfg(test)]
mod readiness_tests {
    use super::{Readiness, StartupWaitMode, required_readiness};
    use phoxal_cli_core::runtime::{
        ParticipantKind, ProcessKey, ProcessState, ProjectLifecycle, StartupRequirement,
    };
    use phoxal_cli_supervisor::SupervisorState;

    #[test]
    fn readiness_tracks_lifecycle_and_required_failures() {
        let board = SupervisorState::new();
        board.configure(
            "/tmp/project".to_string(),
            "v0.test",
            phoxal_cli_core::identity::ExecutionId::mint(),
            "unixsock-stream//tmp/project/.phoxal/run/zenoh.sock",
        );
        // A freshly configured board is Starting - still pending.
        assert_eq!(
            required_readiness(&board.supervisor_snapshot()),
            Readiness::Pending
        );

        // Ready is ready.
        board.set_lifecycle(ProjectLifecycle::Ready);
        assert_eq!(
            required_readiness(&board.supervisor_snapshot()),
            Readiness::Ready
        );

        // Degraded with no required failure is ready enough to signal `READY=1`.
        board.set_lifecycle(ProjectLifecycle::Degraded);
        assert_eq!(
            required_readiness(&board.supervisor_snapshot()),
            Readiness::Ready
        );

        // A startup-required process that failed keeps a Degraded graph pending,
        // so readiness is never signalled on a broken required participant.
        let key = ProcessKey::project("drive");
        board.upsert_process(
            key.clone(),
            ParticipantKind::Service,
            ProcessState::Starting,
            StartupRequirement::Required,
        );
        board.set_state(&key, ProcessState::Failed, Some("boom".to_string()));
        board.set_lifecycle(ProjectLifecycle::Degraded);
        assert_eq!(
            required_readiness(&board.supervisor_snapshot()),
            Readiness::Pending
        );

        // A failed lifecycle names the failed processes.
        assert!(matches!(
            required_readiness(&{
                board.set_lifecycle(ProjectLifecycle::Failed);
                board.supervisor_snapshot()
            }),
            Readiness::Failed(failures) if failures.iter().any(|failure| failure.contains("drive"))
        ));
    }

    #[test]
    fn startup_wait_modes_pin_presentation_deadline_and_cancellation_policy() {
        let now = tokio::time::Instant::now();
        assert!(StartupWaitMode::ForegroundOwned.interactive(true));
        assert!(!StartupWaitMode::ForegroundOwned.interactive(false));
        assert!(!StartupWaitMode::Detached.interactive(true));
        assert_eq!(StartupWaitMode::ForegroundOwned.deadline(now), None);
        assert_eq!(
            StartupWaitMode::Detached.deadline(now),
            Some(now + std::time::Duration::from_secs(5 * 60))
        );
        assert!(!StartupWaitMode::ForegroundOwned.leaves_resident_on_cancel());
        assert!(StartupWaitMode::Detached.leaves_resident_on_cancel());
    }
}

#[cfg(test)]
mod preparation_cancellation_tests {
    use super::{SupervisorState, finish_cancelled_preparation};
    use phoxal_cli_core::runtime::ProjectLifecycle;

    #[tokio::test]
    async fn cancellation_during_preparation_finishes_stopped_not_failed() {
        let board = SupervisorState::new();
        board.set_lifecycle(ProjectLifecycle::Starting);
        let mut preparation = tokio::spawn(std::future::pending::<()>());
        // Other unit tests may be building fixtures concurrently in this
        // process. The production call passes `true`; this lifecycle-only test
        // avoids killing unrelated test-owned cargo process groups.
        finish_cancelled_preparation(&board, &mut preparation).await;
        assert_eq!(
            board.supervisor_snapshot().lifecycle,
            ProjectLifecycle::Stopped
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{AbortTasks, BoardConsole, join_board_console};
    use anyhow::Result;
    use phoxal_cli_core::project::launch_plan::{LaunchMode, LaunchPlan};
    use phoxal_cli_core::runtime::ParticipantSpec;
    use phoxal_cli_core::runtime::{
        ParticipantKind, ProcessKey, ReadinessPolicy, RuntimeFailurePolicy, StartupRequirement,
    };
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[test]
    fn human_launch_report_enters_the_active_session_diagnostics() -> Result<()> {
        let _guard = crate::cli::output::diagnostics::DIAGNOSTICS_TEST_LOCK.blocking_lock();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        crate::cli::output::diagnostics::install(tx);
        let plan = LaunchPlan {
            mode: LaunchMode::Run,
            robots: Vec::new(),
        };
        let specs = [ParticipantSpec {
            key: ProcessKey::project("fixture"),
            id: "fixture".to_string(),
            kind: ParticipantKind::Host,
            executable: PathBuf::from("fixture-command"),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            shutdown_grace: Duration::from_secs(1),
            process_group: false,
            note: None,
            bus_participant: false,
            readiness: ReadinessPolicy::ProcessSpawned,
            startup_requirement: StartupRequirement::Required,
            runtime_failure: RuntimeFailurePolicy::StopProject,
            restart_policy: Default::default(),
        }];

        let result =
            super::report_launch_commands(&plan, &specs, &crate::cli::Ui::new(true, false));
        crate::cli::output::diagnostics::uninstall();
        result?;

        let messages = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|event| match event {
                phoxal_cli_observation::RuntimeEvent::Diagnostic { message, .. } => Some(message),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0], "resolved launch participants:");
        assert!(messages[1].contains("fixture (host) -> fixture-command"));
        assert!(messages[2].starts_with("motion guarantees:"));
        Ok(())
    }

    #[tokio::test]
    async fn dropping_setup_background_tasks_aborts_every_handle() {
        let handle = tokio::spawn(std::future::pending::<()>());
        let abort = handle.abort_handle();
        let mut tasks = AbortTasks::default();
        tasks.push(handle);
        drop(tasks);
        tokio::task::yield_now().await;
        assert!(abort.is_finished());
    }

    #[tokio::test]
    async fn console_error_suppression_requires_observed_report_evidence() {
        for expected in [false, true] {
            let reported_failure = Arc::new(AtomicBool::new(expected));
            let mut console = Some(BoardConsole {
                task: tokio::spawn(async {}),
                reported_failure: Arc::clone(&reported_failure),
            });
            assert_eq!(join_board_console(&mut console).await, expected);
            assert!(console.is_none());
            assert_eq!(reported_failure.load(Ordering::Acquire), expected);
        }
    }
}

mod installation {
    use std::future::Future;
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::pin::Pin;
    use std::process::Command;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use anyhow::{Context, Result, bail};
    use sha2::{Digest, Sha256};

    use phoxal_cli_supervisor::{ProjectLock, ProjectLockIdentity, ProjectOperation};

    const READINESS_TIMEOUT: Duration = Duration::from_secs(5 * 60);

    #[derive(Debug, Clone)]
    struct InstallRoots {
        active: PathBuf,
        releases: PathBuf,
        state: PathBuf,
        volatile: PathBuf,
    }

    impl InstallRoots {
        fn system() -> Self {
            Self {
                active: PathBuf::from(phoxal_cli_project::ACTIVE_RUNTIME_ROOT),
                releases: PathBuf::from(phoxal_cli_project::RELEASES_ROOT),
                state: PathBuf::from(phoxal_cli_project::INSTALLED_STATE_ROOT),
                volatile: PathBuf::from(phoxal_cli_project::INSTALLED_VOLATILE_ROOT),
            }
        }
    }

    trait ServiceManager {
        fn stop(&self) -> Result<()>;
        fn start(&self) -> Result<()>;
        fn wait_ready<'a>(
            &'a self,
            supervisor_socket: &'a Path,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;
    }

    struct SystemdService;

    impl ServiceManager for SystemdService {
        fn stop(&self) -> Result<()> {
            systemctl(["stop", phoxal_cli_project::SYSTEMD_UNIT])
        }

        fn start(&self) -> Result<()> {
            systemctl(["reset-failed", phoxal_cli_project::SYSTEMD_UNIT])?;
            systemctl(["start", "--no-block", phoxal_cli_project::SYSTEMD_UNIT])
        }

        fn wait_ready<'a>(
            &'a self,
            supervisor_socket: &'a Path,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
            Box::pin(async move {
                let deadline = tokio::time::Instant::now() + READINESS_TIMEOUT;
                loop {
                    if let Some(failure) = systemd_failure()? {
                        bail!("phoxal.service failed before readiness: {failure}");
                    }
                    if let Ok(feed) =
                        phoxal_cli_client::SupervisorFeed::connect(supervisor_socket).await
                    {
                        let snapshot = feed.current();
                        match crate::application::run::required_readiness(&snapshot) {
                            crate::application::run::Readiness::Ready => return Ok(()),
                            crate::application::run::Readiness::Failed(failures) => {
                                // Mirrors `wait_for_required_readiness`: a
                                // resident-level failure (no single process
                                // to blame) carries its own reason on the
                                // snapshot and must win over an empty
                                // per-process failure list, which otherwise
                                // renders as a dangling, cause-free colon. A
                                // relayed reason already describes its cause,
                                // while the reason-less branches below must
                                // frame the failure themselves.
                                if let Some(reason) = &snapshot.failure {
                                    bail!(crate::application::attachment::resident_failure_message(
                                        Path::new(phoxal_cli_project::ACTIVE_RUNTIME_ROOT),
                                        Some(reason)
                                    ))
                                }
                                if failures.is_empty() {
                                    let log =
                                        phoxal_cli_core::runtime::paths::RuntimePaths::for_root(
                                            Path::new(phoxal_cli_project::ACTIVE_RUNTIME_ROOT),
                                        )
                                        .supervisor_log();
                                    bail!(
                                        "installed runtime failed readiness before any participant \
                                         launched; see {} for the exact error",
                                        log.display()
                                    )
                                }
                                bail!(
                                    "installed runtime failed readiness: {}",
                                    failures.join(", ")
                                )
                            }
                            crate::application::run::Readiness::Pending => {}
                        }
                    }
                    if tokio::time::Instant::now() >= deadline {
                        bail!("timed out waiting for installed supervisor readiness");
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            })
        }
    }

    fn systemd_failure() -> Result<Option<String>> {
        let output = Command::new("systemctl")
            .args([
                "show",
                "--no-pager",
                "--property=ActiveState",
                "--property=NRestarts",
                "--property=Result",
                phoxal_cli_project::SYSTEMD_UNIT,
            ])
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "failed to inspect phoxal.service readiness state"
        );
        Ok(parse_systemd_failure(&String::from_utf8(output.stdout)?))
    }

    fn parse_systemd_failure(state: &str) -> Option<String> {
        let active = state
            .lines()
            .find_map(|line| line.strip_prefix("ActiveState="))
            .unwrap_or_default();
        let result = state
            .lines()
            .find_map(|line| line.strip_prefix("Result="))
            .unwrap_or_default();
        let restarts = state
            .lines()
            .find_map(|line| line.strip_prefix("NRestarts="))
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or_default();
        if active == "failed" || !matches!(result, "" | "success") || restarts > 0 {
            Some(format!(
                "ActiveState={active}, Result={result}, NRestarts={restarts}"
            ))
        } else {
            None
        }
    }

    pub(super) async fn install(archive: &Path, offline: bool) -> Result<PathBuf> {
        require_system_installation()?;
        let archive = archive
            .canonicalize()
            .with_context(|| format!("failed to resolve build archive {}", archive.display()))?;
        install_archive(&archive, &InstallRoots::system(), &SystemdService, offline).await
    }

    pub(super) async fn rollback(release: Option<&str>) -> Result<PathBuf> {
        require_system_installation()?;
        rollback_release(release, &InstallRoots::system(), &SystemdService).await
    }

    fn require_system_installation() -> Result<()> {
        anyhow::ensure!(
            unsafe { libc::geteuid() } == 0,
            "`phoxal install` and `phoxal rollback` require root"
        );
        anyhow::ensure!(
            Path::new(phoxal_cli_project::SYSTEMD_ACTIVE_ROOT).is_dir(),
            "systemd is not the active service manager on this host"
        );
        anyhow::ensure!(
            Path::new(phoxal_cli_project::SYSTEMD_UNIT_PATH).is_file(),
            "phoxal.service is not installed; run `sudo phoxal service install` first"
        );
        Ok(())
    }

    async fn install_archive(
        archive: &Path,
        roots: &InstallRoots,
        service: &dyn ServiceManager,
        offline: bool,
    ) -> Result<PathBuf> {
        require_build_archive(archive)?;
        let digest = sha256_file(archive)?;
        let name = format!(
            "{}-{}",
            sortable_utc_timestamp(SystemTime::now())?,
            &digest[..8]
        );
        std::fs::create_dir_all(&roots.releases)?;
        std::fs::create_dir_all(&roots.state)?;
        std::fs::create_dir_all(&roots.volatile)?;
        let candidate = roots.releases.join(format!(".{name}.candidate"));
        let release = roots.releases.join(&name);
        anyhow::ensure!(
            !release.exists(),
            "release {name} already exists; retry after the clock advances"
        );
        remove_dir_if_present(&candidate)?;

        let prepared = async {
            phoxal_cli_project::validate(phoxal_cli_project::ValidateRequest {
                source: phoxal_cli_project::ValidationSource::Archive(
                    phoxal_cli_project::ArchiveValidation {
                        archive: archive.to_path_buf(),
                        destination: candidate.clone(),
                    },
                ),
                offline,
                reporter: std::sync::Arc::new(phoxal_cli_project::SilentReporter),
            })
            .await?;
            fsync_tree(&candidate)?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = prepared {
            remove_dir_if_present(&candidate)?;
            return Err(error);
        }
        let pre_stop_active = active_release(&roots.active, &roots.releases)?;
        if let Err(error) = service.stop().context("failed to stop phoxal.service") {
            remove_dir_if_present(&candidate)?;
            return Err(error);
        }
        let identity = ProjectLockIdentity::resolve(&roots.active, ProjectOperation::Install);
        let _lock = match ProjectLock::acquire_path(&roots.state.join("project.lock"), identity)
            .context("failed to acquire the installed-runtime lock")
        {
            Ok(lock) => lock,
            Err(error) => {
                remove_dir_if_present(&candidate)?;
                if pre_stop_active.is_some() {
                    let _ = service.start();
                }
                return Err(error);
            }
        };
        let previous = active_release(&roots.active, &roots.releases)?;
        if let Err(error) = (|| -> Result<()> {
            std::fs::rename(&candidate, &release)?;
            fsync_dir(&roots.releases)?;
            atomic_symlink_switch(&roots.active, &release)?;
            Ok(())
        })() {
            drop(_lock);
            remove_dir_if_present(&candidate)?;
            restore_after_failed_activation(previous.as_deref(), roots, service).await?;
            discard_failed_release(&release, &roots.releases)?;
            return Err(error);
        }
        drop(_lock);

        if let Err(error) = service.start().context("failed to start phoxal.service") {
            restore_after_failed_activation(previous.as_deref(), roots, service).await?;
            discard_failed_release(&release, &roots.releases)?;
            return Err(error);
        }
        if let Err(error) = service
            .wait_ready(&roots.volatile.join("supervisor.sock"))
            .await
        {
            restore_after_failed_activation(previous.as_deref(), roots, service).await?;
            discard_failed_release(&release, &roots.releases)?;
            return Err(error).context("new release was rolled back after failed readiness");
        }
        Ok(release)
    }

    async fn rollback_release(
        requested: Option<&str>,
        roots: &InstallRoots,
        service: &dyn ServiceManager,
    ) -> Result<PathBuf> {
        let active = active_release(&roots.active, &roots.releases)?
            .context("cannot roll back: /var/phoxal does not select a release")?;
        let selected = select_rollback_release(requested, &active, &roots.releases)?;
        service.stop().context("failed to stop phoxal.service")?;
        let identity = ProjectLockIdentity::resolve(&roots.active, ProjectOperation::Install);
        let _lock = ProjectLock::acquire_path(&roots.state.join("project.lock"), identity)
            .context("failed to acquire the installed-runtime lock")?;
        atomic_symlink_switch(&roots.active, &selected)?;
        drop(_lock);
        if let Err(error) = service.start().context("failed to start rollback release") {
            restore_after_failed_activation(Some(&active), roots, service).await?;
            return Err(error);
        }
        if let Err(error) = service
            .wait_ready(&roots.volatile.join("supervisor.sock"))
            .await
        {
            restore_after_failed_activation(Some(&active), roots, service).await?;
            return Err(error).context("rollback target failed readiness; original restored");
        }
        Ok(selected)
    }

    fn discard_failed_release(release: &Path, releases: &Path) -> Result<()> {
        remove_dir_if_present(release)?;
        fsync_dir(releases)
    }

    async fn restore_after_failed_activation(
        previous: Option<&Path>,
        roots: &InstallRoots,
        service: &dyn ServiceManager,
    ) -> Result<()> {
        let _ = service.stop();
        if let Some(previous) = previous {
            atomic_symlink_switch(&roots.active, previous)?;
            service
                .start()
                .context("failed to restart the previous release")?;
            service
                .wait_ready(&roots.volatile.join("supervisor.sock"))
                .await
                .context("previous release did not recover after rollback")?;
        } else {
            remove_file_if_present(&roots.active)?;
            fsync_dir(
                roots
                    .active
                    .parent()
                    .context("active runtime path has no parent")?,
            )?;
        }
        Ok(())
    }

    fn require_build_archive(path: &Path) -> Result<()> {
        anyhow::ensure!(path.is_file(), "{} is not a file", path.display());
        anyhow::ensure!(
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".build.phoxal") || name == "build.phoxal"),
            "{} is not a build.phoxal archive",
            path.display()
        );
        Ok(())
    }

    fn active_release(active: &Path, releases: &Path) -> Result<Option<PathBuf>> {
        match std::fs::symlink_metadata(active) {
            Ok(metadata) => {
                anyhow::ensure!(
                    metadata.file_type().is_symlink(),
                    "{} exists but is not a symlink",
                    active.display()
                );
                let target = std::fs::read_link(active).map(|target| {
                    if target.is_absolute() {
                        target
                    } else {
                        active
                            .parent()
                            .unwrap_or_else(|| Path::new("/"))
                            .join(target)
                    }
                })?;
                let canonical = target.canonicalize()?;
                let canonical_releases = releases.canonicalize()?;
                anyhow::ensure!(
                    canonical.parent() == Some(canonical_releases.as_path()),
                    "{} points outside {}",
                    active.display(),
                    releases.display()
                );
                Ok(Some(canonical))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn select_rollback_release(
        requested: Option<&str>,
        active: &Path,
        releases: &Path,
    ) -> Result<PathBuf> {
        let mut names = std::fs::read_dir(releases)?
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| valid_release_name(name))
            .collect::<Vec<_>>();
        names.sort();
        let selected = if let Some(requested) = requested {
            anyhow::ensure!(
                valid_release_name(requested),
                "invalid release directory name `{requested}`"
            );
            anyhow::ensure!(
                names.iter().any(|name| name == requested),
                "release `{requested}` does not exist"
            );
            requested.to_string()
        } else {
            let active_name = active
                .file_name()
                .and_then(|name| name.to_str())
                .context("active release has no valid directory name")?;
            let index = names
                .iter()
                .position(|name| name == active_name)
                .context("active release is not in the release index")?;
            anyhow::ensure!(index > 0, "there is no older release to roll back to");
            names[index - 1].clone()
        };
        Ok(releases.join(selected))
    }

    fn valid_release_name(name: &str) -> bool {
        let bytes = name.as_bytes();
        bytes.len() == 29
            && bytes[8] == b'T'
            && bytes[15] == b'.'
            && bytes[19] == b'Z'
            && bytes[20] == b'-'
            && bytes[..8].iter().all(u8::is_ascii_digit)
            && bytes[9..15].iter().all(u8::is_ascii_digit)
            && bytes[16..19].iter().all(u8::is_ascii_digit)
            && bytes[21..].iter().all(u8::is_ascii_hexdigit)
    }

    fn sha256_file(path: &Path) -> Result<String> {
        let mut file = std::fs::File::open(path)?;
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(hex::encode(hasher.finalize()))
    }

    fn sortable_utc_timestamp(now: SystemTime) -> Result<String> {
        let duration = now.duration_since(UNIX_EPOCH)?;
        let seconds: libc::time_t = duration.as_secs().try_into()?;
        let mut broken_down = std::mem::MaybeUninit::<libc::tm>::uninit();
        // SAFETY: both pointers refer to initialized, properly aligned storage and
        // `gmtime_r` writes one `tm` without retaining either pointer.
        let result = unsafe { libc::gmtime_r(&seconds, broken_down.as_mut_ptr()) };
        anyhow::ensure!(!result.is_null(), "failed to convert current time to UTC");
        // SAFETY: a non-null `gmtime_r` result initialized the output `tm`.
        let tm = unsafe { broken_down.assume_init() };
        Ok(format!(
            "{:04}{:02}{:02}T{:02}{:02}{:02}.{:03}Z",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec,
            duration.subsec_millis()
        ))
    }

    fn atomic_symlink_switch(active: &Path, target: &Path) -> Result<()> {
        let parent = active
            .parent()
            .context("active runtime path has no parent directory")?;
        std::fs::create_dir_all(parent)?;
        let candidate = parent.join(format!(".phoxal-link-{}", std::process::id()));
        remove_file_if_present(&candidate)?;
        std::os::unix::fs::symlink(target, &candidate)?;
        std::fs::rename(&candidate, active)?;
        fsync_dir(parent)
    }

    fn fsync_tree(root: &Path) -> Result<()> {
        for entry in std::fs::read_dir(root)? {
            let entry = entry?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.is_dir() {
                fsync_tree(&path)?;
            } else if metadata.is_file() {
                std::fs::File::open(&path)?.sync_all()?;
            }
        }
        fsync_dir(root)
    }

    fn fsync_dir(path: &Path) -> Result<()> {
        std::fs::File::open(path)?.sync_all()?;
        Ok(())
    }

    fn remove_file_if_present(path: &Path) -> Result<()> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn remove_dir_if_present(path: &Path) -> Result<()> {
        match std::fs::remove_dir_all(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn systemctl<const N: usize>(args: [&str; N]) -> Result<()> {
        let status = Command::new("systemctl").args(args).status()?;
        anyhow::ensure!(status.success(), "systemctl failed with {status}");
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::Mutex;

        #[derive(Default)]
        struct FakeService {
            operations: Mutex<Vec<&'static str>>,
            fail_readiness_once: Mutex<bool>,
        }

        impl ServiceManager for FakeService {
            fn stop(&self) -> Result<()> {
                self.operations.lock().unwrap().push("stop");
                Ok(())
            }

            fn start(&self) -> Result<()> {
                self.operations.lock().unwrap().push("start");
                Ok(())
            }

            fn wait_ready<'a>(
                &'a self,
                _supervisor_socket: &'a Path,
            ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
                Box::pin(async move {
                    self.operations.lock().unwrap().push("ready");
                    let mut fail = self.fail_readiness_once.lock().unwrap();
                    if *fail {
                        *fail = false;
                        bail!("forced readiness failure");
                    }
                    Ok(())
                })
            }
        }

        fn roots(temp: &tempfile::TempDir) -> InstallRoots {
            InstallRoots {
                active: temp.path().join("var/phoxal"),
                releases: temp.path().join("var/lib/phoxal/releases"),
                state: temp.path().join("var/lib/phoxal/state"),
                volatile: temp.path().join("run/phoxal"),
            }
        }

        #[test]
        fn release_names_are_sortable_and_strict() -> Result<()> {
            let timestamp =
                sortable_utc_timestamp(UNIX_EPOCH + Duration::from_millis(1_753_402_123_456))?;
            let name = format!("{timestamp}-deadbeef");
            assert!(valid_release_name(&name), "{name}");
            assert!(!valid_release_name("../deadbeef"));
            Ok(())
        }

        #[test]
        fn systemd_failure_state_is_detected_without_waiting_for_start_timeout() {
            assert!(
                parse_systemd_failure("ActiveState=activating\nResult=success\nNRestarts=0\n")
                    .is_none()
            );
            assert_eq!(
                parse_systemd_failure("ActiveState=activating\nResult=exit-code\nNRestarts=0\n")
                    .as_deref(),
                Some("ActiveState=activating, Result=exit-code, NRestarts=0")
            );
            assert_eq!(
                parse_systemd_failure("ActiveState=activating\nResult=success\nNRestarts=1\n")
                    .as_deref(),
                Some("ActiveState=activating, Result=success, NRestarts=1")
            );
        }

        #[test]
        fn default_rollback_selects_immediately_older_release() -> Result<()> {
            let temp = tempfile::tempdir()?;
            let roots = roots(&temp);
            std::fs::create_dir_all(&roots.releases)?;
            let older = roots.releases.join("20260724T010000.000Z-11111111");
            let active = roots.releases.join("20260725T010000.000Z-22222222");
            std::fs::create_dir(&older)?;
            std::fs::create_dir(&active)?;
            assert_eq!(
                select_rollback_release(None, &active, &roots.releases)?,
                older
            );
            Ok(())
        }

        #[tokio::test]
        async fn failed_activation_restores_the_previous_symlink_and_readiness() -> Result<()> {
            let temp = tempfile::tempdir()?;
            let roots = roots(&temp);
            std::fs::create_dir_all(&roots.releases)?;
            std::fs::create_dir_all(&roots.state)?;
            std::fs::create_dir_all(&roots.volatile)?;
            let previous = roots.releases.join("20260724T010000.000Z-11111111");
            let failed = roots.releases.join("20260725T010000.000Z-22222222");
            std::fs::create_dir(&previous)?;
            std::fs::create_dir(&failed)?;
            atomic_symlink_switch(&roots.active, &failed)?;
            let service = FakeService::default();

            restore_after_failed_activation(Some(&previous), &roots, &service).await?;

            assert_eq!(std::fs::read_link(&roots.active)?, previous);
            assert_eq!(
                *service.operations.lock().unwrap(),
                ["stop", "start", "ready"]
            );
            Ok(())
        }

        #[test]
        fn atomic_switch_never_exposes_a_partial_release() -> Result<()> {
            let temp = tempfile::tempdir()?;
            let roots = roots(&temp);
            std::fs::create_dir_all(&roots.releases)?;
            let first = roots.releases.join("20260724T010000.000Z-11111111");
            let second = roots.releases.join("20260725T010000.000Z-22222222");
            std::fs::create_dir(&first)?;
            std::fs::create_dir(&second)?;
            atomic_symlink_switch(&roots.active, &first)?;
            assert_eq!(std::fs::read_link(&roots.active)?, first);
            atomic_symlink_switch(&roots.active, &second)?;
            assert_eq!(std::fs::read_link(&roots.active)?, second);
            Ok(())
        }

        #[test]
        fn post_activation_power_loss_state_remains_explicitly_rollbackable_without_metadata()
        -> Result<()> {
            let temp = tempfile::tempdir()?;
            let roots = roots(&temp);
            std::fs::create_dir_all(&roots.releases)?;
            let previous = roots.releases.join("20260724T010000.000Z-11111111");
            let active = roots.releases.join("20260725T010000.000Z-22222222");
            std::fs::create_dir(&previous)?;
            std::fs::create_dir(&active)?;

            // This is the documented narrow crash window: activation completed,
            // but the process vanished before it could confirm readiness.
            atomic_symlink_switch(&roots.active, &active)?;
            assert!(!roots.state.join("installed.json").exists());
            assert!(!roots.state.join("previous.json").exists());

            let selected = select_rollback_release(None, &active, &roots.releases)?;
            assert_eq!(selected, previous);
            atomic_symlink_switch(&roots.active, &selected)?;
            assert_eq!(std::fs::read_link(&roots.active)?, previous);
            Ok(())
        }

        #[test]
        fn failed_new_release_is_not_left_in_the_rollback_index() -> Result<()> {
            let temp = tempfile::tempdir()?;
            let roots = roots(&temp);
            std::fs::create_dir_all(&roots.releases)?;
            let failed = roots.releases.join("20260725T010000.000Z-22222222");
            std::fs::create_dir(&failed)?;
            discard_failed_release(&failed, &roots.releases)?;
            assert!(!failed.exists());
            Ok(())
        }
    }
}

pub(crate) async fn install_command(app: &AppContext, archive: &Path) -> Result<()> {
    let release = installation::install(archive, app.offline).await?;
    app.ui
        .info(format!("installed runtime release {}", release.display()));
    Ok(())
}

pub(crate) async fn rollback_command(app: &AppContext, release: Option<&str>) -> Result<()> {
    let release = installation::rollback(release).await?;
    app.ui
        .info(format!("active runtime restored to {}", release.display()));
    Ok(())
}

mod deployment {
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};

    use anyhow::{Context, Result, bail};
    pub(crate) const REMOTE_PHOXAL: &str = "/usr/local/bin/phoxal";

    use crate::cli::AppContext;

    pub(super) struct DeployRequest {
        pub(super) target: String,
        pub(super) project: Option<PathBuf>,
        pub(super) build: Option<PathBuf>,
    }

    impl DeployRequest {
        pub async fn run(&self, app: &AppContext) -> Result<()> {
            validate_ssh_target(&self.target)?;
            require_remote_phoxal(&self.target)?;
            if let Some(archive) = &self.build {
                self.deploy_archive(archive)?;
            } else {
                let output = tempfile::Builder::new()
                    .prefix("phoxal-deploy-build-")
                    .suffix(".build.phoxal")
                    .tempfile()?;
                let built = self.build_source(app, output.path()).await?;

                // The project build use case deliberately returns one locally
                // verified archive regardless of backend. Deploy sends that exact
                // artifact through the same remote installer as `--build`, trading
                // one remote-local-remote transfer for a single validation and
                // installation contract.
                self.deploy_archive(&built.archive)?;
            }
            app.ui.info(format!("deployed runtime to {}", self.target));
            Ok(())
        }

        fn deploy_archive(&self, archive: &Path) -> Result<()> {
            // Create the remote deploy directory only after all source-build
            // capability checks and compilation have succeeded.
            let remote_dir = create_remote_temp(&self.target)?;
            let result = self.deploy_prebuilt(archive, &remote_dir);
            let cleanup = cleanup_remote_temp(&self.target, &remote_dir);
            result?;
            cleanup
        }

        fn deploy_prebuilt(&self, archive: &Path, remote_dir: &str) -> Result<()> {
            let archive = archive
                .canonicalize()
                .with_context(|| format!("failed to resolve {}", archive.display()))?;
            anyhow::ensure!(archive.is_file(), "{} is not a file", archive.display());
            let remote_archive = format!("{remote_dir}/build.phoxal");
            run_local(
                "scp",
                &[
                    "-q",
                    archive.to_string_lossy().as_ref(),
                    &format!("{}:{remote_archive}", self.target),
                ],
            )?;
            run_remote(&self.target, &remote_install_command(&remote_archive))
                .context("remote installer rejected the prebuilt runtime")
        }

        async fn build_source(
            &self,
            app: &AppContext,
            output: &Path,
        ) -> Result<phoxal_cli_project::BuiltBundle> {
            let target =
                phoxal_cli_project::resolve_target(self.project.as_deref(), app.project.root())?;
            let _lock = phoxal_cli_supervisor::ProjectLock::acquire(
                phoxal_cli_supervisor::ProjectLockIdentity::resolve(
                    &target.logical_root,
                    phoxal_cli_supervisor::ProjectOperation::Build,
                ),
            )
            .context("failed to acquire the project lock for deploy")?;
            super::remove_obsolete_component_state(&target.logical_root)?;
            let (reporter, signal_task) =
                crate::cli::output::progress::cancellable_preparation_reporter(app.ui);
            let built = phoxal_cli_project::build_bundle(phoxal_cli_project::BuildBundleRequest {
                target,
                backend: phoxal_cli_project::BuildBackend::Ssh {
                    host: self.target.clone(),
                    target: None,
                },
                output: Some(output.to_path_buf()),
                publish: false,
                offline: app.offline,
                reporter,
            })
            .await;
            signal_task.abort();
            built.context("remote source build failed")
        }
    }

    pub(crate) fn remote_install_command(archive: &str) -> String {
        format!("sudo -n {REMOTE_PHOXAL} install {}", shell_quote(archive))
    }

    pub(crate) fn validate_ssh_target(target: &str) -> Result<()> {
        let Some((user, host)) = target.split_once('@') else {
            bail!("deploy target must be `user@host`");
        };
        anyhow::ensure!(
            !user.is_empty()
                && !host.is_empty()
                && !host.contains('@')
                && target
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._@:-".contains(&byte)),
            "invalid deploy target `{target}`; expected `user@host`"
        );
        Ok(())
    }

    pub(crate) fn require_remote_phoxal(target: &str) -> Result<()> {
        let output = remote_output(
            target,
            &format!("test -x {REMOTE_PHOXAL} && sudo -n test -x {REMOTE_PHOXAL}"),
        )?;
        anyhow::ensure!(
            output.status.success(),
            "{target} does not have phoxal installed. Install the verified Linux release binary as \
             `/usr/local/bin/phoxal`, then run `sudo /usr/local/bin/phoxal service install` and \
             `/usr/local/bin/phoxal service status`; deploy never provisions the device"
        );
        Ok(())
    }

    pub(crate) fn create_remote_temp(target: &str) -> Result<String> {
        let output = remote_output(target, "mktemp -d /tmp/phoxal-deploy.XXXXXX")?;
        anyhow::ensure!(
            output.status.success(),
            "failed to create remote temporary directory"
        );
        let path = String::from_utf8(output.stdout)?.trim().to_string();
        anyhow::ensure!(
            path.starts_with("/tmp/phoxal-deploy.")
                && path
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"/._-".contains(&byte)),
            "remote host returned unsafe temporary path `{path}`"
        );
        Ok(path)
    }

    pub(crate) fn cleanup_remote_temp(target: &str, path: &str) -> Result<()> {
        anyhow::ensure!(
            path.starts_with("/tmp/phoxal-deploy."),
            "refusing to clean unexpected remote path `{path}`"
        );
        run_remote(target, &format!("rm -rf -- {}", shell_quote(path)))
    }

    pub(crate) fn run_remote(target: &str, command: &str) -> Result<()> {
        let output = remote_output(target, command)?;
        if output.status.success() {
            return Ok(());
        }
        bail!(
            "ssh command failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }

    fn remote_output(target: &str, command: &str) -> Result<Output> {
        Command::new("ssh")
            .args(["-o", "BatchMode=yes", target, command])
            .output()
            .with_context(|| format!("failed to run ssh for {target}"))
    }

    pub(crate) fn run_local(program: &str, args: &[&str]) -> Result<()> {
        let status = Command::new(program).args(args).status()?;
        anyhow::ensure!(
            status.success(),
            "{} {} failed with {status}",
            program,
            args.join(" ")
        );
        Ok(())
    }

    pub(crate) fn shell_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn deploy_target_is_exactly_user_at_host() {
            assert!(validate_ssh_target("robot@jetson-nano-orin").is_ok());
            assert!(validate_ssh_target("jetson-nano-orin").is_err());
            assert!(validate_ssh_target("robot@host;reboot").is_err());
        }

        #[test]
        fn remote_cleanup_is_prefix_fenced_without_running_ssh() {
            assert!(cleanup_remote_temp("robot@host", "/").is_err());
        }

        #[test]
        fn source_and_prebuilt_modes_share_the_exact_installer_command() {
            assert_eq!(
                remote_install_command("/tmp/phoxal-deploy.ABC/build.phoxal"),
                "sudo -n /usr/local/bin/phoxal install '/tmp/phoxal-deploy.ABC/build.phoxal'"
            );
        }
    }
}

pub(crate) async fn deploy_command(
    app: &AppContext,
    target: String,
    project: Option<PathBuf>,
    build: Option<PathBuf>,
) -> Result<()> {
    deployment::DeployRequest {
        target,
        project,
        build,
    }
    .run(app)
    .await
}

mod service {
    use std::collections::BTreeMap;
    use std::io::ErrorKind;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use anyhow::{Context, Result};

    use crate::cli::AppContext;

    struct ServiceInstall;
    struct ServiceUninstall;
    struct ServiceStatus;

    const UNIT_PATH: &str = phoxal_cli_project::SYSTEMD_UNIT_PATH;
    const UNIT_MARKER: &str = "# Managed by phoxal";
    const LEGACY_INSTALL_ROOT: &str = "/opt/phoxal";

    fn unit_contents() -> String {
        format!(
            r#"# Managed by phoxal
[Unit]
Description=Phoxal robot runtime
After=network-online.target
Wants=network-online.target

[Service]
Type=notify
NotifyAccess=main
User=phoxal
Group=phoxal-engineering
SupplementaryGroups=phoxal
WorkingDirectory={active}
ExecStart=/usr/local/bin/phoxal start {active}
Restart=on-failure
RestartSec=2s
WatchdogSec=30s
TimeoutStartSec=300s
TimeoutStopSec=300s
KillMode=control-group
UMask=0007
RuntimeDirectory=phoxal
RuntimeDirectoryMode=2775
ProtectSystem=strict
ProtectHome=true
ReadWritePaths={state} {volatile}

[Install]
WantedBy=multi-user.target
"#,
            active = phoxal_cli_project::ACTIVE_RUNTIME_ROOT,
            state = phoxal_cli_project::INSTALLED_STATE_ROOT,
            volatile = phoxal_cli_project::INSTALLED_VOLATILE_ROOT,
        )
    }

    impl ServiceInstall {
        async fn run(&self, app: &AppContext) -> Result<()> {
            require_root()?;
            require_systemd()?;
            let legacy = sweep_legacy_units(
                Path::new(phoxal_cli_project::SYSTEMD_UNIT_ROOT),
                Path::new(LEGACY_INSTALL_ROOT),
                &HostSystemctl,
            )?;
            for path in &legacy.skipped_foreign {
                app.ui.warn(format!(
                    "left same-named foreign systemd entry untouched: {}",
                    path.display()
                ));
            }
            if !legacy.removed_units.is_empty() {
                app.ui.info(format!(
                    "removed legacy {} systemd wiring; preserved {}",
                    legacy.removed_units.join(", "),
                    LEGACY_INSTALL_ROOT
                ));
            }
            ensure_group("phoxal", true)?;
            ensure_group("phoxal-engineering", false)?;
            ensure_service_user()?;
            ensure_runtime_paths()?;
            write_managed_unit(Path::new(UNIT_PATH))?;
            run_status("systemctl", &["daemon-reload"])?;
            run_status("systemctl", &["enable", phoxal_cli_project::SYSTEMD_UNIT])?;
            app.ui.info(
                "installed the single phoxal.service; install a build.phoxal before starting it",
            );
            Ok(())
        }
    }

    impl ServiceUninstall {
        async fn run(&self, app: &AppContext) -> Result<()> {
            require_root()?;
            require_systemd()?;
            let path = Path::new(UNIT_PATH);
            if path.exists() {
                let contents = std::fs::read_to_string(path)?;
                anyhow::ensure!(
                    contents.starts_with(UNIT_MARKER),
                    "refusing to remove foreign unit {}",
                    path.display()
                );
                let _ = run_status(
                    "systemctl",
                    &["disable", "--now", phoxal_cli_project::SYSTEMD_UNIT],
                );
                std::fs::remove_file(path)?;
                sync_parent(path)?;
                run_status("systemctl", &["daemon-reload"])?;
            }
            app.ui.info(
                "removed phoxal.service; releases, state, users, and hardware-group membership were preserved",
            );
            Ok(())
        }
    }

    impl ServiceStatus {
        async fn run(&self, _app: &AppContext) -> Result<()> {
            require_systemd()?;
            run_status(
                "systemctl",
                &[
                    "status",
                    "--no-pager",
                    "--full",
                    phoxal_cli_project::SYSTEMD_UNIT,
                ],
            )
        }
    }

    fn require_root() -> Result<()> {
        anyhow::ensure!(
            unsafe { libc::geteuid() } == 0,
            "`phoxal service install` and `uninstall` require root"
        );
        Ok(())
    }

    fn require_systemd() -> Result<()> {
        anyhow::ensure!(
            Path::new(phoxal_cli_project::SYSTEMD_ACTIVE_ROOT).is_dir(),
            "systemd is not the active service manager on this host"
        );
        Ok(())
    }

    fn ensure_group(name: &str, system: bool) -> Result<()> {
        if Command::new("getent")
            .args(["group", name])
            .status()
            .is_ok_and(|status| status.success())
        {
            return Ok(());
        }
        let mut args = Vec::new();
        if system {
            args.push("--system");
        }
        args.push(name);
        run_status("groupadd", &args)
    }

    fn ensure_service_user() -> Result<()> {
        if Command::new("id")
            .args(["-u", "phoxal"])
            .status()
            .is_ok_and(|status| status.success())
        {
            return run_status(
                "usermod",
                &[
                    "--gid",
                    "phoxal",
                    "--append",
                    "--groups",
                    "phoxal-engineering",
                    "phoxal",
                ],
            );
        }
        run_status(
            "useradd",
            &[
                "--system",
                "--gid",
                "phoxal",
                "--groups",
                "phoxal-engineering",
                "--home-dir",
                phoxal_cli_project::INSTALL_ROOT,
                "--shell",
                "/usr/sbin/nologin",
                "phoxal",
            ],
        )
    }

    fn ensure_runtime_paths() -> Result<()> {
        use std::fs::OpenOptions;
        use std::os::unix::fs::PermissionsExt;
        for path in [
            phoxal_cli_project::RELEASES_ROOT,
            phoxal_cli_project::INSTALLED_STATE_ROOT,
            phoxal_cli_project::INSTALLED_VOLATILE_ROOT,
        ] {
            std::fs::create_dir_all(path)?;
        }
        std::fs::set_permissions(
            phoxal_cli_project::RELEASES_ROOT,
            std::fs::Permissions::from_mode(0o755),
        )?;
        run_status("chown", &["root:root", phoxal_cli_project::RELEASES_ROOT])?;
        for path in [
            phoxal_cli_project::INSTALLED_STATE_ROOT,
            phoxal_cli_project::INSTALLED_VOLATILE_ROOT,
        ] {
            run_status("chown", &["phoxal:phoxal-engineering", path])?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o2775))?;
        }
        let lock = Path::new(phoxal_cli_project::INSTALLED_STATE_ROOT).join("project.lock");
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock)?
            .sync_all()?;
        run_status(
            "chown",
            &["phoxal:phoxal-engineering", lock.to_string_lossy().as_ref()],
        )?;
        std::fs::set_permissions(lock, std::fs::Permissions::from_mode(0o660))?;
        Ok(())
    }

    fn write_managed_unit(path: &Path) -> Result<()> {
        if path.exists() {
            let current = std::fs::read_to_string(path)?;
            anyhow::ensure!(
                current.starts_with(UNIT_MARKER),
                "refusing to overwrite foreign unit {}",
                path.display()
            );
            if current == unit_contents() {
                return Ok(());
            }
        }
        let candidate = PathBuf::from(format!(
            "{}.candidate-{}",
            path.display(),
            std::process::id()
        ));
        std::fs::write(&candidate, unit_contents())?;
        std::fs::File::open(&candidate)?.sync_all()?;
        std::fs::rename(&candidate, path)?;
        sync_parent(path)
    }

    fn sync_parent(path: &Path) -> Result<()> {
        std::fs::File::open(path.parent().context("path has no parent")?)?.sync_all()?;
        Ok(())
    }

    fn run_status(program: &str, args: &[&str]) -> Result<()> {
        let status = Command::new(program).args(args).status()?;
        anyhow::ensure!(
            status.success(),
            "{} {} failed with {status}",
            program,
            args.join(" ")
        );
        Ok(())
    }

    trait Systemctl {
        fn disable_now(&self, unit: &str) -> Result<()>;
    }

    struct HostSystemctl;

    impl Systemctl for HostSystemctl {
        fn disable_now(&self, unit: &str) -> Result<()> {
            run_status("systemctl", &["disable", "--now", unit])
        }
    }

    #[derive(Debug, Default, PartialEq, Eq)]
    struct LegacySweep {
        removed_units: Vec<String>,
        skipped_foreign: Vec<PathBuf>,
    }

    fn sweep_legacy_units(
        systemd_root: &Path,
        legacy_root: &Path,
        systemctl: &impl Systemctl,
    ) -> Result<LegacySweep> {
        let mut links_by_unit = BTreeMap::<String, Vec<PathBuf>>::new();
        let mut skipped_foreign = Vec::new();
        let canonical_legacy_root = match legacy_root.canonicalize() {
            Ok(path) => Some(path),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        for directory in [
            systemd_root.to_path_buf(),
            systemd_root.join("phoxal.target.wants"),
            systemd_root.join("multi-user.target.wants"),
        ] {
            let entries = match std::fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            for entry in entries {
                let entry = entry?;
                let Some(unit) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if !is_legacy_unit_name(&unit) {
                    continue;
                }
                let path = entry.path();
                if is_confirmed_legacy_link(&path, canonical_legacy_root.as_deref()) {
                    links_by_unit.entry(unit).or_default().push(path);
                } else {
                    skipped_foreign.push(path);
                }
            }
        }

        let mut units = links_by_unit.keys().cloned().collect::<Vec<_>>();
        units.sort_by(|left, right| {
            legacy_unit_rank(left)
                .cmp(&legacy_unit_rank(right))
                .then_with(|| left.cmp(right))
        });
        for unit in &units {
            systemctl.disable_now(unit)?;
            for path in &links_by_unit[unit] {
                match std::fs::symlink_metadata(path) {
                    Ok(_) if is_confirmed_legacy_link(path, canonical_legacy_root.as_deref()) => {
                        std::fs::remove_file(path)?;
                        sync_parent(path)?;
                    }
                    Ok(_) => anyhow::bail!(
                        "refusing to remove changed or foreign systemd entry {}",
                        path.display()
                    ),
                    Err(error) if error.kind() == ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        let legacy_wants = systemd_root.join("phoxal.target.wants");
        match std::fs::remove_dir(&legacy_wants) {
            Ok(()) => sync_parent(&legacy_wants)?,
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::NotFound | ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(error) => return Err(error.into()),
        }
        skipped_foreign.sort();
        Ok(LegacySweep {
            removed_units: units,
            skipped_foreign,
        })
    }

    fn is_legacy_unit_name(name: &str) -> bool {
        name == "phoxal.target"
            || name == "phoxal-router.service"
            || (name
                .strip_prefix("phoxal-participant-")
                .and_then(|suffix| suffix.strip_suffix(".service"))
                .is_some_and(|participant| !participant.is_empty()))
    }

    fn legacy_unit_rank(name: &str) -> u8 {
        match name {
            "phoxal.target" => 0,
            "phoxal-router.service" => 1,
            _ => 2,
        }
    }

    fn is_confirmed_legacy_link(path: &Path, canonical_legacy_root: Option<&Path>) -> bool {
        let Ok(metadata) = std::fs::symlink_metadata(path) else {
            return false;
        };
        if !metadata.file_type().is_symlink() {
            return false;
        }
        let Some(legacy_root) = canonical_legacy_root else {
            return false;
        };
        path.canonicalize()
            .is_ok_and(|target| target.starts_with(legacy_root))
    }

    pub(super) async fn install(app: &AppContext) -> Result<()> {
        ServiceInstall.run(app).await
    }

    pub(super) async fn uninstall(app: &AppContext) -> Result<()> {
        ServiceUninstall.run(app).await
    }

    pub(super) async fn status(app: &AppContext) -> Result<()> {
        ServiceStatus.run(app).await
    }

    #[cfg(test)]
    mod unit_tests {
        use std::cell::RefCell;
        use std::os::unix::fs::symlink;

        use super::*;

        #[test]
        fn managed_service_renders_one_resident_runtime_authority() {
            let unit = unit_contents();
            assert_eq!(unit.matches("ExecStart=").count(), 1);
            assert!(unit.contains("ExecStart=/usr/local/bin/phoxal start /var/phoxal"));
            assert!(unit.contains("Type=notify"));
            assert!(unit.contains("NotifyAccess=main"));
            assert!(unit.contains("WatchdogSec=30s"));
            assert!(unit.contains("User=phoxal\nGroup=phoxal-engineering"));
            assert!(!unit.contains("StateDirectory="));
            assert!(!unit.contains("participant"));
        }

        #[derive(Default)]
        struct FakeSystemctl {
            disabled: RefCell<Vec<String>>,
        }

        impl Systemctl for FakeSystemctl {
            fn disable_now(&self, unit: &str) -> Result<()> {
                self.disabled.borrow_mut().push(unit.to_string());
                Ok(())
            }
        }

        #[test]
        fn legacy_sweep_removes_only_opt_phoxal_unit_links_in_safe_order() -> Result<()> {
            let temp = tempfile::tempdir()?;
            let systemd = temp.path().join("etc/systemd/system");
            let legacy = temp.path().join("opt/phoxal");
            let foreign = temp.path().join("foreign");
            std::fs::create_dir_all(systemd.join("phoxal.target.wants"))?;
            std::fs::create_dir_all(systemd.join("multi-user.target.wants"))?;
            std::fs::create_dir_all(legacy.join("systemd"))?;
            std::fs::create_dir_all(&foreign)?;
            for unit in [
                "phoxal.target",
                "phoxal-router.service",
                "phoxal-participant-drive.service",
            ] {
                std::fs::write(legacy.join("systemd").join(unit), "[Unit]\n")?;
                symlink(legacy.join("systemd").join(unit), systemd.join(unit))?;
            }
            symlink(
                legacy.join("systemd/phoxal.target"),
                systemd.join("multi-user.target.wants/phoxal.target"),
            )?;
            symlink(
                legacy.join("systemd/phoxal-router.service"),
                systemd.join("phoxal.target.wants/phoxal-router.service"),
            )?;
            symlink(
                legacy.join("systemd/phoxal-participant-drive.service"),
                systemd.join("phoxal.target.wants/phoxal-participant-drive.service"),
            )?;
            std::fs::write(systemd.join("phoxal.service"), "# Managed by phoxal\n")?;
            std::fs::write(foreign.join("phoxal-participant-map.service"), "[Unit]\n")?;
            symlink(
                foreign.join("phoxal-participant-map.service"),
                systemd.join("phoxal-participant-map.service"),
            )?;
            std::fs::write(
                systemd.join("phoxal-participant-safety.service"),
                "[Unit]\n",
            )?;
            let systemctl = FakeSystemctl::default();

            let result = sweep_legacy_units(&systemd, &legacy, &systemctl)?;

            assert_eq!(
                result.removed_units,
                [
                    "phoxal.target",
                    "phoxal-router.service",
                    "phoxal-participant-drive.service"
                ]
            );
            assert_eq!(
                *systemctl.disabled.borrow(),
                result.removed_units,
                "the target must stop before its router and participants"
            );
            assert_eq!(
                result.skipped_foreign,
                [
                    systemd.join("phoxal-participant-map.service"),
                    systemd.join("phoxal-participant-safety.service")
                ]
            );
            assert!(systemd.join("phoxal.service").is_file());
            assert!(legacy.join("systemd/phoxal.target").is_file());
            assert!(!systemd.join("phoxal.target").exists());
            assert!(!systemd.join("phoxal.target.wants").exists());
            Ok(())
        }

        #[test]
        fn legacy_unit_name_never_selects_the_resident_service() {
            assert!(is_legacy_unit_name("phoxal.target"));
            assert!(is_legacy_unit_name("phoxal-router.service"));
            assert!(is_legacy_unit_name("phoxal-participant-drive.service"));
            assert!(!is_legacy_unit_name("phoxal.service"));
            assert!(!is_legacy_unit_name("phoxal-participant-.service"));
            assert!(!is_legacy_unit_name("phoxal-participant-drive.timer"));
        }
    }
}

pub(crate) async fn service_install_command(app: &AppContext) -> Result<()> {
    service::install(app).await
}

pub(crate) async fn service_uninstall_command(app: &AppContext) -> Result<()> {
    service::uninstall(app).await
}

pub(crate) async fn service_status_command(app: &AppContext) -> Result<()> {
    service::status(app).await
}

mod doctor {
    use crate::cli::AppContext;
    use anyhow::{Context, Result};
    use phoxal_cli_core::project::train::RegistryStatus;
    struct Doctor;

    impl Doctor {
        pub async fn run(&self, app: &AppContext) -> Result<()> {
            for status in phoxal_cli_project::host::doctor::probes() {
                match status {
                    phoxal_cli_project::host::doctor::ProbeStatus::Ok(message) => {
                        app.ui.success(message);
                    }
                    phoxal_cli_project::host::doctor::ProbeStatus::Warn(message) => {
                        app.ui.warn(message);
                    }
                    phoxal_cli_project::host::doctor::ProbeStatus::Fail(error) => {
                        app.ui.warn(error.to_string());
                    }
                }
            }
            let train = phoxal_cli_core::project::train::resolve_locked_train(
                app.project.root(),
                app.offline,
            )?;
            println!(
                "framework train: {} ({})",
                train.version,
                match &train.source {
                    phoxal_cli_core::project::train::TrainSource::Registry => "registry",
                    phoxal_cli_core::project::train::TrainSource::Git(_) => "git",
                    phoxal_cli_core::project::train::TrainSource::Path => "path",
                }
            );
            println!("root package: Cargo.toml and Cargo.lock are coherent");
            if train.is_published() {
                if app.offline {
                    println!("framework facade: crates.io probe skipped in offline mode");
                } else {
                    match inspect_registry_train(train.version.clone()).await {
                        Ok(RegistryStatus::Available) => {
                            println!("framework facade: available on crates.io");
                        }
                        Ok(RegistryStatus::Yanked) => {
                            println!(
                                "warning: locked framework train {} is yanked; existing locked deployment remains valid, but a new Cargo update will not select it",
                                train.version
                            );
                        }
                        Err(error) => {
                            eprintln!(
                                "warning: could not inspect framework train {} on crates.io: {error:#}",
                                train.version
                            );
                        }
                    }
                }
            }
            Ok(())
        }
    }

    async fn inspect_registry_train(version: String) -> Result<RegistryStatus> {
        run_registry_probe(move || phoxal_cli_project::registry::inspect_registry_train(&version))
            .await
    }

    async fn run_registry_probe<F>(probe: F) -> Result<RegistryStatus>
    where
        F: FnOnce() -> Result<RegistryStatus> + Send + 'static,
    {
        tokio::task::spawn_blocking(probe)
            .await
            .context("crates.io probe worker failed")?
    }

    pub(super) async fn run(app: &AppContext) -> Result<()> {
        Doctor.run(app).await
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[tokio::test]
        async fn blocking_http_client_is_dropped_outside_the_async_runtime() {
            let status = run_registry_probe(|| {
                let _client = reqwest::blocking::Client::new();
                Ok(RegistryStatus::Available)
            })
            .await
            .unwrap();

            assert_eq!(status, RegistryStatus::Available);
        }
    }
}

pub(crate) async fn doctor_command(app: &AppContext) -> Result<()> {
    doctor::run(app).await
}
