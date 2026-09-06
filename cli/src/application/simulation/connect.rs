//! Robot connection and attachment transaction orchestration.

use super::start::rollback_host;
use super::*;
use crate::application::{lifecycle, startup::Startup};

/// Inputs resolved before a `simulation run` creates its transaction-owned
/// world process.
pub(super) struct ConnectionIntent {
    spawn: Option<SpawnId>,
    target: lifecycle::Target,
    detach: bool,
}

pub(super) enum ConnectedSimulationEnding {
    World(Box<WorldTerminalSummary>),
    Member(WorldMemberEvidence),
}

struct SimulationStartupTransaction {
    world: Option<StartedWorld>,
    robot: Option<lifecycle::LaunchedSession>,
}

impl SimulationStartupTransaction {
    fn new(world: Option<StartedWorld>) -> Self {
        Self { world, robot: None }
    }

    fn registration(&self) -> Option<&LocalWorldRegistration> {
        self.world.as_ref().map(|world| &world.registration)
    }

    fn retain_robot(&mut self, robot: lifecycle::LaunchedSession) {
        self.robot = Some(robot);
    }

    fn robot(&self) -> Result<&lifecycle::LaunchedSession> {
        self.robot
            .as_ref()
            .context("simulation startup transaction does not own a robot execution")
    }

    fn commit(
        mut self,
    ) -> std::result::Result<lifecycle::LaunchedSession, Box<(Self, anyhow::Error)>> {
        let Some(robot) = self.robot.take() else {
            return Err(Box::new((
                self,
                anyhow::anyhow!(
                    "cannot commit simulation startup before launching a robot execution"
                ),
            )));
        };
        if let Some(world) = self.world.take() {
            world.host.detach();
        }
        Ok(robot)
    }

    async fn rollback(mut self, mut error: anyhow::Error) -> anyhow::Error {
        if let Some(robot) = self.robot.take()
            && let Err(cleanup) = lifecycle::rollback_launched_session(robot).await
        {
            error = error.context(format!("robot rollback also failed: {cleanup:#}"));
        }
        if let Some(world) = self.world.take() {
            error = rollback_host(world.host, error).await;
        }
        error
    }
}
pub(super) fn connection_intent(
    app: &AppContext,
    spawn: Option<&str>,
    detach: bool,
) -> Result<ConnectionIntent> {
    let spawn = spawn
        .map(SpawnId::new)
        .transpose()
        .context("invalid world spawn name")?;
    let target = lifecycle::Target::resolve(None, app.project.root())?;
    Ok(ConnectionIntent {
        spawn,
        target,
        detach,
    })
}

pub(super) async fn connect_world(
    app: &AppContext,
    instance: &str,
    intent: ConnectionIntent,
    started: Option<StartedWorld>,
) -> Result<()> {
    let transaction = SimulationStartupTransaction::new(started);
    let stores = match Stores::discover() {
        Ok(stores) => stores,
        Err(error) => return Err(transaction.rollback(error).await),
    };
    let registration = match transaction.registration() {
        Some(registration) => registration.clone(),
        None => match stores.registry.resolve(instance) {
            Ok(registration) => registration,
            Err(error) => return Err(transaction.rollback(error).await),
        },
    };
    let client = match connect_verified(&registration).await {
        Ok(client) => client,
        Err(error) => return Err(transaction.rollback(error).await),
    };
    let project = app.project.root().to_path_buf();
    let offline = app.offline;
    let locked = tokio::task::spawn_blocking(move || {
        phoxal_cli_project::source::train::resolve_locked_train(&project, offline)
    })
    .await
    .context("locked framework preflight worker failed")
    .and_then(|locked| locked);
    let locked = match locked {
        Ok(locked) => locked,
        Err(error) => return Err(transaction.rollback(error).await),
    };
    if let Err(error) = ensure_compatible_train(registration.framework, locked.framework()) {
        return Err(transaction.rollback(error).await);
    }

    // The compatibility decision is deliberately before project preparation,
    // package materialization, supervisor launch, or native scene mutation.
    connect_fresh_execution(app, registration, client, intent, transaction).await
}

pub(super) fn ensure_compatible_train(
    world: FrameworkVersion,
    robot: FrameworkVersion,
) -> Result<()> {
    ensure!(
        world.is_compatible_with(robot),
        "world instance uses framework {world}, but this robot lockfile resolves {robot}; the world accepts {}, so select a robot patch on that line before any build or launch",
        world.compatibility_line()
    );
    Ok(())
}

async fn connect_fresh_execution(
    app: &AppContext,
    registration: LocalWorldRegistration,
    client: WorldSessionClient,
    intent: ConnectionIntent,
    mut transaction: SimulationStartupTransaction,
) -> Result<()> {
    let ConnectionIntent {
        spawn,
        target,
        detach,
    } = intent;
    let startup = Startup::begin(app, &target.project, Mode::Simulation);
    let prepared = match lifecycle::prepare_driver_free_execution(app, &target, &startup).await {
        Ok(prepared) => prepared,
        Err(error) => {
            let error = transaction.rollback(error).await;
            return Err(startup.failed(error));
        }
    };
    let launched = match lifecycle::launch_driver_free_execution(&target, &startup, prepared).await
    {
        Ok(launched) => launched,
        Err(error) => {
            let error = transaction.rollback(error).await;
            return Err(startup.failed(error));
        }
    };
    transaction.retain_robot(launched);
    let drivers_absent = transaction
        .robot()
        .and_then(lifecycle::LaunchedSession::ensure_drivers_absent);
    if let Err(error) = drivers_absent {
        let error = transaction.rollback(error).await;
        return Err(startup.failed(error));
    }

    let mut states = match client.state_subscription().await {
        Ok(states) => states,
        Err(error) => {
            let error = transaction.rollback(error.into()).await;
            return Err(startup.failed(error));
        }
    };
    let execution = match transaction.robot() {
        Ok(robot) => robot.session.connected().execution,
        Err(error) => {
            let error = transaction.rollback(error).await;
            return Err(startup.failed(error));
        }
    };
    startup.step(
        StepId::Attachment,
        format!("attaching {execution} to {}", registration.instance),
    );
    let attached = match client
        .attach(execution, target.endpoint.clone(), spawn)
        .await
    {
        Ok(state) => state,
        Err(error) => {
            let error = transaction.rollback(error.into()).await;
            return Err(startup.failed(error));
        }
    };
    if !member_is_active(&attached, execution) {
        let active = tokio::select! {
            result = tokio::time::timeout(
                ATTACHMENT_BUDGET,
                states.wait_for_member_active(execution),
            ) => match result {
                Ok(result) => result.map(|_| ()).map_err(anyhow::Error::from),
                Err(_) => Err(anyhow::anyhow!(
                    "timed out after {}s waiting for world member {execution} to become active",
                    ATTACHMENT_BUDGET.as_secs()
                )),
            },
            () = startup.cancelled() => Err(anyhow::anyhow!(
                "world attachment was interrupted before member {execution} became active"
            )),
        };
        if let Err(error) = active {
            let error = transaction.rollback(error).await;
            return Err(startup.failed(error));
        }
    }

    startup.complete(
        StepId::Attachment,
        format!("member {execution} active in {}", registration.instance),
    );
    let launched = match transaction.commit() {
        Ok(launched) => launched,
        Err(failure) => {
            let (transaction, error) = *failure;
            let error = transaction.rollback(error).await;
            return Err(startup.failed(error));
        }
    };
    startup.ready();
    if detach {
        launched.detach();
        report_detached_attachment(app, execution, &attached, registration.instance);
        report_world_commands(app, registration.instance);
        return Ok(());
    }
    let outcome =
        lifecycle::drive_launched_session(app, &target, launched, Mode::Simulation).await?;
    if outcome == phoxal_cli_ui::AttachmentOutcome::Detached {
        lifecycle::report_outcome(app, &target, &outcome)?;
        report_world_commands(app, registration.instance);
        return Ok(());
    }

    let stores = Stores::discover()?;
    let ending =
        await_connected_simulation_ending(&stores, &registration, &client, execution).await?;
    let world_remains_live = matches!(&ending, ConnectedSimulationEnding::Member(_));
    report_connected_simulation_ending(app, &target, &ending)?;
    if world_remains_live {
        report_world_commands(app, registration.instance);
    }
    Ok(())
}

fn report_detached_attachment(
    app: &AppContext,
    execution: phoxal::identity::ExecutionId,
    state: &WorldSessionState,
    instance: phoxal::model::world::WorldInstanceId,
) {
    match state.lifecycle {
        WorldLifecycle::Ready {
            motion: WorldMotion::Paused,
        } => app.ui.success(format!(
            "robot execution {execution} is active; world physics is paused. Resume with `phoxal simulation resume {instance}`"
        )),
        WorldLifecycle::Ready {
            motion: WorldMotion::Running,
        } => app.ui.success(format!(
            "robot execution {execution} is active; world physics is running"
        )),
        WorldLifecycle::Stopping | WorldLifecycle::Failed { .. } | WorldLifecycle::Starting => {
            app.ui.info(format!(
                "robot execution {execution} attached, but world lifecycle is {:?}",
                state.lifecycle
            ));
        }
    }
}

fn report_world_commands(app: &AppContext, instance: phoxal::model::world::WorldInstanceId) {
    app.ui.info(format!(
        "world physics is independent of robot execution. Inspect it with `phoxal simulation status {instance}`, control it with `phoxal simulation pause {instance}` or `phoxal simulation resume {instance}`, open the terminal view with `phoxal simulation open {instance}`, or stop it with `phoxal simulation stop {instance}`"
    ));
}

pub(super) async fn await_connected_simulation_ending(
    stores: &Stores,
    registration: &LocalWorldRegistration,
    client: &WorldSessionClient,
    execution: phoxal::identity::ExecutionId,
) -> Result<ConnectedSimulationEnding> {
    let instance = registration.instance.to_string();
    tokio::time::timeout(STOP_BUDGET, async {
        loop {
            if let Some(summary) = stores.evidence.read_summary(&instance)? {
                return Ok(ConnectedSimulationEnding::World(Box::new(summary)));
            }
            if stores.registry.find(&instance)?.is_some() {
                if let Some(member) = stores
                    .evidence
                    .read_member_terminal(&instance, execution)?
                {
                    // World stop persists member evidence before its summary and registration
                    // removal. A held registration alone cannot prove this was member-only.
                    if let Ok(state) = client.current_state().await {
                        ensure_state_matches_registration(&state, registration)?;
                        if matches!(state.lifecycle, WorldLifecycle::Ready { .. })
                            && !state.members.iter().any(|member| member.execution == execution)
                        {
                            return Ok(ConnectedSimulationEnding::Member(member));
                        }
                    }
                }
            } else if let Some(summary) = stores.recover_terminal(&instance).await? {
                return Ok(ConnectedSimulationEnding::World(Box::new(summary)));
            }
            tokio::time::sleep(TERMINAL_POLL_INTERVAL).await;
        }
    })
    .await
    .with_context(|| {
        format!(
            "timed out after {}s waiting for typed terminal evidence for member {execution} in world {instance}",
            STOP_BUDGET.as_secs()
        )
    })?
}

fn report_connected_simulation_ending(
    app: &AppContext,
    target: &lifecycle::Target,
    ending: &ConnectedSimulationEnding,
) -> Result<()> {
    let (description, failed) = connected_simulation_ending_description(ending);
    SessionSummary::new(description, vec![target.paths().supervisor_log()])
        .print(&target.project, app.output.theme);
    if failed {
        return Err(ReportedExit(1).into());
    }
    Ok(())
}

pub(super) fn connected_simulation_ending_description(
    ending: &ConnectedSimulationEnding,
) -> (String, bool) {
    match ending {
        ConnectedSimulationEnding::World(summary) => {
            let failed = matches!(&summary.outcome, TerminalOutcome::Failed { .. });
            let detail = summary
                .outcome
                .detail()
                .map_or_else(String::new, |detail| format!(": {detail}"));
            (
                format!(
                    "world terminal outcome {}/{:?}{detail}",
                    summary.outcome.kind(),
                    summary.outcome.reason()
                ),
                failed,
            )
        }
        ConnectedSimulationEnding::Member(member) => {
            let (cleanup, cleanup_failed) = match &member.terminal.cleanup {
                WorldMemberCleanup::Complete => ("complete".to_owned(), false),
                WorldMemberCleanup::Incomplete { detail } => {
                    (format!("incomplete: {detail}"), true)
                }
            };
            (
                format!(
                    "simulation member terminal outcome {:?}; cleanup {cleanup}",
                    member.terminal.reason
                ),
                cleanup_failed || member.terminal.reason != WorldMemberEndReason::Stopped,
            )
        }
    }
}

fn member_is_active(state: &WorldSessionState, execution: phoxal::identity::ExecutionId) -> bool {
    state
        .members
        .iter()
        .any(|member| member.execution == execution && member.phase == WorldMemberPhase::Active)
}

pub(super) async fn connect_verified(
    registration: &LocalWorldRegistration,
) -> Result<WorldSessionClient> {
    let client = WorldSessionClient::connect(&registration.endpoint)
        .await
        .with_context(|| {
            format!(
                "failed to connect to live world instance {}",
                registration.instance
            )
        })?;
    ensure_bootstrap_matches_registration(client.bootstrap(), registration)?;
    Ok(client)
}

pub(super) async fn current_verified(
    registration: &LocalWorldRegistration,
) -> Result<(WorldSessionClient, WorldSessionState)> {
    let client = connect_verified(registration).await?;
    let state = client
        .current_state()
        .await
        .context("failed to read current world state")?;
    ensure_state_matches_registration(&state, registration)?;
    Ok((client, state))
}

pub(super) fn ensure_ready_and_paused(state: &WorldSessionState) -> Result<()> {
    ensure!(
        matches!(
            state.lifecycle,
            WorldLifecycle::Ready {
                motion: WorldMotion::Paused
            }
        ),
        "world host published readiness while its authoritative lifecycle was {}",
        lifecycle_text(state.lifecycle)
    );
    Ok(())
}

pub(super) fn ensure_bootstrap_matches_registration(
    bootstrap: &WorldSessionBootstrap,
    registration: &LocalWorldRegistration,
) -> Result<()> {
    ensure!(
        bootstrap.instance == registration.instance,
        "world endpoint registered for {} bootstrapped as {}; refusing mismatched locator",
        registration.instance,
        bootstrap.instance
    );
    ensure!(
        bootstrap.framework == registration.framework,
        "world {} registered framework {} but bootstrapped as {}",
        registration.instance,
        registration.framework,
        bootstrap.framework
    );
    ensure!(
        bootstrap.world == registration.world.id,
        "world {} registered authored ID {} but bootstrapped as {}",
        registration.instance,
        registration.world.id,
        bootstrap.world
    );
    ensure!(
        bootstrap.digest == registration.world.digest,
        "world {} registered digest {} but bootstrapped as {}",
        registration.instance,
        registration.world.digest,
        bootstrap.digest
    );
    Ok(())
}
