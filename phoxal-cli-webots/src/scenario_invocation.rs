use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use phoxal_engine::RobotIdentity;
use phoxal_utils_scenario::harness::ScenarioEnvironment;
use tokio::process::Command as TokioCommand;

use phoxal_cli_core::AppContext;

use crate::commands::LOCAL_HOST_ROUTER_ENDPOINT;
use crate::commands::session::{Down, Up, WebotsUiMode};

const ENV_LOCALIZE_BACKEND: &str = "ROBOT_LOCALIZE_BACKEND";
const SIMULATOR_TRUTH_BACKEND: &str = "simulator_truth";
const SCENARIO_CONNECT_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone)]
pub struct WebotsScenarioInvocation {
    pub owner_package: String,
    pub scenario_name: String,
    pub fixture_bundle: String,
    pub timeout_secs: u64,
}

pub async fn run_scenario(
    app: &AppContext,
    scenario: &WebotsScenarioInvocation,
    world: &str,
    gui: bool,
    wallclock_timeout_secs: u64,
) -> Result<()> {
    let namespace = "scenario".to_string();
    let robot_id = scenario.fixture_bundle.clone();
    let identity = RobotIdentity::new(robot_id.clone(), namespace.clone());
    let previous_backend = if forces_simulator_truth(&scenario.scenario_name) {
        let previous_backend = std::env::var_os(ENV_LOCALIZE_BACKEND);
        unsafe {
            std::env::set_var(ENV_LOCALIZE_BACKEND, SIMULATOR_TRUTH_BACKEND);
        }
        Some(previous_backend)
    } else {
        None
    };
    let up = Up {
        robot_model: scenario.fixture_bundle.clone(),
        world: world.to_string(),
        local_driver: Vec::new(),
        local_runtime: Vec::new(),
        with_rerun: false,
        with_component: Vec::new(),
        with_joypad: None,
        rust_log: None,
        start_simulation: true,
        robot_id: Some(robot_id.clone()),
        robot_namespace: namespace.clone(),
        no_cache: false,
    };
    let down = Down {
        robot_model: scenario.fixture_bundle.clone(),
        force: true,
        robot_id: Some(robot_id),
        robot_namespace: namespace.clone(),
    };
    let ui_mode = if gui {
        WebotsUiMode::Gui
    } else {
        WebotsUiMode::Headless
    };

    let mut started = None;
    let run_result = match up.start_session(app, ui_mode) {
        Ok(session) => {
            started = Some(session);
            match command_deadline(wallclock_timeout_secs) {
                Ok(deadline) => run_webots_connected(app, scenario, identity, deadline).await,
                Err(error) => Err(error),
            }
        }
        Err(error) => {
            Err(error).map_err(|error| anyhow!("failed to start Webots world '{world}': {error}"))
        }
    };

    let teardown_result = down
        .execute(app)
        .map_err(|error| anyhow!("failed to tear down Webots world '{world}': {error}"));
    if let Some(session) = started {
        session.stop_log_followers();
    }

    if let Some(previous_backend) = previous_backend {
        unsafe {
            match previous_backend {
                Some(value) => std::env::set_var(ENV_LOCALIZE_BACKEND, value),
                None => std::env::remove_var(ENV_LOCALIZE_BACKEND),
            }
        }
    }

    match (run_result, teardown_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(run_error), Ok(())) => Err(run_error),
        (Ok(()), Err(teardown_error)) => Err(teardown_error),
        (Err(run_error), Err(teardown_error)) => {
            Err(run_error).map_err(|error| anyhow!("{error}; {teardown_error}"))
        }
    }
}

async fn run_webots_connected(
    app: &AppContext,
    scenario: &WebotsScenarioInvocation,
    identity: RobotIdentity,
    deadline: Instant,
) -> Result<()> {
    let environment = ScenarioEnvironment::new(
        LOCAL_HOST_ROUTER_ENDPOINT,
        identity.robot_namespace.clone(),
        identity.robot_id.clone(),
    )?;
    let context = with_deadline(
        deadline,
        "scenario bus connect",
        environment.connect(Duration::from_secs(SCENARIO_CONNECT_TIMEOUT_SECS)),
    )
    .await
    .map_err(|error| {
        anyhow!(
            "failed to connect scenario bus for '{}': {error}",
            scenario.scenario_name
        )
    })?;

    with_deadline(
        deadline,
        "simulation status readiness",
        context.wait_until_ready(),
    )
    .await
    .map_err(|error| {
        anyhow!(
            "simulation status did not reach step > 0 for '{}': {error}",
            scenario.scenario_name
        )
    })?;

    with_deadline(deadline, "simulation reset", context.reset_simulation()).await?;
    spawn_webots_scenario_binary(app, &identity, scenario, deadline).await
}

async fn spawn_webots_scenario_binary(
    app: &AppContext,
    identity: &RobotIdentity,
    scenario: &WebotsScenarioInvocation,
    deadline: Instant,
) -> Result<()> {
    let timeout = Duration::from_secs(scenario.timeout_secs).min(remaining_wallclock(deadline)?);
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let robot_config = fixture_bundle_path(app, &scenario.fixture_bundle);
    let mut command = TokioCommand::new(cargo);
    command
        .arg("run")
        .arg("-q")
        .arg("-p")
        .arg(&scenario.owner_package)
        .arg("--")
        .arg("--robot-config")
        .arg(robot_config)
        .arg("--robot-router-endpoint")
        .arg(LOCAL_HOST_ROUTER_ENDPOINT)
        .arg("--robot-id")
        .arg(&identity.robot_id)
        .arg("--robot-namespace")
        .arg(&identity.robot_namespace)
        .arg("scenario")
        .arg("--name")
        .arg(&scenario.scenario_name)
        .current_dir(app.project.root())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let output = tokio::time::timeout(timeout, command.output())
        .await
        .with_context(|| {
            format!(
                "{} scenario '{}' timed out after {}s",
                scenario.owner_package,
                scenario.scenario_name,
                timeout.as_secs()
            )
        })?
        .with_context(|| {
            format!(
                "failed to start scenario binary {} for '{}'",
                scenario.owner_package, scenario.scenario_name
            )
        })?;

    if output.status.success() {
        return Ok(());
    }

    bail!(
        "{} scenario '{}' failed with status {}\nstderr:\n{}\nstdout:\n{}",
        scenario.owner_package,
        scenario.scenario_name,
        output.status,
        String::from_utf8_lossy(&output.stderr).trim_end(),
        String::from_utf8_lossy(&output.stdout).trim_end()
    )
}

fn fixture_bundle_path(app: &AppContext, fixture_bundle: &str) -> String {
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

fn forces_simulator_truth(scenario_name: &str) -> bool {
    !matches!(
        scenario_name,
        "p2-localization-rgbd-inertial-orb-slam3"
            | "p2-mapping-orb-driven-chain"
            | "p2-localization-gnss-anchored"
    )
}

fn command_deadline(timeout_secs: u64) -> Result<Instant> {
    Instant::now()
        .checked_add(Duration::from_secs(timeout_secs))
        .ok_or_else(|| anyhow!("validate scenario wallclock timeout overflows"))
}

async fn with_deadline<T>(
    deadline: Instant,
    label: &str,
    future: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout(remaining_wallclock(deadline)?, future)
        .await
        .map_err(|_| anyhow!("{label} exceeded validate scenario wallclock timeout"))?
}

fn remaining_wallclock(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| anyhow!("validate scenario command exceeded wallclock timeout"))
}
