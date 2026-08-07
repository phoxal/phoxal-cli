//! The client-owned simulation session.
//!
//! `phoxal simulation webots run <ROBOT_YAML> <WORLD>` owns the whole session.
//! The daemon has no simulation concept at all: the bundle
//! this command finalizes says `clock: simulated`, has its driver blocks
//! stripped, and stages the simulator, and `phoxald` derives the participant
//! set and the clock source from that manifest exactly as it does for any other
//! bundle. It is launched on the bundle root with no flags.
//!
//! What is genuinely different is lifetime coordination: this client owns the
//! Webots application, so two processes have to end together in either order,
//! and there is no detach - leaving would strand a simulator with no operator.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::daemon::{self, LaunchedDaemon};
use super::lifecycle::Target;
use super::session::{self, Detachable};
use super::webots::Webots;
use crate::attach::Session;
use crate::cli::AppContext;
use crate::lock::{ProjectLock, ProjectLockIdentity, ProjectOperation};

/// How long the daemon is given to answer connect after Webots is up.
const HANDSHAKE_BUDGET: Duration = Duration::from_secs(2 * 60);

/// How long the daemon is given to reach a terminal state once it has been
/// asked to stop because Webots went first. It is bounded rather than
/// open-ended: the world clock is already gone, so waiting forever would only
/// hold the operator's terminal hostage to a wedged daemon.
const TERMINAL_BUDGET: Duration = Duration::from_secs(30);

/// How often the Webots process is polled while the session runs.
const WEBOTS_POLL_INTERVAL: Duration = Duration::from_millis(250);

pub(crate) async fn run_command(
    app: &AppContext,
    world: String,
    project: Option<&Path>,
) -> Result<()> {
    crate::pair::require_exact()?;
    let target = Target::resolve(project, app.project.root())?;
    phoxal_cli_core::project::train::resolve_locked_train(&target.project, app.offline)
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
    // refused while a daemon is executing out of it.
    crate::lock::refuse_while_execution_is_live(&target.project)?;

    // The client finalizes the simulated bundle: `clock: simulated`, driver
    // blocks stripped, simulators staged. The daemon is handed the result and
    // told nothing about it.
    let webots = webots_host()?;
    let runtime_target =
        phoxal_cli_project::resolve_target(Some(&target.project), &target.project)?;
    let (reporter, signal_task) =
        crate::cli::output::progress::cancellable_preparation_reporter(app.ui);
    let prepared =
        phoxal_cli_project::prepare_simulation(phoxal_cli_project::PrepareSimulationRequest {
            target: runtime_target,
            run: phoxal_cli_core::project::launch_plan::RunIdentity::mint_or_adopt(None),
            world,
            offline: app.offline,
            webots,
            reporter,
        })
        .await;
    signal_task.abort();
    let prepared = prepared?;
    let simulation = prepared
        .simulation
        .as_ref()
        .context("simulation preparation returned no simulation data")?;

    app.ui.info(format!(
        "world: {}; bundle: {}",
        simulation.world.display(),
        prepared.staged_root.display()
    ));

    // Webots first: the daemon's graph waits on a world clock that only the
    // simulator produces, so a daemon started first would sit in readiness
    // with nothing yet able to satisfy it.
    let webots_spec = prepared
        .participants
        .iter()
        .find(|participant| participant.kind == phoxal_cli_core::runtime::ParticipantKind::Host)
        .and_then(|participant| participant.launch.as_ref())
        .context("simulation preparation returned no Webots launch spec")?;
    let webots = Webots::launch(webots_spec)?;

    let launched = daemon::spawn(&prepared.staged_root)?;
    let session = await_simulated_attachment(&target, launched, app).await;
    let session = match session {
        Ok(session) => session,
        Err(error) => {
            // The daemon never came up, so there is nothing to stop through
            // the supervisor API - but Webots is this client's and must not be
            // left behind.
            webots.stop().await?;
            return Err(error);
        }
    };

    // Either exit order: if Webots goes first, the execution it was the world
    // clock for has nothing left to run on, so the daemon is asked to stop and
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
                    if plan(&SessionEnd::WebotsExited { status }).stop_daemon {
                        let _ = supervisor.stop().await;
                        if await_terminal(&mut snapshots, TERMINAL_BUDGET)
                            .await
                            .is_err()
                        {
                            tracing::warn!(
                                "the execution did not reach a terminal state within {}s of \
                                 losing its world clock",
                                TERMINAL_BUDGET.as_secs()
                            );
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
    watcher.abort();
    let webots = std::sync::Arc::into_inner(webots)
        .expect("the Webots watcher is aborted before the handle is reclaimed")
        .into_inner();
    let webots_exit = std::sync::Arc::into_inner(webots_exit)
        .expect("the Webots watcher is aborted before its observation is reclaimed")
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

/// Wait for a terminal snapshot, for the feed to end, or for `budget`.
///
/// The feed ending is terminal too: a daemon whose snapshots stopped arriving
/// is a daemon that is no longer executing anything.
async fn await_terminal(
    snapshots: &mut tokio::sync::watch::Receiver<Option<phoxal_supervisor_api::Snapshot>>,
    budget: Duration,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if let Some(snapshot) = snapshots.borrow_and_update().clone()
            && matches!(
                snapshot.lifecycle,
                phoxal_supervisor_api::Lifecycle::Stopped
                    | phoxal_supervisor_api::Lifecycle::Failed
            )
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
    /// The execution ended first: a terminal snapshot, a failure, or a daemon
    /// that vanished with its identity token.
    DaemonEnded { failure: Option<String> },
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
            Ok(phoxal_cli_ui::AttachmentOutcome::ExecutionFailed { reason }) => Self::DaemonEnded {
                failure: reason
                    .as_ref()
                    .map(|failure| format!("{:?}: {}", failure.reason, failure.detail.as_str())),
            },
            Ok(_) => Self::Operator,
            Err(error) => Self::DaemonEnded {
                failure: Some(format!("{error:#}")),
            },
        }
    }
}

/// What a session end requires of this client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Shutdown {
    /// Ask the daemon to stop and await its terminal state within the bound.
    /// Only true when the daemon is still the reachable half.
    pub(crate) stop_daemon: bool,
    /// The unexpected termination to report. An operator-driven end is not
    /// unexpected and reports nothing.
    pub(crate) report: Option<String>,
}

/// The whole shutdown matrix, as one pure decision.
pub(crate) fn plan(end: &SessionEnd) -> Shutdown {
    match end {
        SessionEnd::Operator => Shutdown {
            stop_daemon: false,
            report: None,
        },
        SessionEnd::WebotsExited { status } => Shutdown {
            stop_daemon: true,
            report: Some(format!(
                "Webots exited with {status} before the execution did; the execution was stopped \
                 because the world clock it depends on is gone"
            )),
        },
        SessionEnd::DaemonEnded { failure } => Shutdown {
            stop_daemon: false,
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

/// Wait for the daemon while a simulated bundle's graph comes up.
///
/// A simulated bundle has a longer road to connect than a real one - Webots
/// has to open the world before the world clock exists - so the budget is
/// larger, but the rule is the same: the completed handshake is readiness, and
/// an early child exit is reported with the daemon's own diagnostics.
async fn await_simulated_attachment(
    target: &Target,
    mut launched: LaunchedDaemon,
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
    /// the execution by then: nothing more is asked of the daemon, and there
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
                stop_daemon: false,
                report: None,
            }
        );
    }

    /// Order one: Webots dies first. Its execution has no world clock left, so
    /// the daemon is asked to stop, awaited within a bound, and the operator is
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
            shutdown.stop_daemon,
            "the reachable daemon is asked to stop"
        );
        let report = shutdown
            .report
            .expect("an unexpected termination is reported");
        assert!(report.contains("exit status: 139"), "{report}");
        assert!(report.contains("world clock"), "{report}");
    }

    /// Order two: the daemon dies first. There is nothing left to stop over the
    /// supervisor API, and Webots must not be left behind.
    #[test]
    fn the_daemon_ending_first_asks_nothing_of_it_and_still_reports() {
        let failure = phoxal_supervisor_api::DaemonFailure {
            reason: phoxal_supervisor_api::DaemonFailureReason::WorldClockMissing,
            detail: phoxal_supervisor_api::Detail::new("the world clock never became ready"),
        };
        let end = SessionEnd::observe(
            None,
            Ok(&phoxal_cli_ui::AttachmentOutcome::ExecutionFailed {
                reason: Some(failure),
            }),
        );
        let shutdown = plan(&end);
        assert!(
            !shutdown.stop_daemon,
            "a daemon that already ended is not asked to stop"
        );
        let report = shutdown
            .report
            .expect("an unexpected termination is reported");
        assert!(report.contains("WorldClockMissing"), "{report}");
        assert!(report.contains("stopping Webots"), "{report}");
    }

    /// A session that never produced an outcome at all - the attachment itself
    /// failed - is still the daemon-ended half of the matrix, not a silent exit.
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
