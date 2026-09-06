//! Backend-neutral local world lifecycle and robot connection workflow.

mod connect;
mod observation;
mod start;
mod terminal;

#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use phoxal::model::identity::SpawnId;
use phoxal::session::WorldSessionClient;
use phoxal::version::FrameworkVersion;
use phoxal::world::api::session::connect::WorldSessionBootstrap;
use phoxal::world::api::session::control::WorldControl;
use phoxal::world::api::session::state::WorldSessionState;
use phoxal::world::api::session::{
    WorldLifecycle, WorldMemberCleanup, WorldMemberEndReason, WorldMemberPhase, WorldMotion,
};
use phoxal_cli_host::world::{
    DEFAULT_LOG_BYTE_LIMIT, DEFAULT_TERMINAL_SESSION_LIMIT, LocalWorldRegistration,
    TerminalOutcome, WorldEvidence, WorldMemberEvidence, WorldPaths, WorldRegistry,
    WorldTerminalSummary,
};

use crate::cli::context::AppContext;
use crate::cli::exit::ReportedExit;
use crate::cli::output::simulation as simulation_output;
use crate::cli::output::welcome::{Mode, StepId};

use super::summary::SessionSummary;
use connect::{connect_verified, connect_world, connection_intent};
use observation::{
    StatusReport, ensure_state_matches_registration, load_list, load_logs, load_status, stop_world,
};
use phoxal_cli_host::world_process::LaunchedWorldHost;
use start::{StartedWorld, start_world};
use terminal::open_world_tui;

use simulation_output::lifecycle as lifecycle_text;
#[cfg(test)]
use simulation_output::live_status as format_live_status;
#[cfg(test)]
use terminal::{spawn_diagnostics_feed, spawn_state_feed};

const WORLD_UI_INGRESS_CAPACITY: usize = 64;
const ATTACHMENT_BUDGET: Duration = Duration::from_secs(5 * 60);
const STOP_BUDGET: Duration = Duration::from_secs(60);
const TERMINAL_POLL_INTERVAL: Duration = Duration::from_millis(100);
const STREAM_RECONNECT_DELAY: Duration = Duration::from_millis(100);

pub(crate) async fn run_command(app: &AppContext, world: &Path, spawn: Option<&str>) -> Result<()> {
    let intent = connection_intent(app, spawn, false)?;
    let started = start_world(app, world).await?;
    connect_world(
        app,
        &started.registration.instance.to_string(),
        intent,
        Some(started),
    )
    .await
}

pub(crate) async fn start_command(app: &AppContext, world: &Path) -> Result<()> {
    let started = start_world(app, world).await?;
    let instance = started.registration.instance.to_string();
    started.host.detach();
    app.ui.info(format!(
        "world instance {instance} ready and paused; open with `phoxal simulation open {instance}` or stop with `phoxal simulation stop {instance}`"
    ));
    Ok(())
}

pub(crate) async fn open_command(app: &AppContext, instance: &str) -> Result<()> {
    let stores = Stores::discover()?;
    let registration = stores.registry.resolve(instance)?;
    open_world_tui(app, registration).await
}

pub(crate) async fn connect_command(
    app: &AppContext,
    instance: &str,
    spawn: Option<&str>,
    detach: bool,
) -> Result<()> {
    connect_world(app, instance, connection_intent(app, spawn, detach)?, None).await
}

pub(crate) async fn status_command(_app: &AppContext, instance: &str) -> Result<()> {
    let stores = Stores::discover()?;
    match load_status(&stores, instance).await? {
        StatusReport::Live(state) => println!("{}", simulation_output::live_status(&state)),
        StatusReport::Terminal { summary, members } => {
            println!("{}", simulation_output::terminal_status(&summary, &members));
        }
    }
    Ok(())
}

pub(crate) async fn logs_command(_app: &AppContext, instance: &str) -> Result<()> {
    let stores = Stores::discover()?;
    let logs = load_logs(&stores, instance).await?;
    if logs.is_empty() {
        eprint!("{}", simulation_output::logs(&logs));
    } else {
        print!("{}", simulation_output::logs(&logs));
    }
    Ok(())
}

pub(crate) async fn list_command(_app: &AppContext, all: bool) -> Result<()> {
    let stores = Stores::discover()?;
    let report = load_list(&stores, all).await?;
    println!(
        "{}",
        simulation_output::list(&report.live, &report.terminal, all)
    );
    Ok(())
}

pub(crate) async fn stop_command(_app: &AppContext, instance: &str) -> Result<()> {
    let stores = Stores::discover()?;
    let registration = stores.registry.resolve(instance)?;
    stop_world(registration, &stores).await?;
    println!("stopped world {instance}");
    Ok(())
}

pub(crate) async fn pause_command(_app: &AppContext, instance: &str) -> Result<()> {
    control_world(instance, WorldControl::Pause).await
}

pub(crate) async fn resume_command(_app: &AppContext, instance: &str) -> Result<()> {
    control_world(instance, WorldControl::Resume).await
}

async fn control_world(instance: &str, operation: WorldControl) -> Result<()> {
    let stores = Stores::discover()?;
    let registration = stores.registry.resolve(instance)?;
    let client = connect_verified(&registration).await?;
    let state = client.control(operation).await?;
    ensure_state_matches_registration(&state, &registration)?;
    println!("{}", simulation_output::live_status(&state));
    Ok(())
}

struct Stores {
    paths: WorldPaths,
    registry: WorldRegistry,
    evidence: WorldEvidence,
}

impl Stores {
    fn discover() -> Result<Self> {
        Ok(Self::at(WorldPaths::discover()?))
    }

    fn at(paths: WorldPaths) -> Self {
        Self {
            registry: WorldRegistry::new(
                paths.clone(),
                phoxal_cli_host::world::SystemProcessInspector,
            ),
            evidence: WorldEvidence::new(paths.clone()),
            paths,
        }
    }

    fn prune(&self) -> Result<()> {
        let live = self
            .registry
            .list()?
            .into_iter()
            .map(|registration| registration.instance.to_string())
            .collect::<BTreeSet<_>>();
        let report = self.evidence.prune(DEFAULT_TERMINAL_SESSION_LIMIT, &live)?;
        for path in report.incomplete {
            tracing::warn!(path = %path.display(), "incomplete world evidence was retained");
        }
        Ok(())
    }

    async fn recover_terminal(&self, instance: &str) -> Result<Option<WorldTerminalSummary>> {
        let paths = self.paths.clone();
        let instance = instance.to_owned();
        tokio::task::spawn_blocking(move || {
            let inspector = phoxal_cli_host::world::SystemProcessInspector;
            let registry = WorldRegistry::new(paths.clone(), inspector);
            let evidence = WorldEvidence::new(paths);
            registry.recover_host_loss(&evidence, &instance, &inspector)
        })
        .await
        .context("world host-loss recovery worker failed")?
    }
}
