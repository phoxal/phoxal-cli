//! World compilation, exact-train materialization, and launch commitment.

use super::*;

pub(super) async fn start_world(app: &AppContext, source: &Path) -> Result<StartedWorld> {
    let stores = Stores::discover()?;
    stores.prune()?;

    let staging = tempfile::Builder::new()
        .prefix(".world-launch-")
        .tempdir()
        .context("failed to create a world-bundle launch staging directory")?;
    let bundle_path = staging.path().join("world-bundle");
    let source = source.to_path_buf();
    let destination = bundle_path.clone();
    app.ui.info(format!("compiling world {}", source.display()));
    let compiled = tokio::task::spawn_blocking(move || {
        phoxal_cli_project::compile_world(&source, &destination)
    })
    .await
    .context("world compiler worker failed")??;

    let expected_world = compiled.bundle().world().id().clone();
    let expected_digest = compiled.digest();
    let framework = FrameworkVersion::CURRENT;
    let offline = app.offline;
    app.ui
        .info(format!("materializing simulation host train {framework}"));
    let tools = tokio::task::spawn_blocking(move || {
        phoxal_cli_project::materialize_webots_tools(
            framework,
            offline,
            &phoxal_cli_project::SilentReporter,
        )
    })
    .await
    .context("simulation host materializer worker failed")??;

    let (instance, host) = phoxal_cli_host::world_process::launch(
        tools.host(),
        compiled.path(),
        &stores.paths,
        DEFAULT_LOG_BYTE_LIMIT,
    )
    .await?;
    let registration = match stores.registry.resolve(&instance) {
        Ok(registration) => registration,
        Err(error) => return Err(rollback_host(host, error).await),
    };
    let validation = || -> Result<()> {
        ensure!(
            registration.framework == framework,
            "new world registered framework {}, expected exact host train {framework}",
            registration.framework
        );
        ensure!(
            registration.world.id == expected_world,
            "new world registered ID {}, compiled source was {}",
            registration.world.id,
            expected_world
        );
        ensure!(
            registration.world.digest == expected_digest,
            "new world registered digest {}, compiled bundle was {expected_digest}",
            registration.world.digest
        );
        Ok(())
    };
    if let Err(error) = validation() {
        return Err(rollback_host(host, error).await);
    }
    let ready = match current_verified(&registration).await {
        Ok((_, state)) => ensure_ready_and_paused(&state),
        Err(error) => Err(error),
    };
    if let Err(error) = ready {
        return Err(rollback_host(host, error).await);
    }
    drop(staging);
    Ok(StartedWorld { registration, host })
}

pub(super) async fn rollback_host(host: LaunchedWorldHost, error: anyhow::Error) -> anyhow::Error {
    match host.stop().await {
        Ok(()) => error,
        Err(cleanup) => error.context(format!("world rollback also failed: {cleanup:#}")),
    }
}
