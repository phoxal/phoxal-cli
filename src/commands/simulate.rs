use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;
use serde::Serialize;

use crate::AppContext;
use crate::catalog::CATALOG;
use crate::commands::MessageFormat;
use crate::resolver::{ResolveOptions, ResolvedRobot, RobotManifestExtras, resolve};
use crate::world;

const DEFAULT_BUS_CONNECT: &str = "tcp/127.0.0.1:7447";
const JOYPAD: &str = "joypad";

#[derive(Debug, Args)]
pub struct Simulate {
    #[arg(
        value_name = "WORLD",
        help = "World file or bare name (e.g. `default`, or `worlds/foo.wbt`). Resolved against <project>/worlds/<world>.wbt, then <project>/<world>, then ~/.phoxal/worlds/<world>.wbt."
    )]
    pub world: String,
    #[arg(
        long,
        help = "Resolve and write run artifacts without starting simulation processes."
    )]
    pub dry_run: bool,
    #[arg(long, help = "Launch joypad from the cached Phoxal tool binaries.")]
    pub joypad: bool,
    #[arg(
        long,
        help = "Refresh native service artifacts and host tools instead of reusing compatible cached artifacts."
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
    pub joypad: bool,
    pub pull: bool,
    pub message_format: MessageFormat,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SimulatePlan {
    pub robot_path: PathBuf,
    pub project_root: PathBuf,
    pub run_dir: PathBuf,
    pub state_path: PathBuf,
    pub bus_connect: String,
    pub written_files: Vec<PathBuf>,
    pub native_tools: Vec<String>,
    pub resolved: ResolvedRobot,
}

struct ResolvedSimulation {
    robot_path: PathBuf,
    project_root: PathBuf,
    world_path: PathBuf,
    resolved: ResolvedRobot,
    manifest_extras: RobotManifestExtras,
}

#[derive(Debug, Serialize)]
struct DryRunState {
    mode: &'static str,
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
            joypad: self.joypad,
            pull: self.pull,
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
            let _ = (app, options);
            Err(crate::native_pending::error(
                "native simulation launch (10)",
            ))
        }
    }
}

pub fn prepare(project_start: &Path, options: SimulateOptions) -> Result<SimulatePlan> {
    let resolved = resolve_project(project_start, options.clone(), SimulateMode::DryRun)?;
    write_simulation_files(resolved, options)
}

fn resolve_project(
    project_start: &Path,
    options: SimulateOptions,
    _mode: SimulateMode,
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

    // Always resolve live: simulate does not pin tool checksums, but it does
    // resolve git component commits so component drivers can be staged. A
    // path-only / official-only graph needs no network; a git component pinned
    // to a commit SHA resolves offline; a tag/branch ref is resolved live via
    // `git ls-remote` (with an actionable error if the network is unavailable).
    let resolved = resolve(
        &robot,
        &project_root,
        &CATALOG,
        ResolveOptions {
            resolve_external_artifacts: false,
            resolve_source_commits: true,
        },
    )?;
    Ok(ResolvedSimulation {
        robot_path,
        project_root,
        world_path,
        resolved,
        manifest_extras,
    })
}

fn write_simulation_files(
    resolved: ResolvedSimulation,
    options: SimulateOptions,
) -> Result<SimulatePlan> {
    let run_dir = resolved.project_root.join(".phoxal").join("run");
    crate::run_view::assemble(&resolved.project_root, &resolved.resolved, &run_dir)?;
    let bus_profile = resolved
        .manifest_extras
        .materialized_bus_profile(DEFAULT_BUS_CONNECT);
    crate::simulator_staging::stage_webots_artifacts(
        &resolved.project_root,
        &resolved.resolved,
        &run_dir,
        &resolved.world_path,
        &bus_profile.connect,
    )?;
    let native_tools = native_tool_labels(options);

    let state_path = resolved
        .project_root
        .join(".phoxal")
        .join("cache")
        .join("state.yaml");
    write_dry_run_state(&state_path, &native_tools, &resolved)?;

    let mut written_files = collect_files_under(&run_dir)?;
    let webots_dir = resolved.project_root.join(".phoxal").join("webots");
    if webots_dir.is_dir() {
        written_files.extend(collect_files_under(&webots_dir)?);
    }
    written_files.push(state_path.clone());
    written_files.sort();
    written_files.dedup();

    Ok(SimulatePlan {
        robot_path: resolved.robot_path,
        project_root: resolved.project_root,
        run_dir,
        state_path,
        bus_connect: bus_profile.connect,
        written_files,
        native_tools,
        resolved: resolved.resolved,
    })
}

fn write_dry_run_state(
    state_path: &Path,
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
        written_files: plan.written_files.clone(),
        platform_service_count: plan.resolved.platform_runtimes.len(),
        native_tools: plan.native_tools.clone(),
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
                "official services ({}):",
                plan.resolved.platform_runtimes.len()
            );
            for runtime in &plan.resolved.platform_runtimes {
                println!("  - {} -> {}", runtime.name, runtime.artifact_ref());
            }
            println!("dry-run - no simulation processes started");
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
    written_files: Vec<PathBuf>,
    platform_service_count: usize,
    native_tools: Vec<String>,
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

fn collect_files_under(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_files_under_inner(root, &mut files)?;
    Ok(files)
}

fn collect_files_under_inner(path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    if path.is_file() {
        files.push(path.to_path_buf());
        return Ok(());
    }
    if !path.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry.with_context(|| format!("failed to read entry in {}", path.display()))?;
        collect_files_under_inner(&entry.path(), files)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_resolve_path_only_project_needs_no_lock_or_network() -> Result<()> {
        // With no lockfile, a path-only / official-only project resolves live
        // with no network for either mode: there is nothing to look up remotely
        // (no git components), so resolution succeeds and writes no lock.
        let temp = tempfile::tempdir()?;
        write_robot_project(temp.path())?;

        let resolved = resolve_project(
            temp.path(),
            SimulateOptions {
                world: "test".to_string(),
                ..SimulateOptions::default()
            },
            SimulateMode::Live,
        )?;

        assert_eq!(resolved.resolved.api_version, "y2026_1");
        assert!(resolved.resolved.components.is_empty());
        Ok(())
    }

    #[test]
    fn dry_run_resolve_path_only_project_needs_no_lock_or_network() -> Result<()> {
        let temp = tempfile::tempdir()?;
        write_robot_project(temp.path())?;

        let resolved = resolve_project(
            temp.path(),
            SimulateOptions {
                world: "test".to_string(),
                ..SimulateOptions::default()
            },
            SimulateMode::DryRun,
        )?;

        assert_eq!(resolved.resolved.api_version, "y2026_1");
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

phoxal_participants:
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
