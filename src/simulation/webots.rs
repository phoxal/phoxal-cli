//! Webots process preparation and spawn-query serving.

use super::{
    SimPlan, WEBOTS_APP_ID, stage_simulation_for_robot, stage_simulator_controller_binaries,
    webots_world,
};
use crate::supervisor::ParticipantSpec;
use crate::supervisor::wait_for_endpoint;
use crate::webots_stage_root;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use phoxal::bus::{Codec, ContractBody, MessagePack, QueryFailure};
use phoxal::raw::{Bus, BusConfig};
use phoxal_api::v0_2::simulation::RobotSpawn;
use phoxal_api::v0_2::simulation::{SpawnRequest, SpawnSet};
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::session::ParticipantKind;
use phoxal_cli_core::session::{
    ProcessKey, ReadinessPolicy, RuntimeFailurePolicy, StartupRequirement,
};
use std::path::Path;
use tokio::task::JoinHandle;

/// Stage the simulation world and build the Webots application process spec.
pub(crate) fn stage_and_prepare_webots_spec(
    ui: &crate::Ui,
    sim: &SimPlan,
) -> Result<(ParticipantSpec, Vec<RobotSpawn>)> {
    let world = webots_world(&sim.plan.mode);
    let staged =
        stage_simulation_for_robot(&sim.ctx.project_root, world, &sim.ctx.resolved, &sim.plan)?;
    stage_simulator_controller_binaries(&sim.ctx.resolved, ui)?;
    let webots_path = crate::host_doctor::webots_executable_path()
        .map_err(|error| anyhow!("{error}"))
        .context("failed to locate the Webots executable for live simulate")?;
    // Print the generated project-local staging root explicitly.
    ui.info(format!(
        "staged simulation to {}",
        webots_stage_root::root()?.display()
    ));
    ui.info(format!(
        "staged simulation world at {}",
        staged.staged_world_path.display()
    ));
    let spec = ParticipantSpec {
        key: ProcessKey::project(WEBOTS_APP_ID),
        id: WEBOTS_APP_ID.to_string(),
        kind: ParticipantKind::Tool,
        executable: webots_path,
        args: webots_launch_args(&staged.staged_world_path),
        cwd: None,
        env: Vec::new(),
        shutdown_grace: std::time::Duration::from_secs(20),
        process_group: true,
        note: None,
        // The Webots application itself has no bus identity of its own - it
        // never declares participant Liveliness (the supervisor and each
        // controller Webots launches do, and those are tracked separately as
        // SIMULATION-MANAGED participants). Its readiness is necessarily
        // process-lifecycle-only, so it keeps the old spawn-is-ready behavior.
        bus_participant: false,
        readiness: ReadinessPolicy::ProcessSpawned,
        startup_requirement: StartupRequirement::Required,
        runtime_failure: RuntimeFailurePolicy::StopProject,
        restart_policy: Default::default(),
    };
    Ok((spec, staged.spawn_descriptors))
}

/// Declare and keep alive the query responder that owns the simulation spawn
/// set. With an external router, declaration completes before the caller gives
/// Webots to the process supervisor. With a CLI-managed router, this task
/// retries until that router starts, while the Webots supervisor retries its
/// bounded query. The task and its bus session live until simulation ends.
pub(crate) async fn start_spawn_responder(
    launch_plan: &LaunchPlan,
    robots: Vec<RobotSpawn>,
    connect: &str,
) -> Result<JoinHandle<()>> {
    let robot = launch_plan
        .robots
        .first()
        .context("sim launch plan has no robot for the spawn responder bus root")?;
    let bus_config = BusConfig {
        namespace: robot.namespace.clone(),
        robot_id: robot.id.clone(),
        participant: "phoxal-cli-simulation-spawn".to_string(),
        incarnation: 0,
        connect_endpoints: vec![connect.to_string()],
    };
    let response = MessagePack::encode(&SpawnSet {
        revision: 1,
        robots,
    })
    .context("failed to encode simulation spawn set")?;

    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
        let mut ready_tx = Some(ready_tx);
        loop {
            wait_for_endpoint(&bus_config.connect_endpoints[0]).await;
            let bus = match Bus::open(bus_config.clone()).await {
                Ok(bus) => bus,
                Err(error) => {
                    tracing::debug!(%error, "simulation spawn responder waiting for router");
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    continue;
                }
            };
            let queryable = match bus
                .declare_server(<SpawnRequest as ContractBody>::TOPIC)
                .await
            {
                Ok(queryable) => queryable,
                Err(error) => {
                    tracing::debug!(%error, "simulation spawn responder declaration failed");
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    continue;
                }
            };
            if let Some(ready_tx) = ready_tx.take() {
                let _ = ready_tx.send(());
            }

            if let Err(error) = serve_spawn_queries(&bus, &queryable, &response).await {
                tracing::warn!(%error, "simulation spawn responder disconnected; retrying");
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    });

    // With an external router, declaration must finish before Webots is added
    // to supervision. With a CLI-managed router, the router is itself launched
    // by that supervision call, so the responder retries in parallel and the
    // supervisor's bounded query retry bridges the bootstrap dependency.
    ready_rx
        .await
        .context("simulation spawn responder exited before declaring its queryable")?;

    Ok(handle)
}

pub(crate) async fn serve_spawn_queries(
    bus: &Bus,
    queryable: &phoxal::raw::ServerQueryable,
    response: &[u8],
) -> Result<()> {
    loop {
        let incoming = queryable.recv().await?;
        let request = incoming
            .request_metadata()
            .and_then(|_| incoming.request_bytes())
            .and_then(|bytes| {
                MessagePack::decode::<SpawnRequest>(&bytes)
                    .map_err(|error| phoxal::bus::BusError::Transport(error.to_string()))
            });
        match request {
            Ok(request) => {
                tracing::debug!(
                    known_revision = request.known_revision,
                    "serving simulation spawn set"
                );
                incoming.reply(bus, response.to_vec()).await?;
            }
            Err(error) => {
                let failure = QueryFailure::invalid_argument(format!(
                    "invalid simulation spawn request: {error}"
                ));
                incoming.reply_err(&failure).await?;
            }
        }
    }
}

/// Build Webots' argv for a live simulate launch.
///
/// `--mode=realtime` is load-bearing, not cosmetic: Webots opens a world in the
/// PAUSED state by default, so without an explicit run mode the supervisor's
/// `#[step]` is never called, `simulation/clock` never advances, and services
/// that use simulation time remain idle (session Liveliness can remain
/// present). `realtime` starts the simulation running,
/// synced to wall time so the operator watches the robot move at a natural
/// speed; the clock authority (the Webots supervisor) still owns logical time.
///
/// `--batch` suppresses Webots' blocking modal dialogs (notably the "save world
/// changes?" prompt on quit), so the CLI's requested SIGTERM stop can complete
/// without an operator having to dismiss a popup.
pub(crate) fn webots_launch_args(staged_world_path: &Path) -> Vec<String> {
    vec![
        "--mode=realtime".to_string(),
        "--batch".to_string(),
        staged_world_path.display().to_string(),
    ]
}
