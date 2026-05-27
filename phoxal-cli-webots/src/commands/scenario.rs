use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use phoxal_engine::RobotIdentity;
use phoxal_utils_helpers::parse_trimmed_non_empty;
use phoxal_utils_scenario::definition::ScenarioSpec;
use phoxal_utils_scenario::harness::ScenarioEnvironment;
use phoxal_utils_scenario::records::{ScenarioOutcome, ScenarioReport, ScenarioResult};
use serde::{Deserialize, Serialize};
use tokio::process::Command as TokioCommand;

use phoxal_cli_core::AppContext;

use super::LOCAL_HOST_ROUTER_ENDPOINT;
use super::session::{Down, Up, WebotsUiMode};

const SCENARIO_CONNECT_TIMEOUT_SECS: u64 = 30;

struct WorldRun<'a> {
    app: &'a AppContext,
    robot_config: &'a Path,
    identity: &'a RobotIdentity,
    world: &'a str,
    specs: Vec<ScenarioSpec>,
    deadline: Instant,
    interrupted: Arc<AtomicBool>,
}

#[derive(Debug, Parser, Clone)]
pub struct Scenario {
    /// Robot model directory under `models/` (e.g. "robot-v1").
    pub robot_model: String,

    /// Comma-separated scenario names. Empty means "all discovered".
    #[arg(long = "scenario", value_delimiter = ',', value_parser = parse_trimmed_non_empty)]
    pub scenarios: Vec<String>,

    /// Run Webots headless (no GUI). Default is GUI for this interactive command.
    #[arg(long)]
    pub headless: bool,

    /// Optional RUST_LOG override propagated into the simulator session.
    #[arg(long)]
    pub rust_log: Option<String>,

    /// Robot identifier; defaults to a stable value derived from `robot_model`.
    #[arg(long, value_parser = parse_trimmed_non_empty)]
    pub robot_id: Option<String>,

    /// Robot namespace used as the `phoxal-bus` topic prefix.
    #[arg(
        long,
        default_value_t = String::from("scenario"),
        value_parser = parse_trimmed_non_empty
    )]
    pub robot_namespace: String,

    /// Maximum wallclock seconds the whole command may run before failing.
    #[arg(long, default_value_t = 600)]
    pub wallclock_timeout_secs: u64,
}

impl Scenario {
    pub async fn execute(&self, app: &AppContext) -> Result<()> {
        let deadline = command_deadline(self.wallclock_timeout_secs)?;
        let interrupted = install_interrupt_flag()?;
        let robot_config = app.project.model_dir(&self.robot_model);
        let specs = discover_scenarios(&self.robot_model, &robot_config, deadline).await?;
        validate_specs(&specs)?;
        let selected = filter_scenarios(&specs, &self.scenarios)?;
        validate_worlds(app, &selected)?;
        let grouped = group_by_world(selected);

        let identity = RobotIdentity::new(
            self.robot_id
                .clone()
                .unwrap_or_else(|| self.robot_model.clone()),
            self.robot_namespace.clone(),
        );

        let mut results = Vec::new();
        let mut teardown_errors = Vec::new();
        for (world, specs) in grouped {
            let world_results = self
                .run_world(WorldRun {
                    app,
                    robot_config: &robot_config,
                    identity: &identity,
                    world: &world,
                    specs,
                    deadline,
                    interrupted: interrupted.clone(),
                })
                .await;
            match world_results {
                Ok(world_results) => results.extend(world_results),
                Err(error) => {
                    teardown_errors.push(error.to_string());
                }
            }
        }

        let report = ScenarioReport::new(format!("webots:{}", self.robot_model), results);
        print_report(&report, &teardown_errors)?;

        if report.has_failures() || !teardown_errors.is_empty() {
            let failures = report
                .results
                .iter()
                .filter(|result| result.outcome != ScenarioOutcome::Passed)
                .map(|result| format!("{}:{:?}", result.spec.name, result.outcome))
                .collect::<Vec<_>>()
                .join(", ");
            let teardown = teardown_errors.join("; ");
            bail!(
                "webots robot-acceptance scenario runner failed for {}; failures [{}]{}",
                self.robot_model,
                failures,
                if teardown.is_empty() {
                    String::new()
                } else {
                    format!("; teardown errors [{teardown}]")
                }
            );
        }

        Ok(())
    }

    async fn run_world(&self, run: WorldRun<'_>) -> Result<Vec<ScenarioResult>> {
        if run.interrupted.load(Ordering::Relaxed) {
            return Ok(startup_failure_results(
                &run.specs,
                "interrupted by Ctrl+C".to_string(),
            ));
        }

        let up = Up {
            robot_model: self.robot_model.clone(),
            world: run.world.to_string(),
            local_driver: Vec::new(),
            local_runtime: Vec::new(),
            with_rerun: false,
            with_component: Vec::new(),
            with_joypad: None,
            rust_log: self.rust_log.clone(),
            start_simulation: true,
            robot_id: Some(run.identity.robot_id.clone()),
            robot_namespace: run.identity.robot_namespace.clone(),
            no_cache: false,
        };
        let down = Down {
            robot_model: self.robot_model.clone(),
            force: true,
            robot_id: Some(run.identity.robot_id.clone()),
            robot_namespace: run.identity.robot_namespace.clone(),
        };
        let environment = ScenarioEnvironment::new(
            LOCAL_HOST_ROUTER_ENDPOINT,
            run.identity.robot_namespace.clone(),
            run.identity.robot_id.clone(),
        )?;

        let mut started = None;
        let mut results = match up.start_session(
            run.app,
            if self.headless {
                WebotsUiMode::Headless
            } else {
                WebotsUiMode::Gui
            },
        ) {
            Ok(session) => {
                started = Some(session);
                match with_deadline(
                    run.deadline,
                    "scenario bus connect",
                    environment.connect(Duration::from_secs(SCENARIO_CONNECT_TIMEOUT_SECS)),
                )
                .await
                {
                    Ok(context) => match with_deadline(
                        run.deadline,
                        "simulation status readiness",
                        context.wait_until_ready(),
                    )
                    .await
                    {
                        Ok(_) => {
                            self.run_scenarios(
                                run.robot_config,
                                &context,
                                &run.specs,
                                run.deadline,
                                run.interrupted.clone(),
                            )
                            .await
                        }
                        Err(error) => startup_failure_results(
                            &run.specs,
                            format!(
                                "simulation status did not reach step > 0 for world '{}': {error}",
                                run.world
                            ),
                        ),
                    },
                    Err(error) => startup_failure_results(
                        &run.specs,
                        format!(
                            "failed to connect scenario bus for world '{}': {error}",
                            run.world
                        ),
                    ),
                }
            }
            Err(error) => startup_failure_results(
                &run.specs,
                format!("failed to start Webots world '{}': {error}", run.world),
            ),
        };

        let teardown = down.execute(run.app);
        if let Some(session) = started {
            session.stop_log_followers();
        }
        if let Err(error) = teardown {
            results.push(ScenarioResult {
                spec: ScenarioSpec::new(format!("teardown-{}", run.world))
                    .world(phoxal_utils_scenario::definition::WebotsWorld::named(
                        run.world,
                    ))
                    .test("teardown")
                    .timeout_secs(0),
                outcome: ScenarioOutcome::TeardownError,
                elapsed_logical_ns: None,
                failure_diagnostics: vec![error.to_string()],
            });
        }

        Ok(results)
    }

    async fn run_scenarios(
        &self,
        robot_config: &Path,
        context: &phoxal_utils_scenario::harness::ScenarioContext,
        specs: &[ScenarioSpec],
        deadline: Instant,
        interrupted: Arc<AtomicBool>,
    ) -> Vec<ScenarioResult> {
        let mut results = Vec::new();
        for spec in specs {
            if interrupted.load(Ordering::Relaxed) {
                results.push(ScenarioResult {
                    spec: spec.clone(),
                    outcome: ScenarioOutcome::Failed,
                    elapsed_logical_ns: None,
                    failure_diagnostics: vec!["interrupted by Ctrl+C".to_string()],
                });
                break;
            }
            let result = self
                .run_one_scenario(robot_config, context, spec, deadline)
                .await;
            let interrupted_after = interrupted.load(Ordering::Relaxed);
            results.push(result);
            if interrupted_after {
                break;
            }
        }
        results
    }

    async fn run_one_scenario(
        &self,
        robot_config: &Path,
        context: &phoxal_utils_scenario::harness::ScenarioContext,
        spec: &ScenarioSpec,
        deadline: Instant,
    ) -> ScenarioResult {
        let reset_result =
            with_deadline(deadline, "simulation reset", context.reset_simulation()).await;
        if let Err(error) = reset_result {
            return ScenarioResult {
                spec: spec.clone(),
                outcome: ScenarioOutcome::Failed,
                elapsed_logical_ns: None,
                failure_diagnostics: vec![error.to_string()],
            };
        }

        match run_scenario(robot_config, spec, self, deadline).await {
            Ok(ScenarioRunOutcome::Passed) => ScenarioResult {
                spec: spec.clone(),
                outcome: ScenarioOutcome::Passed,
                elapsed_logical_ns: None,
                failure_diagnostics: Vec::new(),
            },
            Ok(ScenarioRunOutcome::Failed(status)) => ScenarioResult {
                spec: spec.clone(),
                outcome: ScenarioOutcome::Failed,
                elapsed_logical_ns: None,
                failure_diagnostics: vec![format!("scenario binary exited with status {status}")],
            },
            Ok(ScenarioRunOutcome::TimedOut) => ScenarioResult {
                spec: spec.clone(),
                outcome: ScenarioOutcome::Timeout,
                elapsed_logical_ns: None,
                failure_diagnostics: vec![format!(
                    "scenario exceeded {} second timeout",
                    spec.timeout_secs
                )],
            },
            Err(error) => ScenarioResult {
                spec: spec.clone(),
                outcome: ScenarioOutcome::Failed,
                elapsed_logical_ns: None,
                failure_diagnostics: vec![error.to_string()],
            },
        }
    }
}

enum ScenarioRunOutcome {
    Passed,
    Failed(std::process::ExitStatus),
    TimedOut,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct RuntimeScenarioDescriptor {
    name: String,
    kind: RuntimeScenarioKind,
    timeout_secs: u64,
    category: String,
    tier: u8,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "kebab-case")]
enum RuntimeScenarioKind {
    Headless,
    Webots { world: String },
}

impl RuntimeScenarioDescriptor {
    fn into_spec(self) -> Result<ScenarioSpec> {
        let RuntimeScenarioKind::Webots { world } = self.kind else {
            bail!(
                "robot acceptance runner can only execute Webots scenarios; '{}' is headless",
                self.name
            );
        };
        let name = self.name;
        Ok(ScenarioSpec::new(name.clone())
            .world(phoxal_utils_scenario::definition::WebotsWorld::named(world))
            .test(name)
            .timeout_secs(self.timeout_secs)
            .category(self.category)
            .tier(self.tier))
    }
}

fn install_interrupt_flag() -> Result<Arc<AtomicBool>> {
    let interrupted = Arc::new(AtomicBool::new(false));
    let handler_flag = interrupted.clone();
    ctrlc::set_handler(move || {
        handler_flag.store(true, Ordering::Relaxed);
    })
    .context("failed to install Ctrl+C handler for Webots scenario runner")?;
    Ok(interrupted)
}

fn startup_failure_results(specs: &[ScenarioSpec], diagnostic: String) -> Vec<ScenarioResult> {
    specs
        .iter()
        .map(|spec| ScenarioResult {
            spec: spec.clone(),
            outcome: ScenarioOutcome::Failed,
            elapsed_logical_ns: None,
            failure_diagnostics: vec![diagnostic.clone()],
        })
        .collect()
}

async fn discover_scenarios(
    robot_model: &str,
    robot_config: &Path,
    deadline: Instant,
) -> Result<Vec<ScenarioSpec>> {
    if !robot_config.is_dir() {
        bail!(
            "robot model directory is missing: {}",
            robot_config.display()
        );
    }

    let package = scenario_package(robot_model);
    let mut command = TokioCommand::new("cargo");
    command
        .arg("run")
        .arg("-p")
        .arg(&package)
        .arg("--quiet")
        .arg("--")
        .arg("--robot-config")
        .arg(robot_config)
        .arg("scenarios")
        .arg("list");
    let output = with_deadline(deadline, "scenario list", async move {
        command
            .output()
            .await
            .context("failed to run robot scenario list command")
    })
    .await?;
    if !output.status.success() {
        bail!(
            "scenario list failed with status {}; stderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let descriptors: Vec<RuntimeScenarioDescriptor> = serde_json::from_slice(&output.stdout)
        .with_context(|| {
            format!("failed to parse JSON scenario metadata from package {package}")
        })?;
    descriptors
        .into_iter()
        .map(RuntimeScenarioDescriptor::into_spec)
        .collect()
}

fn scenario_package(robot_model: &str) -> String {
    format!("models-{robot_model}")
}

fn validate_specs(specs: &[ScenarioSpec]) -> Result<()> {
    if specs.is_empty() {
        bail!("robot scenario catalog is empty");
    }

    let mut names = BTreeSet::new();
    for spec in specs {
        if spec.name.trim().is_empty() {
            bail!("scenario metadata contains an empty name");
        }
        if spec.world.as_str().trim().is_empty() {
            bail!("scenario '{}' has an empty world", spec.name);
        }
        if spec.timeout_secs == 0 {
            bail!("scenario '{}' must declare a positive timeout", spec.name);
        }
        if !names.insert(spec.name.clone()) {
            bail!("scenario '{}' is declared more than once", spec.name);
        }
    }

    Ok(())
}

fn filter_scenarios(specs: &[ScenarioSpec], requested: &[String]) -> Result<Vec<ScenarioSpec>> {
    if requested.is_empty() {
        return Ok(specs.to_vec());
    }

    let by_name = specs
        .iter()
        .map(|spec| (spec.name.as_str(), spec))
        .collect::<BTreeMap<_, _>>();
    let mut selected = Vec::new();
    let mut missing = Vec::new();
    for name in requested {
        match by_name.get(name.as_str()) {
            Some(spec) => selected.push((*spec).clone()),
            None => missing.push(name.clone()),
        }
    }

    if !missing.is_empty() {
        let available = specs
            .iter()
            .map(|spec| spec.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "unknown robot scenario(s): {}; available scenarios: {}",
            missing.join(", "),
            available
        );
    }

    Ok(selected)
}

fn validate_worlds(app: &AppContext, specs: &[ScenarioSpec]) -> Result<()> {
    let missing = specs
        .iter()
        .filter_map(|spec| {
            let path = app.project.webots_world_source(spec.world.as_str());
            (!path.is_file()).then(|| format!("{} ({})", spec.world.as_str(), path.display()))
        })
        .collect::<BTreeSet<_>>();
    if !missing.is_empty() {
        bail!(
            "missing Webots world source file(s): {}",
            missing.into_iter().collect::<Vec<_>>().join(", ")
        );
    }

    Ok(())
}

fn group_by_world(specs: Vec<ScenarioSpec>) -> BTreeMap<String, Vec<ScenarioSpec>> {
    let mut grouped = BTreeMap::new();
    for spec in specs {
        grouped
            .entry(spec.world.as_str().to_string())
            .or_insert_with(Vec::new)
            .push(spec);
    }
    grouped
}

async fn run_scenario(
    robot_config: &Path,
    spec: &ScenarioSpec,
    scenario: &Scenario,
    deadline: Instant,
) -> Result<ScenarioRunOutcome> {
    let timeout = Duration::from_secs(spec.timeout_secs).min(remaining_wallclock(deadline)?);
    let package = scenario_package(&scenario.robot_model);
    let mut command = TokioCommand::new("cargo");
    command
        .arg("run")
        .arg("-p")
        .arg(package)
        .arg("--quiet")
        .arg("--")
        .arg("--robot-config")
        .arg(robot_config)
        .arg("--robot-router-endpoint")
        .arg(LOCAL_HOST_ROUTER_ENDPOINT)
        .arg("--robot-id")
        .arg(robot_id(scenario))
        .arg("--robot-namespace")
        .arg(&scenario.robot_namespace)
        .arg("scenario")
        .arg("--name")
        .arg(&spec.name);

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to start scenario binary for '{}'", spec.name))?;
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(ScenarioRunOutcome::Passed),
        Ok(Ok(status)) => Ok(ScenarioRunOutcome::Failed(status)),
        Ok(Err(error)) => {
            Err(error).with_context(|| format!("failed while waiting for scenario '{}'", spec.name))
        }
        Err(_) => {
            let _ = child.kill().await;
            Ok(ScenarioRunOutcome::TimedOut)
        }
    }
}

fn robot_id(scenario: &Scenario) -> String {
    scenario
        .robot_id
        .clone()
        .unwrap_or_else(|| scenario.robot_model.clone())
}

fn command_deadline(timeout_secs: u64) -> Result<Instant> {
    Instant::now()
        .checked_add(Duration::from_secs(timeout_secs))
        .ok_or_else(|| anyhow!("webots scenario wallclock timeout overflows"))
}

async fn with_deadline<T>(
    deadline: Instant,
    label: &str,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout(remaining_wallclock(deadline)?, future)
        .await
        .map_err(|_| anyhow!("{label} exceeded webots scenario wallclock timeout"))?
}

fn remaining_wallclock(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| anyhow!("webots scenario command exceeded wallclock timeout"))
}

#[derive(Serialize)]
struct ScenarioReportLine<'a> {
    kind: &'static str,
    name: &'a str,
    world: &'a str,
    outcome: ScenarioOutcome,
    diagnostics: &'a [String],
}

#[derive(Serialize)]
struct ScenarioSummaryLine {
    kind: &'static str,
    passed: usize,
    failed: usize,
    timeout: usize,
    teardown_error: usize,
}

fn print_report(report: &ScenarioReport, teardown_errors: &[String]) -> Result<()> {
    for result in &report.results {
        let line = ScenarioReportLine {
            kind: "webots-scenario-result",
            name: &result.spec.name,
            world: result.spec.world.as_str(),
            outcome: result.outcome,
            diagnostics: &result.failure_diagnostics,
        };
        println!("{}", serde_json::to_string(&line)?);
    }

    for error in teardown_errors {
        let line = serde_json::json!({
            "kind": "webots-scenario-teardown-error",
            "diagnostics": [error],
        });
        println!("{}", serde_json::to_string(&line)?);
    }

    let summary = ScenarioSummaryLine {
        kind: "webots-scenario-summary",
        passed: report
            .results
            .iter()
            .filter(|result| result.outcome == ScenarioOutcome::Passed)
            .count(),
        failed: report
            .results
            .iter()
            .filter(|result| result.outcome == ScenarioOutcome::Failed)
            .count(),
        timeout: report
            .results
            .iter()
            .filter(|result| result.outcome == ScenarioOutcome::Timeout)
            .count(),
        teardown_error: report
            .results
            .iter()
            .filter(|result| result.outcome == ScenarioOutcome::TeardownError)
            .count()
            + teardown_errors.len(),
    };
    println!("{}", serde_json::to_string(&summary)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_utils_scenario::definition::WebotsWorld;

    fn spec(name: &str, world: &str, test: &str) -> ScenarioSpec {
        ScenarioSpec::new(name)
            .world(WebotsWorld::named(world))
            .test(test)
            .timeout_secs(30)
    }

    #[test]
    fn cli_accepts_comma_separated_scenario_filter() {
        let args = Scenario::parse_from([
            "scenario",
            "robot-v1",
            "--scenario",
            "drive-forward,drive-backward",
        ]);

        assert_eq!(args.robot_model, "robot-v1");
        assert_eq!(args.scenarios, ["drive-forward", "drive-backward"]);
        assert!(!args.headless);
        assert_eq!(args.robot_namespace, "scenario");
    }

    #[test]
    fn cli_accepts_headless_opt_out() {
        let args = Scenario::parse_from(["scenario", "robot-v1", "--headless"]);

        assert!(args.headless);
    }

    #[test]
    fn filter_rejects_unknown_requested_scenario() {
        let specs = vec![spec("drive-forward", "ArenaWorld", "drive_forward")];

        let error = filter_scenarios(&specs, &["drive-backward".to_string()]).unwrap_err();

        assert!(error.to_string().contains("unknown robot scenario(s)"));
        assert!(error.to_string().contains("drive-forward"));
    }

    #[test]
    fn filter_returns_all_scenarios_when_no_filter_is_requested() {
        let specs = vec![
            spec("drive-forward", "ArenaWorld", "drive_forward"),
            spec("turn-left", "ArenaWorld", "turn_left"),
        ];

        assert_eq!(filter_scenarios(&specs, &[]).unwrap(), specs);
    }

    #[test]
    fn group_by_world_preserves_world_batches() {
        let grouped = group_by_world(vec![
            spec("drive-forward", "ArenaWorld", "drive_forward"),
            spec("turn-left", "ArenaWorld", "turn_left"),
            spec("climb", "RampWorld", "climb"),
        ]);

        assert_eq!(
            grouped
                .get("ArenaWorld")
                .unwrap()
                .iter()
                .map(|spec| spec.name.as_str())
                .collect::<Vec<_>>(),
            ["drive-forward", "turn-left"]
        );
        assert_eq!(
            grouped
                .get("RampWorld")
                .unwrap()
                .iter()
                .map(|spec| spec.name.as_str())
                .collect::<Vec<_>>(),
            ["climb"]
        );
    }

    #[test]
    fn validate_specs_rejects_duplicate_names() {
        let specs = vec![
            spec("drive-forward", "ArenaWorld", "drive_forward"),
            spec("drive-forward", "ArenaWorld", "drive_forward_again"),
        ];

        assert!(validate_specs(&specs).is_err());
    }
}
