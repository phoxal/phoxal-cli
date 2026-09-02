//! The client-owned simulation session.
//!
//! `phoxal simulation webots run <PROJECT> <WORLD>` runs the same bundle
//! `phoxal run` does - byte for byte, from the same staging pass - and differs
//! in one launch decision: it starts no component driver. The participant
//! processes learn their time domain from the supervisor after attachment.
//! Webots supplies the components instead, through a controller that declares
//! one liveliness token per component instance it simulates.
//!
//! What is genuinely different is lifetime coordination: this client owns the
//! Webots application as well as the session, so two things have to end
//! together in either order, and there is no detach - leaving would strand a
//! simulator with no operator.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

use super::launcher::OwnedSession;
use super::lifecycle::{RunOptions, Target, await_session_ready, launch_execution};
use super::session::{self, Detachable, SessionOwnership};
use super::startup::Startup;
use super::summary::SessionSummary;
use super::webots::Webots;
use crate::cli::context::AppContext;
use crate::cli::exit::ReportedExit;
use crate::cli::output::welcome::{Mode, StepId};

/// How often the Webots process is polled while the session runs.
const WEBOTS_POLL_INTERVAL: Duration = Duration::from_millis(250);

pub(crate) async fn run_command(
    app: &AppContext,
    world: String,
    project: Option<&Path>,
) -> Result<()> {
    let target = Target::resolve(project, app.project.root())?;
    let startup = Startup::begin(app, &target.project, Mode::Webots);
    let started = match start_simulation(app, &target, world, &startup).await {
        Ok(started) => started,
        Err(error) => return Err(startup.failed(error)),
    };
    // The dashboard takes the terminal only from here: both processes are up
    // and the runtimes are present.
    startup.ready();
    drive_simulation(app, &target, started).await
}

/// Everything one simulation session owns for its whole life.
struct StartedSimulation {
    launched: super::lifecycle::LaunchedSession,
    webots: Webots,
}

/// Launch the session, stage Webots against the bundle it published, and only
/// then wait for the runtimes.
///
/// The wait has to come last. A simulated runtime follows the world clock the
/// controller publishes, and the controller is what presents the component
/// instances a driver would otherwise present - the expected set, exactly - so
/// nothing appears until Webots is open and stepping.
/// Staging needs the published bundle and Webots needs the endpoint, both of
/// which exist the moment the launch returns.
async fn start_simulation(
    app: &AppContext,
    target: &Target,
    world: String,
    startup: &Startup,
) -> Result<StartedSimulation> {
    let webots_host = webots_host()?;
    let runtime_target =
        phoxal_cli_project::resolve_target(Some(&target.project), &target.project)?;
    let reporter = startup.reporter();
    let offline = app.offline;
    let simulation = tokio::task::spawn_blocking(move || {
        phoxal_cli_project::prepare_simulation(phoxal_cli_project::PrepareSimulationRequest {
            target: runtime_target,
            world,
            offline,
            webots: webots_host,
            reporter,
        })
    })
    .await??;

    // Drivers are never started here, and the flags that would ask for them do
    // not exist on this command: Webots simulates the components instead.
    let launched = launch_execution(
        app,
        target,
        &RunOptions {
            drivers_off: false,
            drivers_subset: Vec::new(),
        },
        startup,
        true,
    )
    .await?;

    startup.step(StepId::Webots, "staging the world");
    let stage_request = phoxal_cli_project::StageWebotsRequest {
        bundle_root: launched.bundle().to_path_buf(),
        project_root: simulation.project_root.clone(),
        world_source: simulation.world_source.clone(),
        webots_executable: simulation.webots_executable.clone(),
        controller: simulation.controller.clone(),
        endpoint: target.endpoint.clone(),
    };
    let started = async {
        let launch =
            tokio::task::spawn_blocking(move || phoxal_cli_project::stage_webots(stage_request))
                .await
                .context("the Webots staging worker failed")??;
        let world_name = launch
            .world
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("world")
            .to_string();
        startup.detail(StepId::Webots, format!("world {world_name}"));
        let webots = Webots::launch(&launch, &target.paths().webots_log())?;
        startup.complete(StepId::Webots, format!("world {world_name} · launched"));
        Ok::<_, anyhow::Error>(webots)
    }
    .await;
    let webots = match started {
        Ok(webots) => webots,
        Err(error) => return Err(end_launched(launched, error).await),
    };

    match await_session_ready(launched, startup).await {
        Ok(launched) => Ok(StartedSimulation { launched, webots }),
        Err(error) => {
            let stopped = webots.stop().await;
            Err(match stopped {
                Ok(()) => error,
                Err(stopped) => error.context(format!("closing Webots also failed: {stopped:#}")),
            })
        }
    }
}

/// End a session that was launched but never became a simulation.
async fn end_launched(
    launched: super::lifecycle::LaunchedSession,
    error: anyhow::Error,
) -> anyhow::Error {
    let cleanup = launched.owned().stop().await;
    launched.children.abort();
    launched.session.shutdown().await;
    match cleanup {
        Ok(()) => error,
        Err(cleanup) => error.context(format!("session cleanup also failed: {cleanup:#}")),
    }
}

async fn drive_simulation(
    app: &AppContext,
    target: &Target,
    started: StartedSimulation,
) -> Result<()> {
    let StartedSimulation { launched, webots } = started;
    let owned = launched.owned();

    // Either exit order. If Webots goes first, the runtimes it was the world
    // clock for have nothing left to step on, so the session is ended and the
    // dashboard leaves exactly as a confirmed stop would end it.
    let webots = std::sync::Arc::new(tokio::sync::Mutex::new(webots));
    let webots_exit = std::sync::Arc::new(std::sync::Mutex::new(Option::<String>::None));
    let watcher = tokio::spawn(watch_webots(
        std::sync::Arc::clone(&webots),
        std::sync::Arc::clone(&webots_exit),
        owned.clone(),
    ));

    let outcome = session::drive(
        app,
        &target.project,
        launched.session,
        Detachable::No,
        SessionOwnership {
            owned: Some(owned.clone()),
            supervisor_exit: None,
        },
    )
    .await;
    launched.children.abort();
    let _ = launched.children.await;
    watcher.abort();
    let _ = watcher.await;

    // Whichever way the session ended, both halves are this client's and both
    // go down: the session first, then Webots, so the controller is not left
    // publishing a world clock into an execution that has gone.
    let stopped_session = owned.stop().await;
    let webots = std::sync::Arc::into_inner(webots)
        .context("the Webots process handle still has an owner after shutdown")?
        .into_inner();
    let stopped_webots = webots.stop().await;
    let webots_exit = std::sync::Arc::into_inner(webots_exit)
        .context("the Webots exit observation still has an owner after shutdown")?
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let end = SessionEnd::observe(webots_exit, outcome.as_ref());
    let result = finish(outcome.map(|_| ()), stopped_session, stopped_webots);

    // One account of the ending, printed only now: the dashboard has released
    // the terminal, and both processes are already down.
    let paths = target.paths();
    let ending = match &result {
        Ok(()) => report(&end),
        Err(error) => format!("{error:#}"),
    };
    SessionSummary::new(ending, vec![paths.supervisor_log(), paths.webots_log()])
        .print(&target.project, app.output.theme);
    result.map_err(|_| ReportedExit(1).into())
}

/// End the session as soon as Webots goes, and remember that it did.
async fn watch_webots(
    webots: std::sync::Arc<tokio::sync::Mutex<Webots>>,
    observed: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    owned: OwnedSession,
) {
    loop {
        tokio::time::sleep(WEBOTS_POLL_INTERVAL).await;
        let exited = { webots.lock().await.exited() };
        match exited {
            Ok(Some(status)) => {
                let status = status.to_string();
                tracing::warn!("Webots exited with {status}; ending the session");
                *observed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(status);
                if let Err(error) = owned.stop().await {
                    tracing::warn!(%error, "ending the session after Webots exited failed");
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
}

/// Fold the session's outcome and both cleanups into one result.
fn finish(outcome: Result<()>, session: Result<()>, webots: Result<()>) -> Result<()> {
    let mut result = outcome;
    for (label, cleanup) in [("session", session), ("Webots", webots)] {
        result = match (result, cleanup) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) => Err(error),
            (Ok(()), Err(error)) => Err(error.context(format!("{label} cleanup failed"))),
            (Err(error), Err(cleanup)) => {
                Err(error.context(format!("{label} cleanup also failed: {cleanup:#}")))
            }
        };
    }
    result
}

// ---------------------------------------------------------------------------
// shutdown coordination
// ---------------------------------------------------------------------------

/// What ended a simulation session, in the order it actually happened.
///
/// There is no detach here, so every path ends the whole session; what differs
/// is which of the two halves went first and therefore what the operator is
/// told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionEnd {
    /// The operator ended it: `q`, `S`, a confirmed Ctrl+C, or an external
    /// SIGTERM/SIGHUP - the UI turns all of them into the same stop for a
    /// non-detachable session.
    Operator,
    /// Webots exited before the session did.
    WebotsExited { status: String },
    /// The execution went away first: the supervisor this client launched
    /// exited, or its identity token disappeared.
    ExecutionEnded { reason: Option<String> },
}

impl SessionEnd {
    /// Classify the end from the two facts the session leaves behind: whether
    /// the watcher saw Webots exit, and how the dashboard itself ended.
    pub(crate) fn observe(
        webots_exit: Option<String>,
        outcome: Result<&phoxal_cli_ui::AttachmentOutcome, &anyhow::Error>,
    ) -> Self {
        if let Some(status) = webots_exit {
            return Self::WebotsExited { status };
        }
        match outcome {
            Ok(phoxal_cli_ui::AttachmentOutcome::ExecutionEnded { reason }) => {
                Self::ExecutionEnded {
                    reason: reason.clone(),
                }
            }
            Ok(_) => Self::Operator,
            Err(error) => Self::ExecutionEnded {
                reason: Some(format!("{error:#}")),
            },
        }
    }
}

/// What an operator is told about a session that ended cleanly.
pub(crate) fn report(end: &SessionEnd) -> String {
    match end {
        SessionEnd::Operator => {
            "you ended it; the runtimes were stopped and Webots was closed".to_string()
        }
        SessionEnd::WebotsExited { status } => format!(
            "Webots exited with {status} before the session did; the runtimes were stopped \
             because the world clock they follow is gone"
        ),
        SessionEnd::ExecutionEnded { reason } => match reason {
            Some(reason) => {
                format!("the execution ended before Webots did ({reason}); Webots was closed")
            }
            None => "the execution ended before Webots did; Webots was closed".to_string(),
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
    Ok(phoxal_cli_project::WebotsHost { executable, home })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `q`, `S`, a confirmed Ctrl+C and an external termination all reach this
    /// client as the same operator-driven end, and the report says so without
    /// blaming anything.
    #[test]
    fn an_operator_driven_end_reports_what_the_operator_did() {
        for outcome in [
            phoxal_cli_ui::AttachmentOutcome::SessionStopped,
            phoxal_cli_ui::AttachmentOutcome::Detached,
        ] {
            let end = SessionEnd::observe(None, Ok(&outcome));
            assert_eq!(end, SessionEnd::Operator);
            assert!(report(&end).contains("you ended it"), "{end:?}");
        }
    }

    /// Order one: Webots dies first. The runtimes have no world clock left, so
    /// the session is ended and the operator is told why.
    #[test]
    fn webots_exiting_first_ends_the_session_and_reports_it() {
        let end = SessionEnd::observe(
            Some("exit status: 139".to_string()),
            Ok(&phoxal_cli_ui::AttachmentOutcome::SessionStopped),
        );
        assert_eq!(
            end,
            SessionEnd::WebotsExited {
                status: "exit status: 139".to_string()
            }
        );
        let report = report(&end);
        assert!(report.contains("exit status: 139"), "{report}");
        assert!(report.contains("world clock"), "{report}");
    }

    /// Order two: the execution goes first. Webots must not be left behind, and
    /// the supervisor's own account of why survives.
    #[test]
    fn the_execution_ending_first_still_closes_webots_and_keeps_the_reason() {
        let end = SessionEnd::observe(
            None,
            Ok(&phoxal_cli_ui::AttachmentOutcome::ExecutionEnded {
                reason: Some("phoxal-supervisor exited with exit status: 1".to_string()),
            }),
        );
        let report = report(&end);
        assert!(report.contains("exit status: 1"), "{report}");
        assert!(report.contains("Webots was closed"), "{report}");
    }

    /// A session that never produced an outcome at all is still classified and
    /// reported, never silently swallowed.
    #[test]
    fn a_failed_session_is_classified_and_reported_rather_than_swallowed() {
        let error = anyhow::anyhow!("the supervisor's identity token was lost");
        let end = SessionEnd::observe(None, Err(&error));
        assert!(report(&end).contains("identity token"), "{end:?}");
    }

    /// Webots exiting wins over whatever the dashboard reported: it is the
    /// earlier fact, and it is what ended the session at all.
    #[test]
    fn a_webots_exit_is_the_end_even_when_the_dashboard_also_reported_one() {
        let end = SessionEnd::observe(
            Some("signal: 15".to_string()),
            Ok(&phoxal_cli_ui::AttachmentOutcome::ExecutionEnded { reason: None }),
        );
        assert!(matches!(end, SessionEnd::WebotsExited { .. }), "{end:?}");
    }

    /// Neither cleanup may hide the session's own failure, and neither may be
    /// silently dropped when the session itself was fine.
    #[test]
    fn no_cleanup_failure_is_hidden_and_none_hides_the_session_failure() {
        let error = finish(
            Err(anyhow::anyhow!("the session failed")),
            Err(anyhow::anyhow!("a runtime survived SIGKILL")),
            Err(anyhow::anyhow!("Webots cleanup failed")),
        )
        .expect_err("every failure must remain a failure");
        let report = format!("{error:#}");
        assert!(report.contains("the session failed"), "{report}");
        assert!(report.contains("survived SIGKILL"), "{report}");
        assert!(report.contains("Webots cleanup failed"), "{report}");

        let error = finish(
            Ok(()),
            Ok(()),
            Err(anyhow::anyhow!("Webots cleanup failed")),
        )
        .expect_err("a cleanup failure must not become success");
        let report = format!("{error:#}");
        assert!(report.contains("Webots cleanup failed"), "{report}");

        assert!(finish(Ok(()), Ok(()), Ok(())).is_ok());
    }

    /// A simulation session is never detachable: this client owns Webots, so
    /// leaving would strand a simulator with no operator.
    #[test]
    fn a_simulation_session_is_never_detachable() {
        assert_ne!(Detachable::No, Detachable::Yes);
    }
}
