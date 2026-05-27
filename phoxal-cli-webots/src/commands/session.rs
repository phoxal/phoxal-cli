use std::fs;
use std::path::PathBuf;
use std::process::Command as ProcessCommand;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use phoxal_engine::RobotIdentity;
use phoxal_utils_conventions::{
    DEFAULT_ROBOT_NAMESPACE, component_package_name, runtime_package_name,
};
use phoxal_utils_helpers::parse_trimmed_non_empty;

use phoxal_cli_core::AppContext;
use phoxal_cli_core::unit::Unit;
use phoxal_cli_core::unit::compose::{ComposeMode, GenerateCompose};
use phoxal_cli_core::unit::container::{
    BuildPlatform, ContainerSelectionParams, available_component_drivers, plan_local_topology,
};
use phoxal_cli_core::unit::publish::BuildImages;
use phoxal_cli_core::unit::robot::ValidatedRobot;

use super::process::{
    AttachedLogFollowers, SpawnLogMode, build_host_binaries, discover_session_processes,
    discover_webots_processes, ensure_no_webots_running, follow_attached_logs, host_binary_path,
    spawn_tracked_process, stop_owned_process,
};
use super::stage::{StageWebotsProject, stage_webots_project};
use super::{
    CONTROLLER_PACKAGE, INTERACTIVE_CONNECT_RETRIES, INTERACTIVE_CONNECT_TIMEOUT_MS,
    JOYPAD_PACKAGE, LOCAL_HOST_ROUTER_ENDPOINT, RERUN_PROXY_PACKAGE, SUPERVISOR_PACKAGE,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoypadSelection {
    Auto,
    Id(String),
}

fn parse_joypad_selection(value: &str) -> Result<JoypadSelection, String> {
    let value = parse_trimmed_non_empty(value)?;
    if value.eq_ignore_ascii_case("auto") {
        return Ok(JoypadSelection::Auto);
    }

    Ok(JoypadSelection::Id(value))
}

fn host_command(app: &AppContext, package_name: &str) -> ProcessCommand {
    ProcessCommand::new(host_binary_path(app.project.root(), package_name))
}

fn append_local_robot_args(
    command: &mut ProcessCommand,
    bundle_dir: impl AsRef<std::path::Path>,
    identity: &RobotIdentity,
    router_endpoint: &str,
) {
    command
        .arg("--robot-config")
        .arg(bundle_dir.as_ref())
        .arg("--robot-id")
        .arg(&identity.robot_id)
        .arg("--robot-router-endpoint")
        .arg(router_endpoint)
        .arg("--robot-connect-timeout-ms")
        .arg(INTERACTIVE_CONNECT_TIMEOUT_MS.to_string())
        .arg("--robot-connect-retries")
        .arg(INTERACTIVE_CONNECT_RETRIES.to_string())
        .arg(format!("--xtask-session={}", identity.host_name()))
        .arg("--simulation")
        .arg("--robot-namespace")
        .arg(&identity.robot_namespace);
}

fn append_local_tool_args(
    command: &mut ProcessCommand,
    identity: &RobotIdentity,
    router_endpoint: &str,
) {
    command
        .arg("--router-endpoint")
        .arg(router_endpoint)
        .arg("--robot-connect-timeout-ms")
        .arg(INTERACTIVE_CONNECT_TIMEOUT_MS.to_string())
        .arg("--robot-connect-retries")
        .arg(INTERACTIVE_CONNECT_RETRIES.to_string())
        .arg(format!("--xtask-session={}", identity.host_name()))
        .arg("--robot-namespace")
        .arg(&identity.robot_namespace);
}
#[derive(Debug, Parser, Clone)]
pub struct Up {
    #[arg(help = "The robot model to simulate.")]
    pub robot_model: String,

    #[arg(help = "The world to load.")]
    pub world: String,

    #[arg(
        long = "local-driver",
        help = "Component instance ids whose drivers should start locally instead of in Docker. Can be specified multiple times."
    )]
    pub local_driver: Vec<String>,

    #[arg(
        long = "local-runtime",
        help = "Runtime service names to start locally instead of in Docker. Can be specified multiple times."
    )]
    pub local_runtime: Vec<String>,

    #[arg(
        long,
        help = "Start phoxal-rerun-proxy as a managed host-local process."
    )]
    pub with_rerun: bool,

    #[arg(
        long = "with-component",
        value_delimiter = ',',
        value_parser = parse_trimmed_non_empty,
        help = "Component instance ids or * globs to show in managed rerun-proxy. Can be specified multiple times."
    )]
    pub with_component: Vec<String>,

    #[arg(
        long = "with-joypad",
        value_name = "auto|id",
        num_args = 0..=1,
        default_missing_value = "auto",
        value_parser = parse_joypad_selection,
        help = "Start phoxal-joypad and select the first available controller automatically or wait for a specific controller id."
    )]
    pub with_joypad: Option<JoypadSelection>,

    #[arg(long, help = "Pass to local containers and local processes.")]
    pub rust_log: Option<String>,

    #[arg(
        long,
        help = "Launch Webots in realtime instead of paused.",
        default_value_t = true
    )]
    pub start_simulation: bool,

    #[arg(
        long,
        help = "Optional robot ID override used to derive the robot hostname.",
        value_parser = parse_trimmed_non_empty
    )]
    pub robot_id: Option<String>,

    #[arg(
        long,
        help = "Robot namespace used to compose the robot hostname.",
        default_value_t = String::from(DEFAULT_ROBOT_NAMESPACE),
        value_parser = parse_trimmed_non_empty
    )]
    pub robot_namespace: String,

    #[arg(long, help = "Do not use Docker cache when building images.")]
    pub no_cache: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebotsUiMode {
    Gui,
    Headless,
}

pub struct StartedSession {
    pub log_dir: PathBuf,
    attached_logs: Vec<AttachedLogFollowers>,
}

impl StartedSession {
    pub fn into_attached_logs(self) -> Vec<AttachedLogFollowers> {
        self.attached_logs
    }

    pub fn stop_log_followers(self) {
        for attached_log in self.attached_logs {
            attached_log.stop();
        }
    }
}

#[derive(Debug, Parser, Clone)]
pub struct Down {
    #[arg(help = "The robot model to simulate.")]
    pub robot_model: String,

    #[arg(
        long,
        help = "Force-stop owned processes with SIGKILL instead of graceful shutdown."
    )]
    pub force: bool,

    #[arg(
        long,
        help = "Optional robot ID override used to derive the robot hostname.",
        value_parser = parse_trimmed_non_empty
    )]
    pub robot_id: Option<String>,

    #[arg(
        long,
        help = "Robot namespace used to compose the robot hostname.",
        default_value_t = String::from(DEFAULT_ROBOT_NAMESPACE),
        value_parser = parse_trimmed_non_empty
    )]
    pub robot_namespace: String,
}

#[derive(Debug, Parser)]
pub struct Reset {
    #[arg(help = "The robot model to simulate.")]
    pub robot_model: String,

    #[arg(
        long,
        help = "Optional robot ID override used to derive the robot hostname.",
        value_parser = parse_trimmed_non_empty
    )]
    pub robot_id: Option<String>,

    #[arg(
        long,
        help = "Robot namespace used to compose the robot hostname.",
        default_value_t = String::from(DEFAULT_ROBOT_NAMESPACE),
        value_parser = parse_trimmed_non_empty
    )]
    pub robot_namespace: String,
}

impl Up {
    pub fn execute(&self, app: &AppContext) -> Result<()> {
        let started = self.start_session(app, WebotsUiMode::Gui)?;
        let log_dir = started.log_dir.clone();

        app.ui.info(format!(
            "Started Webots and docker compose for '{}'.",
            self.robot_model
        ));
        app.ui.info(format!(
            "Streaming session logs. Press Ctrl+C to detach and leave the session running. Logs are stored in {}",
            log_dir.display()
        ));
        follow_attached_logs(app, started.into_attached_logs(), &self.robot_model)?;

        Ok(())
    }

    pub fn start_session(&self, app: &AppContext, ui_mode: WebotsUiMode) -> Result<StartedSession> {
        let identity = RobotIdentity::new(
            self.robot_id
                .clone()
                .unwrap_or_else(|| self.robot_model.clone()),
            self.robot_namespace.clone(),
        );
        let host_name = identity.host_name();
        let rust_log = self.rust_log.clone().unwrap_or_else(|| "info".to_string());
        let bundle_dir = app.project.bundle_dir(&self.robot_model);
        let log_dir = app.project.dev_log_dir(&host_name);
        let external_router_endpoint = LOCAL_HOST_ROUTER_ENDPOINT.to_string();
        let owned_processes = discover_session_processes(&host_name)?;
        if !owned_processes.is_empty() {
            bail!(
                "simulation session '{}' is already running; use `cargo xtask webots restart {} {}` or `cargo xtask webots down {}` first",
                host_name,
                self.robot_model,
                self.world,
                self.robot_model,
            );
        }
        if !self.with_rerun && !self.with_component.is_empty() {
            bail!("--with-component requires --with-rerun");
        }

        ensure_no_webots_running()?;

        let robot = app.ui.step("Validate Robot", || {
            ValidatedRobot::load(app, &self.robot_model)
        })?;
        let configuration = robot.stage_bundle(app, None)?;

        let selection = ContainerSelectionParams {
            enable_component_drivers: false,
            enable_component_driver: Vec::new(),
            enable_component_driver_id: Vec::new(),
        }
        .resolve_for_local()
        .with_docker_excluded_runtimes(self.local_runtime.clone());
        let topology =
            plan_local_topology(&app.project, &self.robot_model, &configuration, &selection)?;
        let available_component_drivers =
            available_component_drivers(&app.project, &configuration)?
                .into_iter()
                .map(|driver| (driver.component_id, driver.component_type))
                .collect::<std::collections::BTreeMap<_, _>>();
        let local_driver_packages = self
            .local_driver
            .iter()
            .map(|component_id| {
                let component_type = available_component_drivers
                    .get(component_id)
                    .with_context(|| format!("unknown component driver '{}'", component_id))?;
                Ok((component_id.clone(), component_package_name(component_type)))
            })
            .collect::<Result<Vec<_>>>()?;
        build_host_binaries(
            app,
            std::iter::once(SUPERVISOR_PACKAGE.to_string())
                .chain(std::iter::once(CONTROLLER_PACKAGE.to_string()))
                .chain(self.with_rerun.then(|| RERUN_PROXY_PACKAGE.to_string()))
                .chain(
                    self.with_joypad
                        .as_ref()
                        .map(|_| JOYPAD_PACKAGE.to_string()),
                )
                .chain(
                    local_driver_packages
                        .iter()
                        .map(|(_, package)| package.clone()),
                )
                .chain(
                    self.local_runtime
                        .iter()
                        .map(|runtime_name| runtime_package_name(runtime_name)),
                )
                .collect(),
        )?;

        BuildImages {
            topology: topology.clone(),
            extra_images: Vec::new(),
            platform: BuildPlatform::local_default(),
            bundle_dir: bundle_dir.clone(),
            no_cache: self.no_cache,
        }
        .run(app)?;

        let compose_path = app.project.dev_compose_path(&host_name);
        let compose_content = GenerateCompose {
            topology: topology.clone(),
            mode: ComposeMode::Local {
                identity: identity.clone(),
                rust_log: rust_log.clone(),
            },
        }
        .run(app)?;
        if let Some(parent) = compose_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&compose_path, compose_content)?;

        let staged_world_path = stage_webots_project(StageWebotsProject {
            app,
            robot_model: &self.robot_model,
            world: &self.world,
            identity: &identity,
            external_router_endpoint: &external_router_endpoint,
            configuration: &configuration,
            model_structure: &robot.base_structure,
        })?;

        let mut attached_logs = Vec::new();
        let mut compose_up = ProcessCommand::new("docker");
        compose_up
            .arg("compose")
            .arg("-f")
            .arg(&compose_path)
            .arg("up")
            .arg("-d")
            .arg("--remove-orphans");
        let compose_up_title = format!("Starting docker compose stack {}", compose_path.display());
        if let Err(error) = app.ui.step(&compose_up_title, || {
            let status = app.ui.command_status(&mut compose_up)?;
            if !status.success() {
                bail!("docker compose up failed with status {status}");
            }
            Ok(())
        }) {
            let down = Down {
                robot_model: self.robot_model.clone(),
                force: true,
                robot_id: self.robot_id.clone(),
                robot_namespace: self.robot_namespace.clone(),
            };
            let _ = down.execute(app);
            return Err(error);
        }
        let startup = (|| -> Result<Vec<AttachedLogFollowers>> {
            fs::create_dir_all(&log_dir)?;

            for (component_id, package_name) in &local_driver_packages {
                let mut command = host_command(app, package_name);
                append_local_robot_args(
                    &mut command,
                    &bundle_dir,
                    &identity,
                    &external_router_endpoint,
                );
                command.arg("--component-id").arg(component_id);
                attached_logs.push(spawn_tracked_process(
                    app,
                    &log_dir,
                    &format!("driver-{component_id}"),
                    SpawnLogMode::TracingFile,
                    &mut command,
                )?);
            }

            for runtime_name in &self.local_runtime {
                let package_name = runtime_package_name(runtime_name);
                let mut command = host_command(app, &package_name);
                append_local_robot_args(
                    &mut command,
                    &bundle_dir,
                    &identity,
                    &external_router_endpoint,
                );
                attached_logs.push(spawn_tracked_process(
                    app,
                    &log_dir,
                    &format!("runtime-{runtime_name}"),
                    SpawnLogMode::TracingFile,
                    &mut command,
                )?);
            }

            if self.with_rerun {
                let mut rerun = host_command(app, RERUN_PROXY_PACKAGE);
                append_local_tool_args(&mut rerun, &identity, &external_router_endpoint);
                for component in &self.with_component {
                    rerun.arg("--with-component").arg(component);
                }
                attached_logs.push(spawn_tracked_process(
                    app,
                    &log_dir,
                    "rerun",
                    SpawnLogMode::TracingFile,
                    &mut rerun,
                )?);
            }

            if let Some(controller) = &self.with_joypad {
                let mut joypad = host_command(app, JOYPAD_PACKAGE);
                append_local_tool_args(&mut joypad, &identity, &external_router_endpoint);
                joypad
                    .arg("--simulation")
                    .arg("--controller")
                    .arg(match controller {
                        JoypadSelection::Auto => "auto",
                        JoypadSelection::Id(id) => id,
                    });
                attached_logs.push(spawn_tracked_process(
                    app,
                    &log_dir,
                    "joypad",
                    SpawnLogMode::TracingFile,
                    &mut joypad,
                )?);
            }

            let mut webots = ProcessCommand::new("webots");
            if ui_mode == WebotsUiMode::Headless {
                webots.arg("--batch");
            }
            webots
                .arg(if self.start_simulation {
                    "--mode=realtime"
                } else {
                    "--mode=pause"
                })
                .arg("--stdout")
                .arg("--stderr")
                .arg(&staged_world_path);
            attached_logs.push(spawn_tracked_process(
                app,
                &log_dir,
                "webots",
                SpawnLogMode::StdIoCapture,
                &mut webots,
            )?);

            Ok(attached_logs)
        })();

        let attached_logs = match startup {
            Ok(attached_logs) => attached_logs,
            Err(error) => {
                let down = Down {
                    robot_model: self.robot_model.clone(),
                    force: true,
                    robot_id: self.robot_id.clone(),
                    robot_namespace: self.robot_namespace.clone(),
                };
                let _ = down.execute(app);
                return Err(error);
            }
        };

        Ok(StartedSession {
            log_dir,
            attached_logs,
        })
    }
}

impl Down {
    pub fn execute(&self, app: &AppContext) -> Result<()> {
        let identity = RobotIdentity::new(
            self.robot_id
                .clone()
                .unwrap_or_else(|| self.robot_model.clone()),
            self.robot_namespace.clone(),
        );
        let host_name = identity.host_name();

        let owned_processes = discover_session_processes(&host_name)?;
        for process in &owned_processes {
            stop_owned_process(process, self.force, app);
        }
        let lingering_owned_processes = discover_session_processes(&host_name)?;
        for process in &lingering_owned_processes {
            app.ui.warn(format!(
                "Owned process {} (PID {}) is still running after shutdown request",
                process.name, process.pid
            ));
        }

        let webots_processes = discover_webots_processes()?;
        for webots_process in &webots_processes {
            stop_owned_process(webots_process, self.force, app);
        }
        let lingering_webots_processes = discover_webots_processes()?;
        for process in &lingering_webots_processes {
            app.ui.warn(format!(
                "Webots process {} (PID {}) is still running after shutdown request",
                process.name, process.pid
            ));
        }

        let compose_path = app.project.dev_compose_path(&host_name);
        if compose_path.exists() {
            let mut compose_down = ProcessCommand::new("docker");
            compose_down
                .arg("compose")
                .arg("-f")
                .arg(&compose_path)
                .arg("down")
                .arg("--remove-orphans");
            let compose_down_title =
                format!("Stopping docker compose stack {}", compose_path.display());
            app.ui.step(&compose_down_title, || {
                let status = app.ui.command_status(&mut compose_down)?;
                if !status.success() {
                    bail!("docker compose down failed with status {status}");
                }
                Ok(())
            })?;
        }

        if lingering_owned_processes.is_empty() && lingering_webots_processes.is_empty() {
            return Ok(());
        }

        bail!(
            "failed to stop {} owned process(es); rerun `cargo xtask webots down {} --force` to kill them directly",
            lingering_owned_processes.len() + lingering_webots_processes.len(),
            self.robot_model
        )
    }
}

/// Kill a single runtime's container mid-session (fault injection for failure-recovery
/// scenarios). The `restart: always` policy will auto-restart it.
pub fn kill_service(app: &AppContext, identity: &RobotIdentity, service: &str) -> Result<()> {
    let compose_path = app.project.dev_compose_path(&identity.host_name());
    let mut command = ProcessCommand::new("docker");
    command
        .arg("compose")
        .arg("-f")
        .arg(&compose_path)
        .arg("kill")
        .arg(service);
    let title = format!("Killing service '{service}' in {}", compose_path.display());
    app.ui.step(&title, || {
        let status = app.ui.command_status(&mut command)?;
        if !status.success() {
            bail!("docker compose kill {service} failed with status {status}");
        }
        Ok(())
    })
}

/// Restart a single runtime's container mid-session (recovery half of a failure-recovery
/// scenario). `docker compose start` re-launches the previously-killed container, which
/// reconnects to the bus and resumes heart-beating.
pub fn restart_service(app: &AppContext, identity: &RobotIdentity, service: &str) -> Result<()> {
    let compose_path = app.project.dev_compose_path(&identity.host_name());
    let mut command = ProcessCommand::new("docker");
    command
        .arg("compose")
        .arg("-f")
        .arg(&compose_path)
        .arg("start")
        .arg(service);
    let title = format!(
        "Restarting service '{service}' in {}",
        compose_path.display()
    );
    app.ui.step(&title, || {
        let status = app.ui.command_status(&mut command)?;
        if !status.success() {
            bail!("docker compose start {service} failed with status {status}");
        }
        Ok(())
    })
}

impl Reset {
    pub async fn execute(&self, app: &AppContext) -> Result<()> {
        let identity = RobotIdentity::new(
            self.robot_id
                .clone()
                .unwrap_or_else(|| self.robot_model.clone()),
            self.robot_namespace.clone(),
        );
        let external_router_endpoint = LOCAL_HOST_ROUTER_ENDPOINT.to_string();
        let builder = phoxal_bus::builder::Builder::new(external_router_endpoint)
            .with_prefix(identity.robot_namespace.clone());
        let bus = builder.connect().await?;

        use phoxal_bus::query::Retry;
        use phoxal_simulator_api::reset::Request as ResetRequest;

        let request = ResetRequest;
        let retry = Retry::new(1);

        let response = tokio::time::timeout(
            Duration::from_secs(10),
            phoxal_simulator_api::reset::request(&bus, &request, &retry),
        )
        .await
        .map_err(|_| anyhow!("timed out waiting for simulation reset response"))??;

        let response = response.ok_or_else(|| {
            anyhow!(
                "no simulation reset command responder available on '{}'",
                phoxal_simulator_api::reset::topic(&bus)
            )
        })?;

        app.ui.success(format!(
            "Simulation reset accepted. New epoch: {}",
            response.epoch
        ));

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::process::{
        matches_webots_command_line, matches_xtask_session, parse_process_line,
    };
    use super::*;

    // --- Strict matching (HostTool / HostRuntimeOrDriver) ---

    #[test]
    fn webots_match_plain_command() {
        assert!(matches_webots_command_line(
            "webots --mode=pause /path/to/world.wbt"
        ));
    }

    #[test]
    fn webots_match_xtask_fails() {
        assert!(!matches_webots_command_line(
            "target/debug/phoxal-cli webots up robot-v1 empty"
        ));
    }

    #[test]
    fn webots_match_cargo_xtask_fails() {
        assert!(!matches_webots_command_line(
            "cargo xtask webots up robot-v1 empty"
        ));
    }

    #[test]
    fn webots_match_macos_bundle_path() {
        assert!(matches_webots_command_line(
            "/Applications/Webots.app/MacOS/Webots --mode=realtime /world.wbt"
        ));
    }

    #[test]
    fn webots_match_unrelated_gui_fails() {
        assert!(!matches_webots_command_line(
            "/Applications/Photoshop.app/MacOS/Photoshop --document"
        ));
    }

    #[test]
    fn matches_xtask_session_marker() {
        assert!(matches_xtask_session(
            "/path/to/bin --xtask-session=webots-robot-v1 --robot-id=dev-001",
            "webots-robot-v1"
        ));
    }

    #[test]
    fn parse_process_line_extracts_pid_and_args() -> anyhow::Result<()> {
        let (pid, args) = parse_process_line(
            "35672 /Applications/Webots.app/Contents/MacOS/webots --mode=realtime",
        )
        .ok_or_else(|| anyhow::anyhow!("process line did not parse"))?;
        assert_eq!(pid, 35672);
        assert_eq!(
            args,
            "/Applications/Webots.app/Contents/MacOS/webots --mode=realtime"
        );
        Ok(())
    }

    #[test]
    fn with_joypad_without_value_defaults_to_auto() {
        let args = Up::parse_from(["up", "robot-v1", "SimpleWorld", "--with-joypad"]);

        assert_eq!(args.with_joypad, Some(JoypadSelection::Auto));
    }

    #[test]
    fn with_joypad_accepts_explicit_id() {
        let args = Up::parse_from([
            "up",
            "robot-v1",
            "SimpleWorld",
            "--with-joypad=2d931510-d99f-494a-8c67-87feb05e1594",
        ]);

        assert_eq!(
            args.with_joypad,
            Some(JoypadSelection::Id(
                "2d931510-d99f-494a-8c67-87feb05e1594".to_string()
            ))
        );
    }

    #[test]
    fn with_rerun_defaults_to_disabled() {
        let args = Up::parse_from(["up", "robot-v1", "SimpleWorld"]);

        assert!(!args.with_rerun);
    }

    #[test]
    fn with_rerun_enables_rerun_proxy() {
        let args = Up::parse_from(["up", "robot-v1", "SimpleWorld", "--with-rerun"]);

        assert!(args.with_rerun);
    }

    #[test]
    fn with_component_accepts_repeated_and_comma_separated_values() {
        let args = Up::parse_from([
            "up",
            "robot-v1",
            "SimpleWorld",
            "--with-component=*tof,front*",
            "--with-component=imu",
        ]);

        assert_eq!(
            args.with_component,
            vec!["*tof".to_string(), "front*".to_string(), "imu".to_string()]
        );
    }
}
