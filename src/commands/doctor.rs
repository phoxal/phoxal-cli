use anyhow::{Context, Result};
use clap::Args;

use crate::AppContext;
use phoxal_cli_core::project::train::RegistryStatus;

#[derive(Debug, Args)]
pub struct Doctor {}

impl Doctor {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        crate::host_doctor::report(app);
        let train = phoxal_cli_core::project::train::resolve_locked_train(app.project.root())?;
        println!(
            "framework train: {} ({})",
            train.version,
            match &train.source {
                phoxal_cli_core::project::train::TrainSource::Registry => "registry",
                phoxal_cli_core::project::train::TrainSource::Git(_) => "git",
                phoxal_cli_core::project::train::TrainSource::Path => "path",
            }
        );
        println!("train anchor: Cargo.toml and Cargo.lock are coherent");
        if train.is_published() {
            if app.offline || phoxal_cli_core::project::suite::offline_from_env() {
                println!("framework facade: crates.io probe skipped in offline mode");
            } else {
                match inspect_registry_train(train.version.clone()).await {
                    Ok(RegistryStatus::Available) => {
                        println!("framework facade: available on crates.io");
                    }
                    Ok(RegistryStatus::Yanked) => {
                        println!(
                            "warning: locked framework train {} is yanked; existing locked deployment remains valid, but a new Cargo update will not select it",
                            train.version
                        );
                    }
                    Err(error) => {
                        eprintln!(
                            "warning: could not inspect framework train {} on crates.io: {error:#}",
                            train.version
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

async fn inspect_registry_train(version: String) -> Result<RegistryStatus> {
    run_registry_probe(move || phoxal_cli_core::project::train::inspect_registry_train(&version))
        .await
}

async fn run_registry_probe<F>(probe: F) -> Result<RegistryStatus>
where
    F: FnOnce() -> Result<RegistryStatus> + Send + 'static,
{
    tokio::task::spawn_blocking(probe)
        .await
        .context("crates.io probe worker failed")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn blocking_http_client_is_dropped_outside_the_async_runtime() {
        let status = run_registry_probe(|| {
            let _client = reqwest::blocking::Client::new();
            Ok(RegistryStatus::Available)
        })
        .await
        .unwrap();

        assert_eq!(status, RegistryStatus::Available);
    }
}
