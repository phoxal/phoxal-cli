use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Args;
use phoxal_cli_core::AppContext;
use phoxal_cli_core::shell;
use sha2::{Digest, Sha256};
use tokio::time::sleep;

use crate::catalog::CATALOG;
use crate::lockfile::{LOCKFILE_NAME, Lockfile};
use crate::resolver::{ResolveOptions, ResolvedComponentSource, ResolvedRobot};

#[derive(Debug, Args)]
pub struct Simulate {
    #[arg(
        long,
        help = "Require phoxal.lock to exist and match recomputed resolution."
    )]
    pub locked: bool,
    #[arg(
        long,
        help = "Launch rerun-proxy from the cached Phoxal tool binaries."
    )]
    pub rerun_proxy: bool,
    #[arg(long, help = "Launch joypad from the cached Phoxal tool binaries.")]
    pub joypad: bool,
}

impl Simulate {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let robot_path = crate::resolver::discover_robot_yaml(app.project.root())?;
        let project_root = robot_path
            .parent()
            .context("robot.yaml did not have a parent directory")?;
        let robot = crate::resolver::load_robot(&robot_path)?;
        let resolved = crate::resolver::resolve(
            &robot,
            &CATALOG,
            ResolveOptions {
                locked: self.locked,
                allow_floating: true,
                resolve_external_artifacts: true,
            },
        )?;
        reconcile_lockfile(project_root, &resolved, self.locked)?;

        pull_platform_images(app, &resolved)?;
        let user_images = build_user_runtimes(project_root, &resolved)?;
        build_component_drivers(project_root, &resolved)?;

        let run_dir = project_root.join(".phoxal").join("run");
        crate::run_view::assemble(project_root, &resolved, &run_dir)?;
        let compose_path = run_dir.join("docker-compose.yml");
        fs::write(
            &compose_path,
            crate::compose::generate(&resolved, &CATALOG, &run_dir, &user_images)?,
        )
        .with_context(|| format!("failed to write {}", compose_path.display()))?;

        compose_up(&compose_path)?;
        wait_for_router().await?;

        let mut processes = crate::process::SpawnedProcesses::new();
        spawn_cached_tool(project_root, "simulator_webots_controller", &mut processes)?;
        spawn_cached_tool(project_root, "simulator_webots_supervisor", &mut processes)?;
        if self.rerun_proxy {
            spawn_cached_tool(project_root, "rerun_proxy", &mut processes)?;
        }
        if self.joypad {
            spawn_cached_tool(project_root, "joypad", &mut processes)?;
        }
        spawn_webots(project_root, &resolved, &mut processes)?;
        processes.write_state(&project_root.join(".phoxal/cache/state.yaml"))?;

        tokio::signal::ctrl_c()
            .await
            .context("failed to listen for Ctrl-C")?;
        drop(processes);
        compose_down(&compose_path)?;
        Ok(())
    }
}

fn reconcile_lockfile(project_root: &Path, resolved: &ResolvedRobot, locked: bool) -> Result<()> {
    let lock_path = project_root.join(LOCKFILE_NAME);
    let expected = Lockfile::from_resolved(resolved);
    if lock_path.is_file() {
        let actual = Lockfile::read(&lock_path)?;
        if actual == expected {
            return Ok(());
        }
        if locked {
            bail!("{} differs from recomputed resolution", lock_path.display());
        }
    } else if locked {
        bail!("{} is required by --locked", lock_path.display());
    }
    expected.write(&lock_path)
}

fn pull_platform_images(app: &AppContext, resolved: &ResolvedRobot) -> Result<()> {
    for runtime in &resolved.platform_runtimes {
        let image = runtime.pinned_image();
        app.ui.info(format!("pulling {image}"));
        shell::run_status("docker", ["pull", image.as_str()], None)?;
    }
    Ok(())
}

fn build_user_runtimes(
    project_root: &Path,
    resolved: &ResolvedRobot,
) -> Result<BTreeMap<String, String>> {
    let mut images = BTreeMap::new();
    for runtime in &resolved.user_runtimes {
        let runtime_dir = resolve_project_path(project_root, &runtime.path);
        let hash = hash_tree(&runtime_dir)?;
        let image = format!(
            "phoxal-local/{}/user-runtime/{}:{}",
            resolved.robot.identity.id, runtime.name, hash
        );
        shell::run_status(
            "docker",
            ["build", "-t", image.as_str(), "."],
            Some(&runtime_dir),
        )?;
        images.insert(runtime.name.clone(), image);
    }
    Ok(images)
}

fn build_component_drivers(project_root: &Path, resolved: &ResolvedRobot) -> Result<()> {
    for component in &resolved.components {
        if !component.has_driver {
            continue;
        }
        let driver_dir = match &component.source {
            ResolvedComponentSource::Path { path } => {
                resolve_project_path(project_root, path).join("driver")
            }
            ResolvedComponentSource::Git { commit, .. } => project_root
                .join(".phoxal/cache/components")
                .join(format!("{}-{commit}", component.source_name))
                .join("driver"),
        };
        if driver_dir.is_dir() {
            shell::run_status("cargo", ["build", "--release"], Some(&driver_dir))?;
        }
    }
    Ok(())
}

fn compose_up(compose_path: &Path) -> Result<()> {
    let compose_arg = compose_path.to_string_lossy().to_string();
    shell::run_status(
        "docker",
        ["compose", "-f", compose_arg.as_str(), "up", "-d"],
        None,
    )
}

fn compose_down(compose_path: &Path) -> Result<()> {
    let compose_arg = compose_path.to_string_lossy().to_string();
    shell::run_status(
        "docker",
        ["compose", "-f", compose_arg.as_str(), "down"],
        None,
    )
}

async fn wait_for_router() -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        if shell::run_status("nc", ["-z", "127.0.0.1", "7447"], None).is_ok() {
            return Ok(());
        }
        sleep(Duration::from_millis(250)).await;
    }
    bail!("router did not become ready on 127.0.0.1:7447 within 30s")
}

fn spawn_cached_tool(
    project_root: &Path,
    tool_name: &str,
    processes: &mut crate::process::SpawnedProcesses,
) -> Result<()> {
    let cache_dir = project_root.join(".phoxal/cache/tools").join(tool_name);
    if !cache_dir.is_dir() {
        bail!(
            "cached tool {tool_name} is missing under {}; run doctor --fix",
            cache_dir.display()
        );
    }
    let binary = newest_cached_binary(&cache_dir, tool_name)
        .with_context(|| format!("failed to find cached binary for {tool_name}"))?;
    let mut command = ProcessCommand::new(binary);
    command.env("PHOXAL_BUS_ROUTER", "tcp/127.0.0.1:7447");
    processes.spawn(tool_name, &mut command)
}

fn spawn_webots(
    project_root: &Path,
    resolved: &ResolvedRobot,
    processes: &mut crate::process::SpawnedProcesses,
) -> Result<()> {
    let world = resolve_project_path(project_root, &resolved.sim_world);
    let mut command = ProcessCommand::new("webots");
    command.arg(world);
    processes.spawn("webots", &mut command)
}

fn newest_cached_binary(cache_dir: &Path, tool_name: &str) -> Result<PathBuf> {
    let mut candidates = Vec::new();
    for version in fs::read_dir(cache_dir)
        .with_context(|| format!("failed to read {}", cache_dir.display()))?
    {
        let version = version?;
        let version_path = version.path();
        if !version_path.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&version_path)
            .with_context(|| format!("failed to read {}", version_path.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(tool_name))
            {
                candidates.push(path);
            }
        }
    }
    candidates.sort();
    candidates.pop().context("no cached binary found")
}

fn hash_tree(path: &Path) -> Result<String> {
    let mut files = Vec::new();
    collect_files(path, path, &mut files)?;
    files.sort();
    let mut hasher = Sha256::new();
    for file in files {
        hasher.update(file.to_string_lossy().as_bytes());
        hasher.update(fs::read(path.join(&file))?);
    }
    Ok(hex::encode(hasher.finalize())[..16].to_string())
}

fn collect_files(root: &Path, path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        if name == ".git" || name == "target" {
            continue;
        }
        if path.is_dir() {
            collect_files(root, &path, files)?;
        } else if path.is_file() {
            files.push(path.strip_prefix(root)?.to_path_buf());
        }
    }
    Ok(())
}

fn resolve_project_path(project_root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    }
}
