//! Host prerequisite checks.

use anyhow::{Context, Result};
use phoxal_cli_project::source::train::RegistryStatus;

use crate::cli::context::AppContext;

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
        for line in development_overrides() {
            app.ui.warn(line);
        }
        let train = phoxal_cli_project::source::train::resolve_locked_train(
            app.project.root(),
            app.offline,
        )?;
        println!("framework train: {}", train.version());
        println!("root package: Cargo.toml and Cargo.lock are coherent");
        {
            if app.offline {
                println!("framework facade: crates.io probe skipped in offline mode");
            } else {
                match inspect_registry_train(train.version().to_string()).await {
                    Ok(RegistryStatus::Available) => {
                        println!("framework facade: available on crates.io");
                    }
                    Ok(RegistryStatus::Yanked) => {
                        println!(
                            "warning: locked framework train {} is yanked; existing locked deployment remains valid, but a new Cargo update will not select it",
                            train.version()
                        );
                    }
                    Err(error) => {
                        eprintln!(
                            "warning: could not inspect framework train {} on crates.io: {error:#}",
                            train.version()
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

/// The development overrides in effect, each named with what it replaces.
///
/// They are reported as warnings rather than as facts: an operator running
/// binaries built from a local checkout instead of the published train is in a
/// state they must be able to see at a glance, and a `doctor` that stayed quiet
/// about it would be the one place they would expect to find out.
fn development_overrides() -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(checkout) = phoxal_cli_project::framework_path() {
        lines.push(format!(
            "{}={} is set: every official service, component driver, and the supervisor are built \
             from that checkout instead of the phoxal registry",
            phoxal_cli_project::FRAMEWORK_PATH_VAR,
            checkout.display()
        ));
    }
    if let Some(checkout) = phoxal_cli_project::simulator_webots_path() {
        lines.push(format!(
            "{}={} is set: the Webots controller is built from that checkout instead of the \
             phoxal registry",
            phoxal_cli_project::SIMULATOR_WEBOTS_PATH_VAR,
            checkout.display()
        ));
    }
    lines
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

    /// The two overrides are named by the variable that sets them and by what
    /// they replace, so an operator reading `doctor` output knows exactly which
    /// binaries they are about to run.
    #[test]
    fn every_development_override_names_its_variable_and_what_it_replaces() {
        assert_eq!(
            phoxal_cli_project::FRAMEWORK_PATH_VAR,
            "PHOXAL_FRAMEWORK_PATH"
        );
        assert_eq!(
            phoxal_cli_project::SIMULATOR_WEBOTS_PATH_VAR,
            "PHOXAL_SIMULATOR_WEBOTS_PATH"
        );
        // The reporter reads the process environment, which this test does not
        // mutate: an unset override reports nothing at all.
        for line in development_overrides() {
            assert!(line.contains("instead of the phoxal registry"), "{line}");
        }
    }

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
