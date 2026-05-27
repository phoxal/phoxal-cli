use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use phoxal_engine::step::ScenarioDescriptor;
use phoxal_engine::step::ScenarioKind;
use phoxal_utils_scenario_spec::{
    ScenarioOutcome, ScenarioReport, ScenarioResult, ScenarioSpec, WebotsWorld,
};
use serde::Serialize;
use tokio::process::Command as TokioCommand;

use phoxal_cli_core::AppContext;
use phoxal_cli_webots::WebotsScenarioInvocation;

use super::ScenarioRequest;
use super::catalog::{Category, ProfileRequirement};
use super::discovery::{DiscoveredScenario, fixture_bundle_path};
use super::result::PhaseReport;

#[derive(Debug, Serialize)]
pub(super) struct ScenarioListing {
    #[serde(flatten)]
    descriptor: ScenarioDescriptor,
    owner_package: String,
    fixture_bundle: String,
}

pub(super) fn listing(app: &AppContext) -> Result<Vec<ScenarioListing>> {
    let entries = super::discovery::discover(app)?
        .into_iter()
        .map(|scenario| ScenarioListing {
            descriptor: scenario.descriptor.clone(),
            owner_package: scenario.owner_package.clone(),
            fixture_bundle: fixture_bundle_for(&scenario).to_string(),
        })
        .collect::<Vec<_>>();
    Ok(entries)
}

pub(super) struct ScenarioRunner {
    plans: Vec<DiscoveredScenario>,
}

impl ScenarioRunner {
    pub(super) fn discover(app: &AppContext) -> Result<Self> {
        Ok(Self {
            plans: super::discovery::discover(app)?,
        })
    }

    pub(super) async fn run(
        &self,
        request: &ScenarioRequest,
        app: &AppContext,
    ) -> Result<PhaseReport> {
        let selected = self.select(request)?;
        let mut results = Vec::with_capacity(selected.len());
        for plan in selected {
            results.push(run_one(app, plan, request).await);
        }

        let selector = request.to_string();
        Ok(PhaseReport {
            selector: selector.clone(),
            report: ScenarioReport::new(selector, results),
        })
    }

    fn select(&self, request: &ScenarioRequest) -> Result<Vec<&DiscoveredScenario>> {
        let entries = match request {
            ScenarioRequest::Phase { phase, .. } => {
                let requirement = super::catalog::phase_requirement(*phase);
                self.plans
                    .iter()
                    .filter(|scenario| {
                        scenario.descriptor.phase == *phase
                            && scenario.descriptor.tier == requirement.tier
                            && requirement
                                .categories
                                .iter()
                                .any(|category| category_matches(scenario, *category))
                            && profile(scenario) == ProfileRequirement::RequiredByV1Reference
                    })
                    .collect::<Vec<_>>()
            }
            ScenarioRequest::TierCategories {
                tier, categories, ..
            } => self
                .plans
                .iter()
                .filter(|scenario| {
                    scenario.descriptor.tier == *tier
                        && categories
                            .iter()
                            .any(|category| category_matches(scenario, *category))
                        && profile(scenario) == ProfileRequirement::RequiredByV1Reference
                })
                .collect::<Vec<_>>(),
            ScenarioRequest::Scenario { name, .. } => self
                .plans
                .iter()
                .filter(|scenario| scenario.descriptor.name.as_ref() == name)
                .collect::<Vec<_>>(),
        };

        if entries.is_empty() {
            bail!("scenario catalog has no scenarios for {request}");
        }

        Ok(entries)
    }
}

fn category_matches(scenario: &DiscoveredScenario, category: Category) -> bool {
    Category::parse(scenario.descriptor.category.as_ref())
        .map(|candidate| candidate == category)
        .unwrap_or(false)
}

fn profile(scenario: &DiscoveredScenario) -> ProfileRequirement {
    match scenario.descriptor.name.as_ref() {
        "p2-mapping-orb-driven-chain" => ProfileRequirement::ProfileConditional("orb_slam3"),
        "p2-localization-gnss-anchored" => ProfileRequirement::ProfileConditional("gnss"),
        _ => ProfileRequirement::RequiredByV1Reference,
    }
}

async fn run_one(
    app: &AppContext,
    scenario: &DiscoveredScenario,
    request: &ScenarioRequest,
) -> ScenarioResult {
    match &scenario.descriptor.kind {
        ScenarioKind::Headless => match spawn_scenario_binary(app, scenario).await {
            Ok(()) => ScenarioResult {
                spec: discovered_spec(scenario, "headless"),
                outcome: ScenarioOutcome::Passed,
                elapsed_logical_ns: Some(0),
                failure_diagnostics: Vec::new(),
            },
            Err(error) => ScenarioResult {
                spec: discovered_spec(scenario, "headless"),
                outcome: ScenarioOutcome::Failed,
                elapsed_logical_ns: Some(0),
                failure_diagnostics: vec![error.to_string()],
            },
        },
        ScenarioKind::Webots { world } => {
            let spec = discovered_spec(scenario, world.as_ref());
            let invocation = WebotsScenarioInvocation {
                owner_package: scenario.owner_package.clone(),
                scenario_name: scenario.descriptor.name.as_ref().to_string(),
                fixture_bundle: fixture_bundle_for(scenario).to_string(),
                timeout_secs: scenario.descriptor.timeout_secs,
            };
            match phoxal_cli_webots::run_scenario(
                app,
                &invocation,
                world.as_ref(),
                request.gui(),
                request.wallclock_timeout_secs(),
            )
            .await
            {
                Ok(()) => ScenarioResult {
                    spec,
                    outcome: ScenarioOutcome::Passed,
                    elapsed_logical_ns: None,
                    failure_diagnostics: Vec::new(),
                },
                Err(error) => ScenarioResult {
                    spec,
                    outcome: outcome_for_error(&error),
                    elapsed_logical_ns: None,
                    failure_diagnostics: vec![error.to_string()],
                },
            }
        }
    }
}

fn discovered_spec(scenario: &DiscoveredScenario, world: &str) -> ScenarioSpec {
    ScenarioSpec::new(scenario.descriptor.name.as_ref())
        .world(WebotsWorld::named(world))
        .test(scenario.descriptor.name.as_ref())
        .timeout_secs(scenario.descriptor.timeout_secs)
        .fixture_bundle(fixture_bundle_for(scenario))
        .category(scenario.descriptor.category.as_ref())
        .tier(scenario.descriptor.tier)
}

async fn spawn_scenario_binary(app: &AppContext, scenario: &DiscoveredScenario) -> Result<()> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let robot_config = fixture_bundle_path(app, &scenario.fixture_bundle);
    let mut command = TokioCommand::new(&cargo);
    command
        .arg("run")
        .arg("-q")
        .arg("-p")
        .arg(&scenario.owner_package)
        .arg("--")
        .arg("--robot-config")
        .arg(robot_config)
        .arg("scenario")
        .arg("--name")
        .arg(scenario.descriptor.name.as_ref())
        .current_dir(app.project.root())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let timeout = Duration::from_secs(scenario.descriptor.timeout_secs);
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .with_context(|| {
            format!(
                "{} scenario '{}' timed out after {}s",
                scenario.owner_package, scenario.descriptor.name, scenario.descriptor.timeout_secs
            )
        })?
        .with_context(|| {
            format!(
                "failed to invoke {} scenario {}",
                scenario.owner_package, scenario.descriptor.name
            )
        })?;

    if output.status.success() {
        return Ok(());
    }

    bail!(
        "{} scenario '{}' failed with status {}\nstderr:\n{}\nstdout:\n{}",
        scenario.owner_package,
        scenario.descriptor.name,
        output.status,
        String::from_utf8_lossy(&output.stderr).trim_end(),
        String::from_utf8_lossy(&output.stdout).trim_end()
    )
}

fn fixture_bundle_for(scenario: &DiscoveredScenario) -> &str {
    match scenario.descriptor.name.as_ref() {
        "p2-localization-gnss-anchored" => "rgbd-imu-gnss-outdoor",
        "p2-mapping-orb-driven-chain" => "rgbd-diff-drive",
        "p3-safety-range-sensor-staleness" => "lowrate-range-diff-drive",
        "p3-failure-recovery-runtime-loss" | "p5-mission-single-goal" => "rgbd-imu-diff-drive",
        _ => &scenario.fixture_bundle,
    }
}

fn outcome_for_error(error: &anyhow::Error) -> ScenarioOutcome {
    let message = error.to_string();
    if message.contains("wallclock timeout") || message.contains("timed out") {
        ScenarioOutcome::Timeout
    } else {
        ScenarioOutcome::Failed
    }
}
