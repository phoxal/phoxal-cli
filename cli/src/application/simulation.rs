//! The client-owned simulation session.
//!
//! `phoxal simulation webots run <PROJECT> <WORLD>` owns the whole session.
//! The supervisor has no source-level simulation concept: this command compiles a
//! simulated-clock bundle and stages Webots, while `phoxal-supervisor` validates and
//! executes the exact participant set already persisted in `runtime.json`.
//!
//! What is genuinely different is lifetime coordination: this client owns the
//! Webots application, so two processes have to end together in either order,
//! and there is no detach - leaving would strand a simulator with no operator.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use phoxal_client::supervisor::execution::{CommandOutcome, Lifecycle, Snapshot};

use super::lifecycle::Target;
use super::session::{self, Detachable};
use super::supervisor::{self, LaunchedSupervisor};
use super::webots::Webots;
use crate::attach::Session;
use crate::cli::context::AppContext;
use crate::lock::{ProjectLock, ProjectLockIdentity, ProjectOperation};

/// How long the supervisor is given to answer connect after Webots is up.
const HANDSHAKE_BUDGET: Duration = Duration::from_secs(2 * 60);

/// How long the supervisor is given to reach a terminal state once it has been
/// asked to stop because Webots went first. It is bounded rather than
/// open-ended: the world clock is already gone, so waiting forever would only
/// hold the operator's terminal hostage to a wedged supervisor.
const TERMINAL_BUDGET: Duration = Duration::from_secs(30);

/// How often the Webots process is polled while the session runs.
const WEBOTS_POLL_INTERVAL: Duration = Duration::from_millis(250);

pub(crate) async fn run_command(
    app: &AppContext,
    world: String,
    project: Option<&Path>,
) -> Result<()> {
    let target = Target::resolve(project, app.project.root())?;
    phoxal_cli_project::source::train::resolve_locked_train(&target.project, app.offline)
        .with_context(|| {
            format!(
                "simulation requires a buildable source project; {} is not a source project",
                target.project.display()
            )
        })?;

    let _lock = ProjectLock::acquire(ProjectLockIdentity::resolve(
        &target.project,
        ProjectOperation::Run,
    ))
    .context("failed to acquire the project build lock for simulation")?;
    // A simulation run finalizes and publishes this project's bundle, so it is
    // refused while a supervisor is executing out of it.
    crate::lock::refuse_while_execution_is_live(&target.project)?;

    // The client finalizes the simulated bundle: `clock: simulated`, driver
    // blocks stripped, simulators staged. The supervisor is handed the result and
    // told nothing about it.
    let webots = webots_host()?;
    let runtime_target =
        phoxal_cli_project::resolve_target(Some(&target.project), &target.project)?;
    let (reporter, signal_task) =
        crate::cli::output::progress::cancellable_preparation_reporter(app.ui);
    let offline = app.offline;
    let prepared = tokio::task::spawn_blocking(move || {
        phoxal_cli_project::prepare_simulation(phoxal_cli_project::PrepareSimulationRequest {
            target: runtime_target,
            world,
            offline,
            webots,
            reporter,
        })
    })
    .await?;
    signal_task.abort();
    let prepared = prepared?;
    let simulation = prepared
        .simulation
        .as_ref()
        .context("simulation preparation returned no simulation data")?;

    app.ui.info(format!(
        "world source: {}; release: {}",
        simulation.world_source.display(),
        prepared.release.root.display()
    ));

    // The supervisor owns execution identity. Start and attach first, then derive
    // the Webots controller launch from that exact execution and the verified
    // runtime bundle. The supervisor may wait for the external simulator's
    // readiness while its supervisor API remains attachable.
    let launched = supervisor::spawn(&prepared.release)?;
    let session = await_simulated_attachment(&target, launched, app).await;
    let session = match session {
        Ok(session) => session,
        Err(error) => {
            return Err(error);
        }
    };
    let stage_request = phoxal_cli_project::StageWebotsRequest {
        staged_root: prepared.release.bundle.clone(),
        project_root: simulation.project_root.clone(),
        world_source: simulation.world_source.clone(),
        webots_executable: simulation.webots_executable.clone(),
        execution: session.connected().execution,
        endpoint: target.endpoint.clone(),
    };
    let launch =
        tokio::task::spawn_blocking(move || phoxal_cli_project::stage_webots(stage_request))
            .await?;
    let launch = match launch {
        Ok(launch) => launch,
        Err(error) => {
            stop_failed_simulation_start(session).await;
            return Err(error.context("failed to stage Webots after attaching to the execution"));
        }
    };
    app.ui
        .info(format!("staged world: {}", launch.world.display()));
    let webots = match Webots::launch(&launch) {
        Ok(webots) => webots,
        Err(error) => {
            stop_failed_simulation_start(session).await;
            return Err(error);
        }
    };

    // Either exit order: if Webots goes first, the execution it was the world
    // clock for has nothing left to run on, so the supervisor is asked to stop and
    // the session ends on its terminal snapshot exactly as a confirmed stop
    // would.
    let supervisor = session.ports.supervisor.clone();
    let mut snapshots = session.snapshots();
    let webots = std::sync::Arc::new(tokio::sync::Mutex::new(webots));
    let watched = std::sync::Arc::clone(&webots);
    let webots_exit = std::sync::Arc::new(std::sync::Mutex::new(Option::<String>::None));
    let observed = std::sync::Arc::clone(&webots_exit);
    let watcher = tokio::spawn(async move {
        loop {
            tokio::time::sleep(WEBOTS_POLL_INTERVAL).await;
            let exited = { watched.lock().await.exited() };
            match exited {
                Ok(Some(status)) => {
                    let status = status.to_string();
                    tracing::warn!("Webots exited with {status}; stopping the execution");
                    *observed
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(status.clone());
                    if plan(&SessionEnd::WebotsExited { status }).stop_supervisor {
                        match supervisor.stop().await {
                            Ok(CommandOutcome::Accepted { .. }) => {
                                if await_terminal(&mut snapshots, TERMINAL_BUDGET)
                                    .await
                                    .is_err()
                                {
                                    tracing::warn!(
                                        "the execution did not reach a terminal state within {}s \
                                         of losing its world clock",
                                        TERMINAL_BUDGET.as_secs()
                                    );
                                }
                            }
                            Ok(CommandOutcome::Rejected { reason }) => {
                                tracing::warn!(?reason, "the supervisor rejected the Webots stop");
                            }
                            Err(error) => {
                                tracing::warn!(%error, "the Webots stop request failed");
                            }
                        }
                    }
                    return;
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!("failed to poll the Webots process: {error:#}");
                    return;
                }
            }
        }
    });

    let outcome = session::drive(app, &target.project, session, Detachable::No).await;
    // Aborting only *requests* cancellation: until the task has actually been
    // joined it may still be holding its clones of the two Arcs, and
    // reclaiming them would then yield `None`. Awaiting the aborted handle is
    // what turns the reclaim below into a real invariant - and it is what
    // keeps the graceful Webots stop on the path at all.
    watcher.abort();
    let _ = watcher.await;
    let webots = std::sync::Arc::into_inner(webots)
        .ok_or_else(|| anyhow::anyhow!("Webots process handle still has an owner after shutdown"))?
        .into_inner();
    let webots_exit = std::sync::Arc::into_inner(webots_exit)
        .ok_or_else(|| {
            anyhow::anyhow!("Webots exit observation still has an owner after shutdown")
        })?
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let end = SessionEnd::observe(webots_exit, outcome.as_ref());
    if let Some(report) = plan(&end).report {
        app.ui.warn(report);
    }

    // Whichever way the session ended, Webots is this client's and goes last:
    // gracefully, with the explicit kill only if it will not go.
    webots.stop().await?;
    super::lifecycle::report_outcome(&target, outcome?)
}

async fn stop_failed_simulation_start(session: Session) {
    let mut snapshots = session.snapshots();
    match session.ports.supervisor.stop().await {
        Ok(CommandOutcome::Accepted { .. }) => {
            if let Err(error) = await_terminal(&mut snapshots, TERMINAL_BUDGET).await {
                tracing::warn!(%error, "failed simulation did not reach a terminal state");
            }
        }
        Ok(CommandOutcome::Rejected { reason }) => {
            tracing::warn!(?reason, "the supervisor rejected failed-simulation cleanup");
        }
        Err(error) => tracing::warn!(%error, "failed-simulation cleanup request failed"),
    }
    session.shutdown().await;
}

/// Wait for a terminal snapshot, for the feed to end, or for `budget`.
///
/// The feed ending is terminal too: a supervisor whose snapshots stopped arriving
/// is a supervisor that is no longer executing anything.
async fn await_terminal(
    snapshots: &mut tokio::sync::watch::Receiver<Option<Snapshot>>,
    budget: Duration,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if let Some(snapshot) = snapshots.borrow_and_update().clone()
            && matches!(snapshot.lifecycle, Lifecycle::Stopped | Lifecycle::Failed)
        {
            return Ok(());
        }
        tokio::select! {
            changed = snapshots.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                bail!("the execution did not reach a terminal state within the bound")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// shutdown coordination
// ---------------------------------------------------------------------------

/// What ended a simulation session, in the order it actually happened.
///
/// There is no detach here, so every path ends the whole session; what differs
/// is which of the two processes went first and therefore what is left to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionEnd {
    /// The operator ended it: `q`, or a confirmed Ctrl+C stop, or an external
    /// SIGTERM/SIGHUP - the UI turns all three into the same stop for a
    /// non-detachable session, and it has already waited for the execution's
    /// terminal snapshot by the time the session returns.
    Operator,
    /// Webots exited before the execution did.
    WebotsExited { status: String },
    /// The execution ended first: a terminal snapshot, a failure, or a supervisor
    /// that vanished with its identity token.
    SupervisorEnded { failure: Option<String> },
}

impl SessionEnd {
    /// Classify the end from the two facts the session leaves behind: whether
    /// the watcher saw Webots exit, and how the attachment itself ended.
    pub(crate) fn observe(
        webots_exit: Option<String>,
        outcome: Result<&phoxal_cli_ui::AttachmentOutcome, &anyhow::Error>,
    ) -> Self {
        if let Some(status) = webots_exit {
            return Self::WebotsExited { status };
        }
        match outcome {
            Ok(phoxal_cli_ui::AttachmentOutcome::ExecutionFailed { reason }) => {
                Self::SupervisorEnded {
                    failure: reason.as_ref().map(|failure| {
                        format!("{:?}: {}", failure.reason, failure.detail.as_str())
                    }),
                }
            }
            Ok(_) => Self::Operator,
            Err(error) => Self::SupervisorEnded {
                failure: Some(format!("{error:#}")),
            },
        }
    }
}

/// What a session end requires of this client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Shutdown {
    /// Ask the supervisor to stop and await its terminal state within the bound.
    /// Only true when the supervisor is still the reachable half.
    pub(crate) stop_supervisor: bool,
    /// The unexpected termination to report. An operator-driven end is not
    /// unexpected and reports nothing.
    pub(crate) report: Option<String>,
}

/// The whole shutdown matrix, as one pure decision.
pub(crate) fn plan(end: &SessionEnd) -> Shutdown {
    match end {
        SessionEnd::Operator => Shutdown {
            stop_supervisor: false,
            report: None,
        },
        SessionEnd::WebotsExited { status } => Shutdown {
            stop_supervisor: true,
            report: Some(format!(
                "Webots exited with {status} before the execution did; the execution was stopped \
                 because the world clock it depends on is gone"
            )),
        },
        SessionEnd::SupervisorEnded { failure } => Shutdown {
            stop_supervisor: false,
            report: Some(match failure {
                Some(failure) => {
                    format!("the execution ended before Webots did ({failure}); stopping Webots")
                }
                None => "the execution ended before Webots did; stopping Webots".to_string(),
            }),
        },
    }
}

/// Resolve the Webots installation this session will drive.
fn webots_host() -> Result<phoxal_cli_project::WebotsHost> {
    phoxal_cli_project::host::doctor::preflight()
        .map_err(|error| anyhow::anyhow!("{error}"))
        .context("Webots preflight failed; simulation cannot launch the simulator")?;
    let executable = phoxal_cli_project::host::doctor::webots_executable_path()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let home = phoxal_cli_project::host::doctor::webots_home_path()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok(phoxal_cli_project::WebotsHost {
        executable,
        home: Some(home),
    })
}

/// Wait for the supervisor while a simulated bundle's graph comes up.
///
/// A simulated bundle has a longer road to connect than a real one - Webots
/// has to open the world before the world clock exists - so the budget is
/// larger, but the rule is the same: the completed handshake is readiness, and
/// an early child exit is reported with the supervisor's own diagnostics.
async fn await_simulated_attachment(
    target: &Target,
    mut launched: LaunchedSupervisor,
    app: &AppContext,
) -> Result<Session> {
    let deadline = tokio::time::Instant::now() + HANDSHAKE_BUDGET;
    loop {
        if let Some(status) = launched.exited()? {
            bail!(launched.early_exit_message(status));
        }
        if let Ok(session) =
            Session::open(&target.endpoint, target.project.display().to_string()).await
        {
            return Ok(session);
        }
        if tokio::time::Instant::now() >= deadline {
            if let Some(status) = launched.exited()? {
                bail!(launched.early_exit_message(status));
            }
            let diagnostics = launched.diagnostics();
            app.ui.warn(diagnostics.trim());
            bail!(
                "timed out after {}s waiting for the simulated execution to answer at {}",
                HANDSHAKE_BUDGET.as_secs(),
                target.endpoint
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod tests {
    use phoxal_client::supervisor::execution::{
        Detail, SupervisorFailure, SupervisorFailureReason,
    };

    use super::*;

    /// A simulation session is never detachable: this client owns Webots, so
    /// leaving would strand a simulator with no operator.
    #[test]
    fn a_simulation_session_is_never_detachable() {
        assert_ne!(Detachable::No, Detachable::Yes);
        assert!(HANDSHAKE_BUDGET > Duration::ZERO);
        assert!(TERMINAL_BUDGET > Duration::ZERO);
    }

    /// `q`, a confirmed stop, and an external termination all reach this
    /// client as the same operator-driven end, and the UI has already stopped
    /// the execution by then: nothing more is asked of the supervisor, and there
    /// is nothing unexpected to report.
    #[test]
    fn an_operator_driven_end_stops_nothing_further_and_reports_nothing() {
        let end = SessionEnd::observe(
            None,
            Ok(&phoxal_cli_ui::AttachmentOutcome::ExecutionStopped),
        );
        assert_eq!(end, SessionEnd::Operator);
        assert_eq!(
            plan(&end),
            Shutdown {
                stop_supervisor: false,
                report: None,
            }
        );
    }

    /// Order one: Webots dies first. Its execution has no world clock left, so
    /// the supervisor is asked to stop, awaited within a bound, and the operator is
    /// told why the session ended.
    #[test]
    fn webots_exiting_first_stops_the_execution_and_reports_it() {
        let end = SessionEnd::observe(
            Some("exit status: 139".to_string()),
            Ok(&phoxal_cli_ui::AttachmentOutcome::ExecutionStopped),
        );
        assert_eq!(
            end,
            SessionEnd::WebotsExited {
                status: "exit status: 139".to_string()
            }
        );
        let shutdown = plan(&end);
        assert!(
            shutdown.stop_supervisor,
            "the reachable supervisor is asked to stop"
        );
        let report = shutdown
            .report
            .expect("an unexpected termination is reported");
        assert!(report.contains("exit status: 139"), "{report}");
        assert!(report.contains("world clock"), "{report}");
    }

    /// Order two: the supervisor dies first. There is nothing left to stop over the
    /// supervisor API, and Webots must not be left behind.
    #[test]
    fn the_supervisor_ending_first_asks_nothing_of_it_and_still_reports() {
        let failure = SupervisorFailure {
            reason: SupervisorFailureReason::ControlPlaneLost,
            detail: Detail::new("the world clock never became ready"),
        };
        let end = SessionEnd::observe(
            None,
            Ok(&phoxal_cli_ui::AttachmentOutcome::ExecutionFailed {
                reason: Some(failure),
            }),
        );
        let shutdown = plan(&end);
        assert!(
            !shutdown.stop_supervisor,
            "a supervisor that already ended is not asked to stop"
        );
        let report = shutdown
            .report
            .expect("an unexpected termination is reported");
        assert!(report.contains("ControlPlaneLost"), "{report}");
        assert!(report.contains("stopping Webots"), "{report}");
    }

    /// A session that never produced an outcome at all - the attachment itself
    /// failed - is still the supervisor-ended half of the matrix, not a silent exit.
    #[test]
    fn a_failed_session_is_classified_and_reported_rather_than_swallowed() {
        let error = anyhow::anyhow!("the supervisor's identity token was lost");
        let end = SessionEnd::observe(None, Err(&error));
        let report = plan(&end).report.expect("a failure is reported");
        assert!(report.contains("identity token"), "{report}");
    }

    /// Webots exiting wins over whatever the attachment reported: it is the
    /// earlier fact, and it is what made the execution stoppable at all.
    #[test]
    fn a_webots_exit_is_the_end_even_when_the_attachment_also_reported_a_failure() {
        let end = SessionEnd::observe(
            Some("signal: 15".to_string()),
            Ok(&phoxal_cli_ui::AttachmentOutcome::ExecutionFailed { reason: None }),
        );
        assert!(matches!(end, SessionEnd::WebotsExited { .. }), "{end:?}");
    }
}
