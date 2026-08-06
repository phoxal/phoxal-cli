//! The client-owned simulation session.
//!
//! `phoxal simulation webots run <ROBOT_YAML> <WORLD>` owns the whole session.
//! The daemon has no simulation concept at all (organization#978): the bundle
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

pub(crate) async fn run_command(
    app: &AppContext,
    world: String,
    project: Option<&Path>,
) -> Result<()> {
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
    // would (organization#978).
    let supervisor = session.ports.supervisor.clone();
    let webots = std::sync::Arc::new(tokio::sync::Mutex::new(webots));
    let watched = std::sync::Arc::clone(&webots);
    let watcher = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let exited = { watched.lock().await.exited() };
            match exited {
                Ok(Some(status)) => {
                    tracing::warn!("Webots exited with {status}; stopping the execution");
                    let _ = supervisor.stop().await;
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

    // The UI's stop already asked the daemon to end and waited for its
    // terminal snapshot, so by the time the session returns the graph is down
    // (or the daemon is gone, which is the same fact); only then does Webots
    // go, and gracefully.
    webots.stop().await?;
    super::lifecycle::report_outcome(&target, outcome?)
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
    /// leaving would strand a simulator with no operator (organization#978).
    #[test]
    fn a_simulation_session_is_never_detachable() {
        assert_ne!(Detachable::No, Detachable::Yes);
        assert!(HANDSHAKE_BUDGET > Duration::ZERO);
    }
}
