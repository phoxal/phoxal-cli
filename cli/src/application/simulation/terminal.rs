//! Terminal world-view task orchestration and stream reconnection.

use super::connect::connect_verified;
use super::*;

pub(super) async fn open_world_tui(
    app: &AppContext,
    registration: LocalWorldRegistration,
) -> Result<()> {
    let client = connect_verified(&registration).await?;
    let states = client
        .state_subscription()
        .await
        .context("failed to open world state subscription")?;
    let diagnostics = match client.diagnostics_subscription().await {
        Ok(diagnostics) => Some(diagnostics),
        Err(error) => {
            app.ui.warn(format!(
                "world diagnostics are unavailable, but authoritative state remains connected: {error}"
            ));
            None
        }
    };
    let initial_state = states.current().clone();
    let initial_diagnostics = diagnostics
        .as_ref()
        .map(|diagnostics| diagnostics.current());
    ensure_state_matches_registration(&initial_state, &registration)?;

    let (ingress_tx, ingress_rx) = tokio::sync::mpsc::channel(WORLD_UI_INGRESS_CAPACITY);
    let (controls_tx, controls_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut tasks = tokio::task::JoinSet::new();
    spawn_state_feed(
        &mut tasks,
        client.clone(),
        states,
        registration.clone(),
        ingress_tx.clone(),
    );
    if let Some(diagnostics) = diagnostics {
        spawn_diagnostics_feed(&mut tasks, client.clone(), diagnostics, ingress_tx.clone());
    }
    spawn_control_router(
        &mut tasks,
        client,
        registration.clone(),
        controls_rx,
        ingress_tx.clone(),
    );
    drop(ingress_tx);

    let outcome = phoxal_cli_ui::run_world(
        ingress_rx,
        controls_tx,
        phoxal_cli_ui::WorldUiOptions {
            title: "phoxal simulation",
            theme: app.output.theme,
        },
        initial_state,
        initial_diagnostics,
    )
    .await;
    tasks.shutdown().await;
    match outcome? {
        phoxal_cli_ui::WorldOutcome::Detached => app.ui.info(format!(
            "detached from world {}; inspect it with `phoxal simulation status {}`",
            registration.instance, registration.instance
        )),
        phoxal_cli_ui::WorldOutcome::Stopped => app
            .ui
            .success(format!("world {} stopped", registration.instance)),
        phoxal_cli_ui::WorldOutcome::Ended { reason } => {
            bail!(
                "world {} ended{}",
                registration.instance,
                reason.map_or_else(String::new, |reason| format!(": {reason}"))
            );
        }
    }
    Ok(())
}

pub(super) fn spawn_state_feed(
    tasks: &mut tokio::task::JoinSet<()>,
    client: WorldSessionClient,
    mut states: phoxal::session::WorldStateSubscription,
    registration: LocalWorldRegistration,
    ingress: tokio::sync::mpsc::Sender<phoxal_cli_ui::WorldInput>,
) {
    tasks.spawn(async move {
        loop {
            match states.recv().await {
                Ok(state) => {
                    if let Err(error) = ensure_state_matches_registration(state, &registration) {
                        let _ = ingress
                            .send(phoxal_cli_ui::WorldInput::Disconnected {
                                reason: Some(error.to_string()),
                            })
                            .await;
                        return;
                    }
                    if ingress
                        .send(phoxal_cli_ui::WorldInput::State(state.clone()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Err(error) => {
                    match reconnect_state_subscription(&client, &registration, &ingress).await {
                        Ok(reconnected) => states = reconnected,
                        Err(reconnect) => {
                            let _ = ingress
                                .send(phoxal_cli_ui::WorldInput::Disconnected {
                                    reason: Some(format!(
                                        "world state stream ended: {error}; reconnect failed: {reconnect:#}"
                                    )),
                                })
                                .await;
                            return;
                        }
                    }
                }
            }
        }
    });
}

async fn reconnect_state_subscription(
    client: &WorldSessionClient,
    registration: &LocalWorldRegistration,
    ingress: &tokio::sync::mpsc::Sender<phoxal_cli_ui::WorldInput>,
) -> Result<phoxal::session::WorldStateSubscription> {
    tokio::time::sleep(STREAM_RECONNECT_DELAY).await;
    let states = client
        .state_subscription()
        .await
        .context("failed to reopen the world state subscription")?;
    let current = states.current().clone();
    ensure_state_matches_registration(&current, registration)?;
    ingress
        .send(phoxal_cli_ui::WorldInput::State(current))
        .await
        .context("world UI closed during state-stream recovery")?;
    Ok(states)
}

pub(super) fn spawn_diagnostics_feed(
    tasks: &mut tokio::task::JoinSet<()>,
    client: WorldSessionClient,
    mut diagnostics: phoxal::session::WorldDiagnosticsSubscription,
    ingress: tokio::sync::mpsc::Sender<phoxal_cli_ui::WorldInput>,
) {
    tasks.spawn(async move {
        loop {
            match diagnostics.recv().await {
                Ok(diagnostics) => {
                    if ingress
                        .send(phoxal_cli_ui::WorldInput::Diagnostics(diagnostics))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Err(error) => {
                    match reconnect_diagnostics_subscription(&client, &ingress).await {
                        Ok(reconnected) => diagnostics = reconnected,
                        Err(reconnect) => {
                            let _ = ingress
                                .send(phoxal_cli_ui::WorldInput::DiagnosticsUnavailable {
                                    reason: format!(
                                        "diagnostics stream ended: {error}; reconnect failed: {reconnect:#}"
                                    ),
                                })
                                .await;
                            return;
                        }
                    }
                }
            }
        }
    });
}

async fn reconnect_diagnostics_subscription(
    client: &WorldSessionClient,
    ingress: &tokio::sync::mpsc::Sender<phoxal_cli_ui::WorldInput>,
) -> Result<phoxal::session::WorldDiagnosticsSubscription> {
    tokio::time::sleep(STREAM_RECONNECT_DELAY).await;
    let diagnostics = client
        .diagnostics_subscription()
        .await
        .context("failed to reopen the world diagnostics subscription")?;
    ingress
        .send(phoxal_cli_ui::WorldInput::Diagnostics(
            diagnostics.current(),
        ))
        .await
        .context("world UI closed during diagnostics-stream recovery")?;
    Ok(diagnostics)
}

fn spawn_control_router(
    tasks: &mut tokio::task::JoinSet<()>,
    client: WorldSessionClient,
    registration: LocalWorldRegistration,
    mut controls: tokio::sync::mpsc::UnboundedReceiver<WorldControl>,
    ingress: tokio::sync::mpsc::Sender<phoxal_cli_ui::WorldInput>,
) {
    tasks.spawn(async move {
        while let Some(request) = controls.recv().await {
            let input = match client.control(request).await {
                Ok(state) => match ensure_state_matches_registration(&state, &registration) {
                    Ok(()) => phoxal_cli_ui::WorldInput::State(state),
                    Err(error) => phoxal_cli_ui::WorldInput::Disconnected {
                        reason: Some(error.to_string()),
                    },
                },
                Err(error) => phoxal_cli_ui::WorldInput::ControlFailed {
                    request,
                    reason: error.to_string(),
                },
            };
            if ingress.send(input).await.is_err() {
                return;
            }
        }
    });
}
