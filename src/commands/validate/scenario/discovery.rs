use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use phoxal_cli_core::AppContext;
use phoxal_cli_core::unit::runtime_catalog::PLATFORM_RUNTIME_NAMES;
use phoxal_engine::step::ScenarioDescriptor;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct DiscoveredScenario {
    #[serde(flatten)]
    pub(crate) descriptor: ScenarioDescriptor,
    /// Cargo package name, for example `phoxal-runtime-drive` or `models-robot-v1`.
    pub(crate) owner_package: String,
    /// Workspace-relative fixture bundle or robot model passed via `--robot-config`.
    pub(crate) fixture_bundle: String,
}

/// Enumerate runtime and robot binaries and ask each one for its scenario catalog.
pub(crate) fn discover(app: &AppContext) -> Result<Vec<DiscoveredScenario>> {
    let mut discovered = Vec::new();

    for runtime_name in PLATFORM_RUNTIME_NAMES {
        if *runtime_name == "router" {
            continue;
        }

        let package = format!("phoxal-runtime-{runtime_name}");
        let fixture_bundle = default_fixture_bundle(runtime_name);
        for descriptor in scenarios_for(app, &package, &fixture_bundle)? {
            discovered.push(DiscoveredScenario {
                descriptor,
                owner_package: package.clone(),
                fixture_bundle: fixture_bundle.clone(),
            });
        }
    }

    for robot_model in app.project.discover_robot_models()? {
        let package = format!("models-{robot_model}");
        let fixture_bundle = robot_model.clone();
        for descriptor in scenarios_for(app, &package, &fixture_bundle)? {
            discovered.push(DiscoveredScenario {
                descriptor,
                owner_package: package.clone(),
                fixture_bundle: fixture_bundle.clone(),
            });
        }
    }

    Ok(discovered)
}

fn scenarios_for(
    app: &AppContext,
    package: &str,
    fixture_bundle: &str,
) -> Result<Vec<ScenarioDescriptor>> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let robot_config = fixture_bundle_path(app, fixture_bundle);
    let output = Command::new(&cargo)
        .args([
            "run",
            "-q",
            "-p",
            package,
            "--",
            "--robot-config",
            &robot_config,
            "scenarios",
            "list",
        ])
        .current_dir(app.project.root())
        .output()
        .with_context(|| format!("failed to invoke {package} scenarios list"))?;
    if !output.status.success() {
        bail!(
            "{package} scenarios list exited with {}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim_end()
        );
    }

    serde_json::from_slice::<Vec<ScenarioDescriptor>>(&output.stdout)
        .with_context(|| format!("failed to parse scenarios list JSON from {package}"))
}

fn default_fixture_bundle(_runtime_name: &str) -> String {
    "rgbd-imu-diff-drive".to_string()
}

pub(crate) fn fixture_bundle_path(app: &AppContext, fixture_bundle: &str) -> String {
    if Path::new(fixture_bundle).is_absolute() {
        return fixture_bundle.to_string();
    }

    let candidate = app
        .project
        .root()
        .join("fixture")
        .join("robot")
        .join(fixture_bundle);
    if candidate.exists() {
        return candidate.display().to_string();
    }

    app.project
        .root()
        .join("models")
        .join(fixture_bundle)
        .display()
        .to_string()
}
