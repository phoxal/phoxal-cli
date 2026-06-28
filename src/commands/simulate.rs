use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use anyhow::{Context, Result, bail};
use clap::Args;
use serde::Serialize;

use crate::AppContext;
use crate::catalog::CATALOG;
use crate::commands::MessageFormat;
use crate::compose::LaunchClock;
use crate::lockfile::{LOCKFILE_NAME, Lockfile};
use crate::resolver::{ResolveOptions, ResolvedRobot, RobotManifestExtras, resolve};
use crate::world;

const SIMULATOR_WEBOTS_CONTROLLER: &str = "simulator_webots_controller";
const SIMULATOR_WEBOTS_SUPERVISOR: &str = "simulator_webots_supervisor";
const JOYPAD: &str = "joypad";

#[derive(Debug, Args)]
#[command(
    after_help = "Lifecycle note:\n  phoxal simulate starts the singleton phoxal-local-zenoh container and joins it to the phoxal-link network.\n  Running two simulations concurrently is unsupported: whichever run tears down first will stop that shared container.\n  A future phoxal local up/down command will provide explicit lifecycle control."
)]
pub struct Simulate {
    #[arg(
        value_name = "WORLD",
        help = "World file or bare name (e.g. `default`, or `worlds/foo.wbt`). Resolved against <project>/worlds/<world>.wbt, then <project>/<world>, then ~/.phoxal/worlds/<world>.wbt."
    )]
    pub world: String,
    #[arg(
        long,
        help = "Require phoxal.sources.lock to exist and match recomputed resolution."
    )]
    pub locked: bool,
    #[arg(
        long,
        help = "Resolve and write run artifacts without starting containers or processes."
    )]
    pub dry_run: bool,
    #[arg(
        long,
        help = "Refresh phoxal.sources.lock when it is missing or stale (simulate's only way to mutate the lock)."
    )]
    pub update_lock: bool,
    #[arg(long, help = "Launch joypad from the cached Phoxal tool binaries.")]
    pub joypad: bool,
    #[arg(
        long,
        help = "Resolve image digests, component git commits, and tool asset hashes from upstream (requires Docker + network). Off by default during the pre-publish recovery period."
    )]
    pub pin_digests: bool,
    #[arg(
        long,
        help = "Refresh official runtime images and host tools instead of reusing compatible cached artifacts."
    )]
    pub pull: bool,
    #[arg(long, value_enum, default_value_t = MessageFormat::Human)]
    pub message_format: MessageFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimulateMode {
    Live,
    DryRun,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SimulateOptions {
    pub world: String,
    pub locked: bool,
    pub update_lock: bool,
    pub joypad: bool,
    pub pull: bool,
    pub resolve_external_artifacts: bool,
    pub message_format: MessageFormat,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SimulatePlan {
    pub robot_path: PathBuf,
    pub project_root: PathBuf,
    pub run_dir: PathBuf,
    pub compose_path: PathBuf,
    pub state_path: PathBuf,
    pub lockfile_written: Option<PathBuf>,
    pub written_files: Vec<PathBuf>,
    pub native_tools: Vec<String>,
    pub compose_services: Vec<String>,
    pub resolved: ResolvedRobot,
}

struct ResolvedSimulation {
    robot_path: PathBuf,
    project_root: PathBuf,
    world_path: PathBuf,
    resolved: ResolvedRobot,
    manifest_extras: RobotManifestExtras,
    lockfile_written: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct DryRunState {
    mode: &'static str,
    compose_file: PathBuf,
    services: Vec<String>,
    processes: Vec<DryRunProcess>,
}

#[derive(Debug, Serialize)]
struct DryRunProcess {
    label: String,
    command: String,
}

impl Simulate {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let options = SimulateOptions {
            world: self.world.clone(),
            locked: self.locked,
            update_lock: self.update_lock,
            joypad: self.joypad,
            pull: self.pull,
            resolve_external_artifacts: self.pin_digests,
            message_format: self.message_format,
        };
        let mode = if self.dry_run {
            SimulateMode::DryRun
        } else {
            SimulateMode::Live
        };
        run(app, options, mode).await.map(|_| ())
    }
}

pub async fn run(
    app: &AppContext,
    options: SimulateOptions,
    mode: SimulateMode,
) -> Result<SimulatePlan> {
    match mode {
        SimulateMode::DryRun => {
            let project_root = app.project.root().to_path_buf();
            let message_format = options.message_format;
            let plan = tokio::task::spawn_blocking(move || prepare(&project_root, options))
                .await
                .context("simulate dry-run worker failed")??;
            report_plan_only(&plan, message_format)?;
            Ok(plan)
        }
        SimulateMode::Live => {
            // The Webots controller/supervisor are host-native binaries that
            // phoxal/framework publishes only for x86_64 Linux and arm64 macOS.
            // Reject any other host up front (Intel macOS, Linux ARM, Windows, …)
            // rather than letting provisioning fail later with an opaque "release
            // asset not found".
            let host_target = crate::resolver::host_target_triple();
            const SIMULATOR_HOST_TARGETS: [&str; 2] =
                ["x86_64-unknown-linux-gnu", "aarch64-apple-darwin"];
            if !SIMULATOR_HOST_TARGETS.contains(&host_target.as_str()) {
                anyhow::bail!(
                    "phoxal-cli simulate is not supported on host target {host_target}: \
                     the Webots simulator binaries are published only for {}",
                    SIMULATOR_HOST_TARGETS.join(" and ")
                );
            }
            crate::host_doctor::preflight()?;
            let project_root = app.project.root().to_path_buf();
            let resolve_options = options.clone();
            let resolved = tokio::task::spawn_blocking(move || {
                resolve_project(&project_root, resolve_options, SimulateMode::Live)
            })
            .await
            .context("simulate resolver worker failed")??;
            crate::local_build::ensure_platform_images(app, &resolved.resolved, options.pull)?;
            crate::tool_provisioning::ensure_tool_binaries_with_mode(
                &app.ui,
                &resolved.resolved,
                requested_tool_names(&options),
                crate::tool_provisioning::ProvisioningMode::from_pull(options.pull),
            )?;
            let user_images = crate::local_build::build_user_runtimes(
                &resolved.project_root,
                &resolved.resolved,
            )?;
            crate::local_build::build_component_drivers(
                &resolved.project_root,
                &resolved.resolved,
            )?;
            let plan = write_simulation_files(resolved, options, &user_images, SimulateMode::Live)?;
            execute_plan(&plan).await?;
            Ok(plan)
        }
    }
}

pub fn prepare(project_start: &Path, options: SimulateOptions) -> Result<SimulatePlan> {
    let resolved = resolve_project(project_start, options.clone(), SimulateMode::DryRun)?;
    write_simulation_files(resolved, options, &BTreeMap::new(), SimulateMode::DryRun)
}

fn resolve_project(
    project_start: &Path,
    options: SimulateOptions,
    mode: SimulateMode,
) -> Result<ResolvedSimulation> {
    let robot_path = crate::resolver::discover_robot_yaml(project_start)
        .with_context(|| format!("failed to find robot.yaml from {}", project_start.display()))?;
    let project_root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?
        .to_path_buf();
    let world_path = world::resolve_world(&project_root, &options.world)?;
    let loaded = crate::resolver::load_robot_with_extras(&robot_path)?;
    let robot = loaded.robot;
    let manifest_extras = loaded.extras;
    let lock_path = project_root.join(LOCKFILE_NAME);
    let lockfile = if lock_path.is_file() {
        Some(Lockfile::read(&lock_path)?)
    } else {
        None
    };
    if options.locked {
        let lockfile = lockfile
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("{} is required by --locked", lock_path.display()))?;
        let mut resolved = resolve(
            &robot,
            &project_root,
            &CATALOG,
            ResolveOptions {
                locked: true,
                resolve_external_artifacts: false,
            },
        )?;
        crate::lockfile::apply_lockfile(lockfile, &mut resolved)
            .with_context(|| format!("{} is stale", lock_path.display()))?;
        return Ok(ResolvedSimulation {
            robot_path,
            project_root,
            world_path,
            resolved,
            manifest_extras,
            lockfile_written: None,
        });
    }

    let resolve_options = ResolveOptions {
        locked: false,
        resolve_external_artifacts: options.resolve_external_artifacts,
    };
    let resolved = resolve(&robot, &project_root, &CATALOG, resolve_options)?;
    if let Some(lockfile) = &lockfile {
        let mut locked_resolved = resolved.clone();
        if crate::lockfile::apply_lockfile(lockfile, &mut locked_resolved).is_ok() {
            return Ok(ResolvedSimulation {
                robot_path,
                project_root,
                world_path,
                resolved: locked_resolved,
                manifest_extras,
                lockfile_written: None,
            });
        }
    }
    let lockfile_written = match mode {
        SimulateMode::DryRun => None,
        SimulateMode::Live if options.update_lock => {
            crate::lockfile::reconcile_lockfile(&project_root, &resolved, false)?
        }
        SimulateMode::Live => bail!(
            "{LOCKFILE_NAME} is missing or stale; run `phoxal-cli update` to refresh it \
             (recommended), or `phoxal-cli simulate --update-lock` to refresh inline, \
             or commit the lock and use `--locked` to require it as-is."
        ),
    };
    Ok(ResolvedSimulation {
        robot_path,
        project_root,
        world_path,
        resolved,
        manifest_extras,
        lockfile_written,
    })
}

fn write_simulation_files(
    resolved: ResolvedSimulation,
    options: SimulateOptions,
    user_images: &BTreeMap<String, String>,
    mode: SimulateMode,
) -> Result<SimulatePlan> {
    let run_dir = resolved.project_root.join(".phoxal").join("run");
    crate::run_view::assemble(&resolved.project_root, &resolved.resolved, &run_dir)?;
    crate::simulator_staging::stage_webots_artifacts(
        &resolved.project_root,
        &resolved.resolved,
        &run_dir,
        &resolved.world_path,
    )?;
    let native_tools = native_tool_labels(options);
    let compose_path = run_dir.join("docker-compose.yml");
    fs::write(
        &compose_path,
        crate::compose::generate(
            &resolved.resolved,
            &CATALOG,
            &run_dir,
            user_images,
            &native_tools,
            &resolved.manifest_extras,
            LaunchClock::Simulation,
        )?,
    )
    .with_context(|| format!("failed to write {}", compose_path.display()))?;

    let state_path = resolved
        .project_root
        .join(".phoxal")
        .join("cache")
        .join("state.yaml");
    let compose_services = compose_service_names(&resolved.resolved);
    if mode == SimulateMode::DryRun {
        write_dry_run_state(
            &state_path,
            &compose_path,
            &compose_services,
            &native_tools,
            &resolved,
        )?;
    }

    let mut written_files = crate::local_build::collect_files_under(&run_dir)?;
    let webots_dir = resolved.project_root.join(".phoxal").join("webots");
    if webots_dir.is_dir() {
        written_files.extend(crate::local_build::collect_files_under(&webots_dir)?);
    }
    if mode == SimulateMode::DryRun {
        written_files.push(state_path.clone());
    }
    if let Some(lockfile_path) = &resolved.lockfile_written {
        written_files.push(lockfile_path.clone());
    }
    written_files.sort();
    written_files.dedup();

    Ok(SimulatePlan {
        robot_path: resolved.robot_path,
        project_root: resolved.project_root,
        run_dir,
        compose_path,
        state_path,
        lockfile_written: resolved.lockfile_written,
        written_files,
        native_tools,
        compose_services,
        resolved: resolved.resolved,
    })
}

fn write_dry_run_state(
    state_path: &Path,
    compose_path: &Path,
    compose_services: &[String],
    native_tools: &[String],
    resolved: &ResolvedSimulation,
) -> Result<()> {
    if let Some(parent) = state_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let world = staged_webots_world_path(&resolved.project_root);
    let processes = native_tools
        .iter()
        .map(|label| DryRunProcess {
            label: label.clone(),
            command: match label.as_str() {
                "webots" => format!("webots {}", world.display()),
                other => format!("cached-tool {other}"),
            },
        })
        .collect();
    let state = DryRunState {
        mode: "dry-run",
        compose_file: compose_path.to_path_buf(),
        services: compose_services.to_vec(),
        processes,
    };
    fs::write(state_path, serde_yaml::to_string(&state)?)
        .with_context(|| format!("failed to write {}", state_path.display()))
}

fn report_plan_only(plan: &SimulatePlan, message_format: MessageFormat) -> Result<()> {
    let output = SimulateDryRunOutput {
        mode: "dry-run",
        api_version: plan.resolved.api_version.clone(),
        channel: plan.resolved.channel.to_string(),
        compose_file: plan.compose_path.clone(),
        written_files: plan.written_files.clone(),
        platform_runtime_count: plan.resolved.platform_runtimes.len(),
        native_tools: plan.native_tools.clone(),
        compose_services: plan.compose_services.clone(),
    };
    crate::commands::print_message(
        &output,
        || {
            for path in &plan.written_files {
                println!("wrote {}", path.display());
            }
            println!(
                "api_version: {} (channel {})",
                plan.resolved.api_version, plan.resolved.channel
            );
            println!(
                "platform runtimes ({}):",
                plan.resolved.platform_runtimes.len()
            );
            for runtime in &plan.resolved.platform_runtimes {
                println!("  - {} -> {}", runtime.name, runtime.tag_ref());
            }
            println!("compose file: {}", plan.compose_path.display());
            println!("dry-run - no containers or processes started");
            Ok(())
        },
        message_format,
    )
}

#[derive(Debug, Serialize)]
struct SimulateDryRunOutput {
    mode: &'static str,
    api_version: String,
    channel: String,
    compose_file: PathBuf,
    written_files: Vec<PathBuf>,
    platform_runtime_count: usize,
    native_tools: Vec<String>,
    compose_services: Vec<String>,
}

async fn execute_plan(plan: &SimulatePlan) -> Result<()> {
    crate::docker_stack::bring_up_stack(&plan.compose_path)?;

    let mut processes = crate::process::SpawnedProcesses::new();
    if plan.native_tools.iter().any(|tool| tool == JOYPAD) {
        spawn_cached_tool(&plan.resolved, JOYPAD, &mut processes)?;
    }
    spawn_webots(&plan.project_root, &plan.resolved, &mut processes)?;
    processes.write_state(&plan.state_path)?;

    tokio::signal::ctrl_c()
        .await
        .context("failed to listen for Ctrl-C")?;
    drop(processes);
    crate::docker_stack::tear_down_stack(&plan.compose_path)?;
    Ok(())
}

fn spawn_cached_tool(
    resolved: &ResolvedRobot,
    tool_name: &str,
    processes: &mut crate::process::SpawnedProcesses,
) -> Result<()> {
    let tool = resolved
        .tools
        .iter()
        .find(|tool| tool.name == tool_name)
        .with_context(|| {
            format!("resolved tool {tool_name} is missing; cannot spawn requested native tool")
        })?;
    let binary =
        crate::simulator_staging::cached_tool_path(&tool.name, &tool.resolved, &tool.binary_name)?;
    if !binary.is_file() {
        bail!(
            "cached tool {} v{} is missing at {}; it has not been provisioned",
            tool.name,
            tool.resolved,
            binary.display()
        );
    }
    let mut command = ProcessCommand::new(binary);
    command.env("ROBOT_ROUTER_ENDPOINT", "tcp/127.0.0.1:7447");
    command.env("ROBOT_ID", &resolved.robot.identity.id);
    command.env("ROBOT_NAMESPACE", &resolved.robot.identity.namespace);
    processes.spawn(tool_name, &mut command)
}

fn spawn_webots(
    project_root: &Path,
    _resolved: &ResolvedRobot,
    processes: &mut crate::process::SpawnedProcesses,
) -> Result<()> {
    let world = staged_webots_world_path(project_root);
    let mut command = ProcessCommand::new(crate::host_doctor::webots_executable_path()?);
    command.arg(world);
    processes.spawn("webots", &mut command)
}

fn staged_webots_world_path(project_root: &Path) -> PathBuf {
    project_root
        .join(".phoxal")
        .join("webots")
        .join("worlds")
        .join("default.wbt")
}

fn native_tool_labels(options: SimulateOptions) -> Vec<String> {
    // Webots owns simulator controller/supervisor processes via
    // `.phoxal/webots/controllers/<name>/<name>`, so state.yaml only records
    // processes phoxal-cli starts directly.
    let mut labels = Vec::new();
    if options.joypad {
        labels.push(JOYPAD.to_string());
    }
    labels.push("webots".to_string());
    labels
}

fn requested_tool_names(options: &SimulateOptions) -> Vec<&'static str> {
    let mut tools = vec![SIMULATOR_WEBOTS_CONTROLLER, SIMULATOR_WEBOTS_SUPERVISOR];
    if options.joypad {
        tools.push(JOYPAD);
    }
    tools
}

fn compose_service_names(resolved: &ResolvedRobot) -> Vec<String> {
    let mut services = std::iter::once("router".to_string())
        .chain(
            resolved
                .platform_runtimes
                .iter()
                .map(|runtime| runtime.name.clone()),
        )
        .chain(
            resolved
                .user_runtimes
                .iter()
                .map(|runtime| format!("user-{}", runtime.name)),
        )
        .collect::<Vec<_>>();
    services.sort();
    services
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_resolve_missing_lock_requires_update_or_explicit_update_lock() -> Result<()> {
        let temp = tempfile::tempdir()?;
        write_robot_project(temp.path())?;

        let result = resolve_project(
            temp.path(),
            SimulateOptions {
                world: "test".to_string(),
                update_lock: false,
                resolve_external_artifacts: false,
                ..SimulateOptions::default()
            },
            SimulateMode::Live,
        );
        let error = match result {
            Ok(_) => bail!(
                "live resolve unexpectedly succeeded without {}",
                LOCKFILE_NAME
            ),
            Err(error) => error,
        };
        let message = format!("{error:#}");

        assert!(
            message.contains("phoxal-cli update"),
            "expected actionable update guidance, got: {message}"
        );
        assert!(!temp.path().join(LOCKFILE_NAME).exists());

        Ok(())
    }

    fn write_robot_project(root: &Path) -> Result<()> {
        fs::write(root.join("robot.yaml"), minimal_robot_yaml())?;
        fs::write(
            root.join("structure.urdf"),
            r#"<robot name="testbot"><link name="base_footprint"/><link name="base_link"/><joint name="base_joint" type="fixed"><parent link="base_footprint"/><child link="base_link"/></joint></robot>"#,
        )?;
        fs::create_dir_all(root.join("worlds"))?;
        fs::write(
            root.join("worlds/test.wbt"),
            "#VRML_SIM R2023b utf8\n\nWorldInfo {\n}\n",
        )?;
        Ok(())
    }

    fn minimal_robot_yaml() -> &'static str {
        r#"schema: v0
api_version: y2026_1

identity:
  id: testbot
  namespace: test

structure: structure.urdf

phoxal_runtimes:
  channel: stable

motion:
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5

components:
  sources: {}
  instances: {}
"#
    }
}
