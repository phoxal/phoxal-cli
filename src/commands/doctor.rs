use anyhow::Result;
use clap::Args;

use crate::AppContext;

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
                match phoxal_cli_core::project::train::inspect_registry_train(&train.version) {
                    Ok(phoxal_cli_core::project::train::RegistryStatus::Available) => {
                        println!("framework facade: available on crates.io");
                    }
                    Ok(phoxal_cli_core::project::train::RegistryStatus::Yanked) => {
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
