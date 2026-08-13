//! Run one authoritative compiled bundle until its execution ends.

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
use phoxal_api::supervisor::connect::PRESENCE_KEY;
use phoxal_api::supervisor::snapshot::{DaemonFailureReason, Lifecycle, StartupStepKind};
use phoxal_bus::{
    BusConfig, BusHandle, BusOwner, ParticipantReadyEvent, ParticipantReadyObserver,
    ParticipantReadyStatus, SourceLabel,
};
use phoxal_model::Clock;
use phoxal_runtime_contract::identity::ExecutionId;
use phoxal_runtime_contract::origin::ExecutionOrigin;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::process::spec::SupervisorOptions;
use crate::process::stages::{WaitBudget, stages_for_run};
use crate::state::store::SupervisorState;
use projection::ExecutionFacts;
use roster::Roster;
use serve::Control;
use state::ExecutionState;

const READINESS_BUDGET: Duration = Duration::from_secs(120);
const COMMAND_QUEUE_DEPTH: usize = 16;
const SUPERVISOR_LABEL: &str = "phoxald";

pub(crate) async fn run(requested_root: &Path) -> Result<()> {
    let canonical = requested_root.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize bundle root {}",
            requested_root.display()
        )
    })?;
    let paths = phoxal_cli_host::paths::RuntimePaths::for_root(&bundle::owning_root(&canonical));
    let lock = lock::SupervisorLock::acquire(&paths.supervisor_lock())?;
    let runtime = bundle::open(&canonical)?;
    tracing::info!(
        bundle = %runtime.root().display(),
        lock = %lock.path().display(),
        "phoxald starting"
    );

    let board = SupervisorState::new();
    let roster = Roster::from_bundle(&runtime);
    let state = ExecutionState::new(board, ExecutionFacts { roster });
    state.step_active(StartupStepKind::Bundle);
    state.step_detail(
        StartupStepKind::Bundle,
        runtime.root().display().to_string(),
    );
    state.step_done(StartupStepKind::Bundle);
    let shutdown = CancellationToken::new();
    let outcome = execute(runtime, &paths, &state, shutdown.clone()).await;
    shutdown.cancel();
    if let Err(error) = &outcome
        && state.failure().is_none()
    {
        state.fail(DaemonFailureReason::Internal, format!("{error:#}"));
    }
    outcome
}

async fn execute(
    runtime: phoxal_bundle::RuntimeBundle,
    paths: &phoxal_cli_host::paths::RuntimePaths,
    state: &ExecutionState,
    shutdown: CancellationToken,
) -> Result<()> {
    state.step_active(StartupStepKind::Router);
    let execution = ExecutionId::mint();
    let endpoint = router_endpoint(&paths.checked_supervisor_socket()?);
    let router_lost = {
        let state = state.clone();
        let shutdown = shutdown.clone();
        Arc::new(move |reason: String| {
            state.fail(DaemonFailureReason::RouterLost, reason);
            shutdown.cancel();
        }) as crate::router::RouterLost
    };
    let router_config = runtime
        .document()
        .runtime()
        .router()
        .map(|config| runtime.root().join(config.path().as_str()));
    let router = crate::router::start_embedded_router(
        execution,
        endpoint.clone(),
        router_config.as_deref(),
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

    let label = match SourceLabel::new(SUPERVISOR_LABEL) {
        Ok(label) => label,
        Err(error) => {
            return Err(abort_router_startup(state, router, None, error.into()).await);
        }
    };
    let (owner, bus) = match BusOwner::open(BusConfig::for_external(
        execution,
        Some(label),
        vec![endpoint.clone()],
    ))
    .await
    {
        Ok(opened) => opened,
        Err(error) => {
            let error = anyhow::anyhow!("failed to open supervisor bus: {error}");
            return Err(abort_router_startup(state, router, None, error).await);
        }
    };
    if let Err(error) = verify_router_identity(&bus, execution, &endpoint).await {
        return Err(abort_router_startup(state, router, Some(owner), error).await);
    }
    let identity = match owner.declare_liveliness_key(PRESENCE_KEY).await {
        Ok(identity) => identity,
        Err(error) => {
            return Err(abort_router_startup(state, router, Some(owner), error.into()).await);
        }
    };
    state.step_detail(
        StartupStepKind::Router,
        format!("{execution} on {endpoint}"),
    );
    state.step_done(StartupStepKind::Router);

    let origin = match runtime.robot().clock() {
        Clock::Real => Some(
            ExecutionOrigin::try_mint()
                .context("the host boot clock is unavailable; cannot mint execution time")?,
        ),
        Clock::Simulated => None,
    };
    let specs = plan::participant_specs(&runtime, execution, origin, &endpoint)?;
    for spec in &specs {
        state.board().register_planned(&spec.key);
    }
    let readiness = observe_participants(&bus, state, &shutdown).await?;

    let (actions, action_rx) = mpsc::channel(COMMAND_QUEUE_DEPTH);
    let serving = serve::serve(
        bus.clone(),
        state.clone(),
        Control {
            actions,
            stop: shutdown.clone(),
        },
        runtime,
        shutdown.clone(),
    );
    let supervision = crate::process::supervise::supervise_until_shutdown(
        stages_for_run(specs, WaitBudget::Bounded(READINESS_BUDGET)),
        state.board().clone(),
        Arc::new(ParticipantStageProgress(state.clone())),
        SupervisorOptions {
            action_rx: Some(action_rx),
            token: shutdown.clone(),
            publishes_running_on_startup_complete: true,
        },
    );
    let readiness_notify = crate::systemd::notify::SdNotify::from_env()
        .unwrap_or_else(|error| {
            tracing::warn!("ignoring an unusable systemd notify socket: {error:#}");
            None
        })
        .map(|notify| tokio::spawn(notify_readiness(notify, state.clone(), shutdown.clone())));

    state.step_active(StartupStepKind::Participants);
    tokio::pin!(serving);
    tokio::pin!(supervision);
    let outcome = tokio::select! {
        result = &mut supervision => {
            shutdown.cancel();
            let served = serving.await;
            result.and(served)
        }
        result = &mut serving => {
            let was_cancelled = shutdown.is_cancelled();
            let result = match result {
                Ok(()) if !was_cancelled => {
                    Err(anyhow::anyhow!("the supervisor control plane ended unexpectedly"))
                }
                other => other,
            };
            if !was_cancelled {
                let detail = match &result {
                    Ok(()) => "the supervisor control plane stopped during shutdown".to_string(),
                    Err(error) => format!("the supervisor control plane failed: {error:#}"),
                };
                state.fail(DaemonFailureReason::ControlPlaneLost, detail);
                shutdown.cancel();
            }
            let supervised = supervision.await;
            result.and(supervised)
        }
    };

    match &outcome {
        Ok(()) => state.step_done(StartupStepKind::Participants),
        Err(error) => {
            state.step_failed(StartupStepKind::Participants, format!("{error:#}"));
            if state.failure().is_none() {
                state.fail(DaemonFailureReason::LaunchFailed, format!("{error:#}"));
            }
        }
    }

    shutdown.cancel();
    let notify_outcome = if let Some(task) = readiness_notify {
        task.await
            .context("the systemd readiness task panicked")
            .and_then(std::convert::identity)
    } else {
        Ok(())
    };
    drop(readiness);
    drop(identity);
    let close = owner.close().await;
    let router_close = router.close().await;
    let bus_outcome = if close.is_clean() {
        Ok(())
    } else {
        state.fail(DaemonFailureReason::TeardownFailed, close.to_string());
        Err(anyhow::anyhow!("supervisor bus teardown failed: {close}"))
    };
    let router_outcome = router_close.inspect_err(|error| {
        state.fail(DaemonFailureReason::TeardownFailed, format!("{error:#}"));
    });
    outcome
        .and(notify_outcome)
        .and(bus_outcome)
        .and(router_outcome)
}

async fn abort_router_startup(
    state: &ExecutionState,
    router: crate::router::EmbeddedRouter,
    owner: Option<BusOwner>,
    error: anyhow::Error,
) -> anyhow::Error {
    fail_step(
        state,
        StartupStepKind::Router,
        DaemonFailureReason::RouterUnavailable,
        &error,
    );
    if let Some(owner) = owner {
        let close = owner.close().await;
        if !close.is_clean() {
            tracing::warn!(%close, "supervisor bus did not close cleanly after router startup failed");
        }
    }
    if let Err(close_error) = router.close().await {
        tracing::warn!(error = %close_error, "embedded router did not close cleanly after startup failed");
    }
    error
}

async fn observe_participants(
    bus: &BusHandle,
    state: &ExecutionState,
    shutdown: &CancellationToken,
) -> Result<ParticipantReadyObserver> {
    let state = state.clone();
    let shutdown = shutdown.clone();
    Ok(bus
        .observe_participant_ready(move |event| apply_ready(&state, &shutdown, event))
        .await?)
}

fn apply_ready(state: &ExecutionState, shutdown: &CancellationToken, event: ParticipantReadyEvent) {
    if let Some(detail) = state.board().record_instance_presence(
        event.participant().clone(),
        event.producer(),
        event.status == ParticipantReadyStatus::Ready,
    ) {
        state.fail(DaemonFailureReason::LaunchFailed, detail);
        shutdown.cancel();
    }
}

fn router_endpoint(socket: &Path) -> String {
    format!("unixsock-stream/{}", socket.display())
}

async fn verify_router_identity(
    bus: &BusHandle,
    expected: ExecutionId,
    endpoint: &str,
) -> Result<()> {
    let executions = BusOwner::probe_routers(endpoint).await?;
    match executions.as_slice() {
        [reported] if *reported == expected => {}
        [reported] => bail!("router reports {reported}, expected {expected}"),
        [] => bail!("router on {endpoint} reports no execution identity"),
        many => bail!("router endpoint {endpoint} reports {} routers", many.len()),
    }
    anyhow::ensure!(
        bus.execution() == expected,
        "supervisor bus execution mismatch"
    );
    Ok(())
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

async fn notify_readiness(
    notify: crate::systemd::notify::SdNotify,
    state: ExecutionState,
    shutdown: CancellationToken,
) -> Result<()> {
    let mut snapshots = state.subscribe();
    loop {
        match snapshots.borrow_and_update().lifecycle {
            Lifecycle::Ready | Lifecycle::Degraded => break,
            Lifecycle::Failed | Lifecycle::Stopping | Lifecycle::Stopped => return Ok(()),
            Lifecycle::Starting => {}
        }
        tokio::select! {
            () = shutdown.cancelled() => return Ok(()),
            changed = snapshots.changed() => changed.context("snapshot authority closed")?,
        }
    }
    notify.notify_ready()?;
    let Some(interval) = notify.watchdog_interval() else {
        return Ok(());
    };
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return Ok(()),
            _ = ticker.tick() => notify.notify_watchdog()?,
        }
    }
}
