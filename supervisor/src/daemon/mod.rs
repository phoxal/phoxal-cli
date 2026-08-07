//! `phoxald`: one bundle, one execution, one router.
//!
//! The daemon validates and executes. It never builds: no Cargo, no registry,
//! no build script, no source discovery, no project mutation. An installed
//! robot must start with no toolchain and no network, so acquiring any of those
//! responsibilities here would make that impossible.
//!
//! # The startup sequence
//!
//! The order is the contract, and each step is what makes the next one
//! meaningful:
//!
//! ```text
//!  1. canonicalize and require a finalized bundle root
//!  2. take the supervisor lock, for the daemon's whole life
//!  3. load and validate the finalized bundle
//!  4. read the brain's compatibility metadata first
//!  5. derive the runtime requirements from the manifest and the catalog
//!  6. validate every other selected binary
//!  7. mint the ExecutionId
//!  8. open the embedded router with ZID = ExecutionId
//!  9. verify the router reported that ZID
//! 10. mint the ExecutionOrigin and construct the in-memory launch plan
//! 11. register the supervisor API and the identity liveliness token
//! 12. launch and supervise the participant graph
//! ```
//!
//! Steps 2 through 6 happen before any identity exists, so a bundle that fails
//! validation never gets a router, never gets an execution key space, and is
//! never something a client can attach to. Step 9 is not paranoia: the identity
//! equality is the whole attachment mechanism, because a client reads the
//! router's ZID and *has* the execution without asking anyone.
//!
//! # No modes
//!
//! There is no simulation branch anywhere below. `clock: real | simulated` is
//! manifest data: it selects the participant set through the one requirement
//! derivation, is handed to each participant as its clock source, and is
//! reported to clients as `ExecutionMode`. The daemon never asks "am I
//! simulating?" - and it never launches Webots, which is the client's.

pub(crate) mod bundle;
pub(crate) mod lock;
pub(crate) mod plan;
pub(crate) mod projection;
pub(crate) mod roster;
pub(crate) mod serve;
pub(crate) mod state;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use phoxal_bus::{Bus, BusConfig};
use phoxal_cli_core::identity::ExecutionId;
use phoxal_cli_core::project::launch_plan::RunIdentity;
use phoxal_cli_core::project::layout::RuntimeLayout;
use phoxal_cli_core::project::requirements::RequiredParticipantKind;
use phoxal_cli_core::runtime::RobotKey;
use phoxal_cli_core::runtime::paths::RuntimePaths;
use phoxal_manifest::bundle::BundleResolver;
use phoxal_manifest::source::robot::v0::Clock;
use phoxal_supervisor_api::{
    DaemonFailureReason, ExecutionMode, Name, ProcessState, RobotIdentity, StartupStepKind,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{SupervisorOptions, SupervisorState, WaitBudget, stages_for_run};
use projection::ExecutionFacts;
use roster::Roster;
use serve::command::Control;
use state::ExecutionState;

/// The bounded wait for the participant graph to prove itself ready. A headless
/// daemon has no operator to decide it has waited long enough, so the deadline
/// is deterministic rather than unbounded.
const READINESS_BUDGET: Duration = Duration::from_secs(120);

/// The largest file `supervisor/bundle/get` will return in one reply.
const MAX_BUNDLE_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;

/// Depth of the remote command queue. Commands are operator-paced; a deeper
/// queue would only let a stalled supervision loop hide behind acceptances.
const COMMAND_QUEUE_DEPTH: usize = 16;

/// Run one execution from `requested_root` until it stops or fails.
pub async fn run(requested_root: &Path) -> Result<()> {
    let board = SupervisorState::new();
    // Before the manifest is read there is no identity to report, so the state
    // starts on placeholders the loader replaces at step 3.
    let state = ExecutionState::new(
        board.clone(),
        ExecutionFacts {
            robot: RobotIdentity {
                id: Name::new(""),
                namespace: Name::new(""),
            },
            mode: ExecutionMode::Real,
            roster: Roster::default(),
        },
    );
    let bridge = state.bridge_store_changes();
    let outcome = execute(requested_root, &state).await;
    if let Err(error) = &outcome
        && state.failure().is_none()
    {
        state.fail(DaemonFailureReason::Internal, format!("{error:#}"));
    }
    bridge.abort();
    outcome
}

async fn execute(requested_root: &Path, state: &ExecutionState) -> Result<()> {
    // 1-2. The lock comes before the bundle is even read: a build must not be
    // able to replace files under a daemon that is mid-validation.
    state.step_active(StartupStepKind::Bundle);
    let root = bundle::require_finalized_bundle_root(requested_root).inspect_err(|error| {
        fail_step(
            state,
            StartupStepKind::Bundle,
            DaemonFailureReason::InvalidBundle,
            error,
        )
    })?;
    let paths = RuntimePaths::for_root(&bundle::owning_root(&root));
    let lock = lock::SupervisorLock::acquire(&paths.supervisor_lock())?;
    tracing::info!(bundle = %root.display(), lock = %lock.path().display(), "phoxald starting");
    state.step_detail(StartupStepKind::Bundle, root.display().to_string());

    // 3. The framework loader owns document validation, `extends` rejection,
    // deterministic component roots, and asset fencing.
    let layout = RuntimeLayout::open(&root).inspect_err(|error| {
        fail_step(
            state,
            StartupStepKind::Bundle,
            DaemonFailureReason::InvalidBundle,
            error,
        )
    })?;
    let robot = RobotIdentity {
        id: Name::new(layout.robot().robot_id()),
        namespace: Name::new(layout.robot().namespace()),
    };
    let mode = match layout.manifest().clock {
        Clock::Real => ExecutionMode::Real,
        Clock::Simulated => ExecutionMode::Simulated,
    };
    let robot_key = RobotKey::new(layout.robot().namespace(), layout.robot().robot_id());
    state.step_done(StartupStepKind::Bundle);

    // 4-6. Requirement derivation already happened inside `RuntimeLayout::open`
    // - it is a property of the bundle, not a separate decision - so this step
    // reports what it selected. The brain is inspected first: a project whose
    // own composition root disagrees with this daemon is the diagnostic an
    // operator needs, ahead of any official service's.
    state.step_active(StartupStepKind::Requirements);
    let requirements = layout.requirements().clone();
    let roster = Roster::from_requirements(&robot_key, &requirements);
    state.identify(ExecutionFacts {
        robot: robot.clone(),
        mode,
        roster: roster.clone(),
    });
    state.step_detail(
        StartupStepKind::Requirements,
        format!("{} participants", requirements.participants.len()),
    );
    state.step_done(StartupStepKind::Requirements);

    state.step_active(StartupStepKind::Binaries);
    let inspection = phoxal_cli_core::project::layout::LayoutInspection::Host;
    if let Some(brain) = requirements
        .participants
        .iter()
        .find(|required| required.kind == RequiredParticipantKind::Brain)
    {
        layout.inspect_for(brain, inspection).inspect_err(|error| {
            fail_step(
                state,
                StartupStepKind::Binaries,
                DaemonFailureReason::IncompatibleParticipant,
                error,
            );
        })?;
    }
    let selected = layout.inspect_selected(inspection).inspect_err(|error| {
        fail_step(
            state,
            StartupStepKind::Binaries,
            DaemonFailureReason::IncompatibleParticipant,
            error,
        );
    })?;
    state.step_detail(
        StartupStepKind::Binaries,
        format!("{} binaries validated", selected.len()),
    );
    state.step_done(StartupStepKind::Binaries);

    // 7-9. One execution equals one router lifetime, and they are the same
    // identity: a client that reads the router's ZID has learned the execution
    // without asking anyone.
    state.step_active(StartupStepKind::Router);
    let execution = ExecutionId::mint();
    let endpoint = router_endpoint(&paths.supervisor_socket());
    let shutdown = CancellationToken::new();
    let router_lost = {
        let state = state.clone();
        let shutdown = shutdown.clone();
        Arc::new(move |reason: String| {
            // The router IS this execution, so losing it ends the execution
            // rather than degrading it.
            state.fail(DaemonFailureReason::RouterLost, &reason);
            shutdown.cancel();
        }) as crate::RouterLost
    };
    let router = crate::start_embedded_router(
        execution,
        endpoint.clone(),
        layout.router_config(),
        router_lost,
    )
    .await
    .inspect_err(|error| {
        fail_step(
            state,
            StartupStepKind::Router,
            DaemonFailureReason::RouterUnavailable,
            error,
        );
    })?;
    // The authored router config cannot win: the ZID is pinned by Phoxal, and
    // this is where that is proven rather than assumed.
    let bus = Bus::open(BusConfig {
        execution,
        participant: SUPERVISOR_PARTICIPANT.to_string(),
        connect_endpoints: vec![endpoint.clone()],
    })
    .await
    .map_err(|error| anyhow::anyhow!("failed to open the supervisor's own session: {error}"))?;
    verify_router_identity(&bus, execution, &endpoint).await?;
    state.step_detail(
        StartupStepKind::Router,
        format!("{execution} on {endpoint}"),
    );
    state.step_done(StartupStepKind::Router);

    // 10. The plan is in memory and dies with this process. Producers are not
    // planned: each is the ZID of the session its participant opens.
    let run = RunIdentity::mint_or_adopt(Some(execution));
    let constructed = layout.construct_plan_from_selected(&selected, run)?;
    let specs = plan::participant_specs(&constructed.plan, &layout.bin_dir(), &endpoint)?;

    // 11. The bundle index is taken under the held lock, which is what freezes
    // the tree it describes for this execution's lifetime.
    let resolver = BundleResolver::index(&root, MAX_BUNDLE_RESPONSE_BYTES)
        .with_context(|| format!("failed to index the finalized bundle {}", root.display()))?;
    let (actions, action_rx) = mpsc::channel(COMMAND_QUEUE_DEPTH);
    let served = tokio::spawn(serve::serve(
        bus.clone(),
        state.clone(),
        Control {
            actions,
            stop: shutdown.clone(),
        },
        resolver,
        shutdown.clone(),
    ));
    // Serving is the whole control plane: an execution nobody can attach to,
    // observe, or stop is not one worth continuing. If it ends before the
    // shutdown that is supposed to end it, the execution ends with it rather
    // than running on invisibly.
    let watchdog = {
        let state = state.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let reason = match served.await {
                Ok(Ok(())) => return,
                Ok(Err(error)) => format!("the supervisor API stopped serving: {error:#}"),
                Err(error) => format!("the supervisor API task ended unexpectedly: {error}"),
            };
            if !shutdown.is_cancelled() {
                state.fail(DaemonFailureReason::Internal, reason);
                shutdown.cancel();
            }
        })
    };
    // The supervisor's own presence on the bus: it learns each incarnation's
    // producer from the liveliness token that incarnation publishes.
    let liveliness = observe_participants(&bus, state.board(), &robot_key).await?;

    // Under systemd this unit is `Type=notify`, and this daemon is what the
    // unit starts: readiness and the watchdog are the
    // daemon's to signal, because it is the only process that knows when the
    // graph became ready. A graph that never reaches readiness never signals,
    // which is exactly what makes systemd time the start out and mark the unit
    // failed.
    let readiness_notify = crate::systemd::notify::SdNotify::from_env()
        .unwrap_or_else(|error| {
            tracing::warn!("ignoring an unusable systemd notify socket: {error:#}");
            None
        })
        .map(|notify| tokio::spawn(notify_readiness(notify, state.clone())));

    // 12. Launch, then wait. The router is this process's own state, so there
    // is no router child to supervise and no recovery epoch to rebuild the
    // graph around.
    state.step_active(StartupStepKind::Participants);
    for spec in &specs {
        state
            .board()
            .register_planned(&spec.key, spec.kind, spec.startup_requirement);
    }
    let outcome = crate::supervise_until_shutdown(
        stages_for_run(specs, WaitBudget::Bounded(READINESS_BUDGET)),
        state.board().clone(),
        std::sync::Arc::new(ParticipantStageProgress(state.clone())),
        SupervisorOptions {
            action_rx: Some(crate::SupervisorActionReceiver::new(action_rx)),
            token: shutdown.clone(),
            publishes_running_on_startup_complete: true,
        },
    )
    .await;
    match &outcome {
        Ok(()) => state.step_done(StartupStepKind::Participants),
        Err(error) => {
            let reason = classify_launch_failure(state, &roster, mode);
            // A missing world clock has a fix an operator can act on, so the
            // reason carries that explanation rather than only the readiness
            // timeout that surfaced it.
            let detail = match (reason, roster.world_clock()) {
                (DaemonFailureReason::WorldClockMissing, Some(world_clock)) => format!(
                    "{}: {error:#}",
                    world_clock_diagnostic(&world_clock.wire.to_string())
                ),
                _ => format!("{error:#}"),
            };
            state.step_failed(StartupStepKind::Participants, &detail);
            state.fail(reason, detail);
        }
    }

    drop(liveliness);
    if let Some(readiness_notify) = readiness_notify {
        readiness_notify.abort();
    }
    shutdown.cancel();
    watchdog.abort();
    if let Err(error) = bus.close().await {
        tracing::debug!("failed to close the supervisor session: {error}");
    }
    if let Err(error) = router.close().await {
        tracing::warn!("{error:#}");
    }
    outcome
}

/// This daemon's own label in bus metadata. Never identity: identity is the
/// session's ZID.
const SUPERVISOR_PARTICIPANT: &str = "phoxald";

/// Why the graph failed to come up.
///
/// A `clock: simulated` bundle whose world clock never appeared gets its own
/// reason, because it has its own cause: the client-owned simulator was never
/// started. Everything else is a launch failure.
fn classify_launch_failure(
    state: &ExecutionState,
    roster: &Roster,
    mode: ExecutionMode,
) -> DaemonFailureReason {
    if mode != ExecutionMode::Simulated {
        return DaemonFailureReason::LaunchFailed;
    }
    let Some(world_clock) = roster.world_clock() else {
        return DaemonFailureReason::LaunchFailed;
    };
    let ready = state
        .snapshot()
        .processes
        .iter()
        .any(|process| process.key == world_clock.wire && process.state == ProcessState::Ready);
    if ready {
        DaemonFailureReason::LaunchFailed
    } else {
        DaemonFailureReason::WorldClockMissing
    }
}

/// The diagnostic a headless simulated bundle earns: it names the manifest
/// field that put it in this state and the participant that never arrived.
pub(crate) fn world_clock_diagnostic(world_clock: &str) -> String {
    format!(
        "this bundle's robot.yaml declares `clock: simulated`, so simulated time is owned by the \
         world clock `{world_clock}` - and it never became ready. phoxald does not start Webots: \
         the simulator is client-owned, so run this bundle through `phoxal simulation run` rather \
         than starting the daemon on it directly"
    )
}

fn fail_step(
    state: &ExecutionState,
    step: StartupStepKind,
    reason: DaemonFailureReason,
    error: &anyhow::Error,
) {
    let detail = format!("{error:#}");
    state.step_failed(step, &detail);
    state.fail(reason, detail);
}

/// The Zenoh listen endpoint the daemon binds and every participant dials.
///
/// It reuses the old resident IPC socket's filename deliberately: that socket
/// and its private frame protocol are gone, and pre-v1 no old client exists to
/// be confused by the name.
fn router_endpoint(socket: &Path) -> String {
    format!("unixsock-stream/{}", socket.display())
}

/// Prove the router adopted the identity Phoxal pinned.
///
/// An authored router config cannot override it, and this is where that is a
/// fact rather than an intention: a client attaches by reading the router's
/// ZID, so a router running under any other identity is a router no client
/// could reach this execution through.
async fn verify_router_identity(bus: &Bus, expected: ExecutionId, endpoint: &str) -> Result<()> {
    let executions = Bus::probe_routers(endpoint).await.map_err(|error| {
        anyhow::anyhow!("failed to read the embedded router's identity: {error}")
    })?;
    match executions.as_slice() {
        [reported] if *reported == expected => Ok(()),
        [reported] => bail!(
            "the embedded router on {endpoint} reports {reported} but this execution is \
             {expected}; the execution id and the router ZID must be the same value"
        ),
        [] => bail!("the embedded router on {endpoint} reports no identity"),
        many => bail!(
            "{endpoint} is served by {} routers; a robot-owned endpoint must be exactly one",
            many.len()
        ),
    }
    .and_then(|()| {
        anyhow::ensure!(
            bus.execution() == expected,
            "the supervisor's own session is rooted at {} rather than {expected}",
            bus.execution()
        );
        Ok(())
    })
}

/// Observe every participant's liveliness token: this is where the supervisor
/// learns which producer each incarnation actually publishes under, and proves
/// it ready.
async fn observe_participants(
    bus: &Bus,
    board: &SupervisorState,
    robot: &RobotKey,
) -> Result<phoxal_bus::ParticipantLivelinessObserver> {
    let namespace = robot.namespace.clone();
    let robot_id = robot.robot_id.clone();
    let board = board.clone();
    bus.observe_participant_liveliness(move |event| {
        crate::state::readiness::apply_liveliness_event(&board, &namespace, &robot_id, event);
    })
    .await
    .map_err(|error| anyhow::anyhow!("failed to observe participant liveliness: {error}"))
}

/// The daemon's `Participants` startup step, advanced by the supervision loop.
///
/// The loop's stages are all inside that one step - it is the last step in the
/// daemon's own sequence, and everything before it happened before any process
/// was spawned. Detail names the stage that just completed, so an operator
/// watching startup sees which barrier is holding.
struct ParticipantStageProgress(ExecutionState);

impl crate::process::stages::StageProgress for ParticipantStageProgress {
    fn started(&self, label: &str) {
        self.0.step_detail(StartupStepKind::Participants, label);
    }

    fn detail(&self, detail: String) {
        self.0.step_detail(StartupStepKind::Participants, detail);
    }

    fn finished(&self) {
        self.0.step_done(StartupStepKind::Participants);
    }

    fn failed(&self, reason: &str) {
        self.0.step_failed(StartupStepKind::Participants, reason);
    }
}

/// Signal `READY=1` once the execution is ready, then ping `WATCHDOG=1` at the
/// notify socket's cadence until the task is aborted.
///
/// `Degraded` counts as ready: an optional participant that failed does not
/// make the robot unavailable, and refusing to signal would have systemd
/// restart a graph that is running.
async fn notify_readiness(notify: crate::systemd::notify::SdNotify, state: ExecutionState) {
    use phoxal_supervisor_api::Lifecycle;
    let mut snapshots = state.subscribe();
    loop {
        match snapshots.borrow_and_update().lifecycle {
            Lifecycle::Ready | Lifecycle::Degraded => break,
            Lifecycle::Failed | Lifecycle::Stopping | Lifecycle::Stopped => return,
            Lifecycle::Starting => {}
        }
        if snapshots.changed().await.is_err() {
            return;
        }
    }
    if let Err(error) = notify.notify_ready() {
        tracing::warn!("failed to signal systemd readiness: {error:#}");
        return;
    }
    let Some(interval) = notify.watchdog_interval() else {
        return;
    };
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        if let Err(error) = notify.notify_watchdog() {
            tracing::warn!("failed to ping the systemd watchdog: {error:#}");
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::roster::tests::roster;

    fn state_with(mode: ExecutionMode) -> ExecutionState {
        ExecutionState::new(
            SupervisorState::new(),
            ExecutionFacts {
                robot: RobotIdentity {
                    id: Name::new("rover"),
                    namespace: Name::new("demo"),
                },
                mode,
                roster: roster(),
            },
        )
    }

    #[test]
    fn a_headless_simulated_bundle_fails_on_the_missing_world_clock() {
        let state = state_with(ExecutionMode::Simulated);
        let roster = roster();
        assert_eq!(
            classify_launch_failure(&state, &roster, ExecutionMode::Simulated),
            DaemonFailureReason::WorldClockMissing,
            "a simulated bundle whose world clock never arrived has its own cause"
        );

        let diagnostic = world_clock_diagnostic("webots");
        assert!(diagnostic.contains("clock: simulated"), "{diagnostic}");
        assert!(diagnostic.contains("webots"), "{diagnostic}");
        assert!(
            diagnostic.contains("phoxal simulation run"),
            "the diagnostic must name the fix: {diagnostic}"
        );
    }

    #[test]
    fn a_real_clock_failure_is_never_blamed_on_a_world_clock() {
        let state = state_with(ExecutionMode::Real);
        assert_eq!(
            classify_launch_failure(&state, &roster(), ExecutionMode::Real),
            DaemonFailureReason::LaunchFailed
        );
    }

    #[test]
    fn a_ready_world_clock_means_something_else_failed() {
        let state = state_with(ExecutionMode::Simulated);
        let roster = roster();
        let world_clock = roster.world_clock().expect("a simulated roster has one");
        state.board().upsert_process(
            world_clock.core.clone(),
            phoxal_cli_core::runtime::ParticipantKind::Simulator,
            phoxal_cli_core::runtime::ProcessState::Ready,
            phoxal_cli_core::runtime::StartupRequirement::Required,
        );
        state.republish();
        assert_eq!(
            classify_launch_failure(&state, &roster, ExecutionMode::Simulated),
            DaemonFailureReason::LaunchFailed
        );
    }

    #[test]
    fn the_listen_endpoint_is_the_run_directorys_supervisor_socket() {
        assert_eq!(
            router_endpoint(Path::new("/work/rover/.phoxal/run/supervisor.sock")),
            "unixsock-stream//work/rover/.phoxal/run/supervisor.sock"
        );
    }
}
