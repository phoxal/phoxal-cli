//! Host prerequisite checks.

use anyhow::{Context, Result};
use phoxal_cli_core::project::train::RegistryStatus;

use crate::cli::AppContext;

struct Doctor;

impl Doctor {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        for status in phoxal_cli_project::host::doctor::probes() {
            match status {
                phoxal_cli_project::host::doctor::ProbeStatus::Ok(message) => {
                    app.ui.success(message);
                }
                phoxal_cli_project::host::doctor::ProbeStatus::Warn(message) => {
                    app.ui.warn(message);
                }
                phoxal_cli_project::host::doctor::ProbeStatus::Fail(error) => {
                    app.ui.warn(error.to_string());
                }
            }
        }
        let train =
            phoxal_cli_core::project::train::resolve_locked_train(app.project.root(), app.offline)?;
        println!("framework train: {}", train.version);
        println!("root package: Cargo.toml and Cargo.lock are coherent");
        {
            if app.offline {
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
    run_registry_probe(move || phoxal_cli_project::registry::inspect_registry_train(&version)).await
}

async fn run_registry_probe<F>(probe: F) -> Result<RegistryStatus>
where
    F: FnOnce() -> Result<RegistryStatus> + Send + 'static,
{
    tokio::task::spawn_blocking(probe)
        .await
        .context("crates.io probe worker failed")?
}

pub(crate) async fn run(app: &AppContext) -> Result<()> {
    Doctor.run(app).await
}

pub(crate) async fn doctor_command(app: &AppContext) -> Result<()> {
    run(app).await
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
