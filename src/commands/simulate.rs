use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;
use serde::Serialize;

use crate::AppContext;
use crate::commands::MessageFormat;
use crate::launch_plan::{DEFAULT_ROUTER_CONNECT, SITE_TOOL_JOYPAD, SITE_TOOL_ROUTER};
use crate::resolver::{ResolveOptions, ResolvedRobot, resolve};
use crate::world;

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
    #[arg(long, hide = true)]
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
    pub catalog_source: Option<String>,
    pub message_format: MessageFormat,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SimulatePlan {
    pub robot_path: PathBuf,
    pub project_root: PathBuf,
    pub world_path: PathBuf,
    pub bus_connect: String,
    pub native_tools: Vec<String>,
    pub resolved: ResolvedRobot,
}

struct ResolvedSimulation {
    robot_path: PathBuf,
    project_root: PathBuf,
    world_path: PathBuf,
    resolved: ResolvedRobot,
}

impl Simulate {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let options = SimulateOptions {
            world: self.world.clone(),
            joypad: self.joypad,
            pull: self.pull,
            catalog_source: app.catalog_source.clone(),
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
    Ok(SimulatePlan {
        robot_path: resolved.robot_path,
        project_root: resolved.project_root,
        world_path: resolved.world_path,
        bus_connect: DEFAULT_ROUTER_CONNECT.to_string(),
        native_tools: native_tool_labels(options),
        resolved: resolved.resolved,
    })
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
    let catalog = crate::catalog::load_catalog(crate::catalog::CatalogLoadOptions {
        refresh: options.pull,
        cli_source: options.catalog_source.clone(),
        robot_source: manifest_extras.catalog_source.as_ref().map(|source| {
            if source.is_absolute() {
                source.clone()
            } else {
                project_root.join(source)
            }
        }),
    })?;

    // Always resolve live: simulate does not pin tool checksums, but it does
    // resolve git component commits so component drivers can be staged. A
    // path-only / official-only graph needs no network; a git component pinned
    // to a commit SHA resolves offline; a tag/branch ref is resolved live via
    // `git ls-remote` (with an actionable error if the network is unavailable).
    let resolved = resolve(
        &robot,
        &project_root,
        catalog.as_ref(),
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
    })
}

fn report_plan_only(plan: &SimulatePlan, message_format: MessageFormat) -> Result<()> {
    let output = SimulateDryRunOutput {
        mode: "dry-run",
        target_generation: plan.resolved.target_generation.clone(),
        channel: plan.resolved.channel.to_string(),
        catalog_revision: plan.resolved.catalog_revision.clone(),
        world_path: plan.world_path.clone(),
        bus_connect: plan.bus_connect.clone(),
        platform_service_count: plan.resolved.platform_runtimes.len(),
        native_tools: plan.native_tools.clone(),
    };
    crate::commands::print_message(
        &output,
        || {
            println!(
                "target_generation: {} (channel {})",
                plan.resolved.target_generation, plan.resolved.channel
            );
            if let Some(revision) = &plan.resolved.catalog_revision {
                println!("catalog revision: {revision}");
            }
            println!(
                "official services ({}):",
                plan.resolved.platform_runtimes.len()
            );
            for runtime in &plan.resolved.platform_runtimes {
                println!("  - {} -> {}", runtime.name, runtime.artifact_ref());
            }
            println!("world: {}", plan.world_path.display());
            println!("router: {}", plan.bus_connect);
            println!("site tools:");
            for tool in &plan.native_tools {
                println!("  - {tool}");
            }
            println!("dry-run - no files written and no simulation processes started");
            Ok(())
        },
        message_format,
    )
}

#[derive(Debug, Serialize)]
struct SimulateDryRunOutput {
    mode: &'static str,
    target_generation: String,
    channel: String,
    catalog_revision: Option<String>,
    world_path: PathBuf,
    bus_connect: String,
    platform_service_count: usize,
    native_tools: Vec<String>,
}

fn native_tool_labels(options: SimulateOptions) -> Vec<String> {
    let _ = options;
    let mut labels = vec![SITE_TOOL_ROUTER.to_string(), SITE_TOOL_JOYPAD.to_string()];
    labels.push("webots".to_string());
    labels
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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

        assert_eq!(resolved.resolved.target_generation, "y2026_1");
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

        assert_eq!(resolved.resolved.target_generation, "y2026_1");
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

phoxal_artifacts:
  channel: stable
phoxal_participants: {}

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
