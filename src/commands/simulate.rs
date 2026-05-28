use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Args;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::time::sleep;

use crate::AppContext;
use crate::catalog::CATALOG;
use crate::lockfile::{LOCKFILE_NAME, Lockfile};
use crate::resolver::{ResolveOptions, ResolvedComponentSource, ResolvedRobot};
use crate::shell;

#[derive(Debug, Args)]
pub struct Simulate {
    #[arg(
        long,
        help = "Require phoxal.lock to exist and match recomputed resolution."
    )]
    pub locked: bool,
    #[arg(
        long,
        help = "Resolve and write run artifacts without starting containers or processes."
    )]
    pub dry_run: bool,
    #[arg(
        long,
        help = "Launch rerun-proxy from the cached Phoxal tool binaries."
    )]
    pub rerun_proxy: bool,
    #[arg(long, help = "Launch joypad from the cached Phoxal tool binaries.")]
    pub joypad: bool,
    #[arg(
        long,
        help = "Resolve image digests, component git commits, and tool asset hashes from upstream (requires Docker + network). Off by default during the pre-publish recovery period."
    )]
    pub pin_digests: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimulateMode {
    Live,
    DryRun,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SimulateOptions {
    pub locked: bool,
    pub rerun_proxy: bool,
    pub joypad: bool,
    pub resolve_external_artifacts: bool,
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
    resolved: ResolvedRobot,
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
            locked: self.locked,
            rerun_proxy: self.rerun_proxy,
            joypad: self.joypad,
            resolve_external_artifacts: self.pin_digests,
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
            let plan = prepare(app.project.root(), options)?;
            report_plan_only(&plan);
            Ok(plan)
        }
        SimulateMode::Live => {
            let resolved = resolve_project(app.project.root(), options)?;
            pull_platform_images(app, &resolved.resolved)?;
            let user_images = build_user_runtimes(&resolved.project_root, &resolved.resolved)?;
            build_component_drivers(&resolved.project_root, &resolved.resolved)?;
            let plan = write_simulation_files(resolved, options, &user_images, SimulateMode::Live)?;
            execute_plan(&plan).await?;
            Ok(plan)
        }
    }
}

pub fn prepare(project_start: &Path, options: SimulateOptions) -> Result<SimulatePlan> {
    let resolved = resolve_project(project_start, options)?;
    write_simulation_files(resolved, options, &BTreeMap::new(), SimulateMode::DryRun)
}

fn resolve_project(project_start: &Path, options: SimulateOptions) -> Result<ResolvedSimulation> {
    let robot_path = crate::resolver::discover_robot_yaml(project_start)
        .with_context(|| format!("failed to find robot.yaml from {}", project_start.display()))?;
    let project_root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?
        .to_path_buf();
    let robot = crate::resolver::load_robot(&robot_path)?;
    let lock_path = project_root.join(LOCKFILE_NAME);
    if lock_path.is_file() {
        let lockfile = Lockfile::read(&lock_path)?;
        let mut resolved = crate::resolver::resolve(
            &robot,
            &CATALOG,
            ResolveOptions {
                locked: options.locked,
                allow_floating: true,
                resolve_external_artifacts: false,
            },
        )?;
        match apply_lockfile(&lockfile, &mut resolved) {
            Ok(()) => {
                return Ok(ResolvedSimulation {
                    robot_path,
                    project_root,
                    resolved,
                    lockfile_written: None,
                });
            }
            Err(error) if options.locked => {
                return Err(error).with_context(|| format!("{} is stale", lock_path.display()));
            }
            Err(_) => {}
        }
    } else if options.locked {
        bail!("{} is required by --locked", lock_path.display());
    }

    let resolved = crate::resolver::resolve(
        &robot,
        &CATALOG,
        ResolveOptions {
            locked: false,
            allow_floating: true,
            resolve_external_artifacts: options.resolve_external_artifacts,
        },
    )?;
    let lockfile_written = reconcile_lockfile(&project_root, &resolved, false)?;
    Ok(ResolvedSimulation {
        robot_path,
        project_root,
        resolved,
        lockfile_written,
    })
}

fn apply_lockfile(lockfile: &Lockfile, resolved: &mut ResolvedRobot) -> Result<()> {
    if lockfile.phoxal_runtimes.requested != resolved.requested_runtime_set {
        bail!(
            "lockfile requested runtime-set selector '{}' does not match robot.yaml selector '{}'",
            lockfile.phoxal_runtimes.requested,
            resolved.requested_runtime_set
        );
    }
    resolved.runtime_set_version =
        lockfile.phoxal_runtimes.resolved.parse().with_context(|| {
            format!(
                "lockfile runtime-set version '{}' is not semver",
                lockfile.phoxal_runtimes.resolved
            )
        })?;

    let mut runtime_names = BTreeSet::new();
    for runtime in &mut resolved.platform_runtimes {
        runtime_names.insert(runtime.name.clone());
        let image = lockfile
            .phoxal_runtimes
            .images
            .get(&runtime.name)
            .with_context(|| format!("lockfile is missing image for runtime {}", runtime.name))?;
        let (image_repo, image_digest) = image.split_once('@').with_context(|| {
            format!("lockfile image for runtime {} is not pinned", runtime.name)
        })?;
        runtime.image_repo = image_repo.to_string();
        runtime.image_digest = image_digest.to_string();
    }
    for name in lockfile.phoxal_runtimes.images.keys() {
        if !runtime_names.contains(name) {
            bail!("lockfile has image for unknown runtime {name}");
        }
    }

    let mut component_sources = BTreeSet::new();
    for component in &mut resolved.components {
        let ResolvedComponentSource::Git { git, tag, commit } = &mut component.source else {
            continue;
        };
        component_sources.insert(component.source_name.clone());
        let locked = lockfile
            .components
            .get(&component.source_name)
            .with_context(|| {
                format!(
                    "lockfile is missing component source {}",
                    component.source_name
                )
            })?;
        let crate::lockfile::LockedComponentSource::Git {
            git: locked_git,
            tag: locked_tag,
        } = &locked.source;
        if locked_git != git || locked_tag != tag {
            bail!(
                "lockfile component source {} does not match robot.yaml",
                component.source_name
            );
        }
        *commit = locked.commit.clone();
    }
    for name in lockfile.components.keys() {
        if !component_sources.contains(name) {
            bail!("lockfile has component source {name} that robot.yaml no longer uses");
        }
    }

    let mut tool_names = BTreeSet::new();
    for tool in &mut resolved.tools {
        tool_names.insert(tool.name.clone());
        let locked = lockfile
            .tools
            .get(&tool.name)
            .with_context(|| format!("lockfile is missing tool {}", tool.name))?;
        if locked.requested != tool.requested {
            bail!("lockfile tool {} does not match robot.yaml", tool.name);
        }
        tool.resolved = locked.resolved.clone();
        tool.asset = locked.asset.clone();
        tool.sha256 = locked.sha256.clone();
    }
    for name in lockfile.tools.keys() {
        if !tool_names.contains(name) {
            bail!("lockfile has unknown tool {name}");
        }
    }

    Ok(())
}

fn reconcile_lockfile(
    project_root: &Path,
    resolved: &ResolvedRobot,
    locked: bool,
) -> Result<Option<PathBuf>> {
    let lock_path = project_root.join(LOCKFILE_NAME);
    let expected = Lockfile::from_resolved(resolved);
    if lock_path.is_file() {
        let actual = Lockfile::read(&lock_path)?;
        if actual.is_drift_free(resolved) {
            return Ok(None);
        }
        if locked {
            bail!("{} differs from recomputed resolution", lock_path.display());
        }
    } else if locked {
        bail!("{} is required by --locked", lock_path.display());
    }
    expected.write(&lock_path)?;
    Ok(Some(lock_path))
}

fn write_simulation_files(
    resolved: ResolvedSimulation,
    options: SimulateOptions,
    user_images: &BTreeMap<String, String>,
    mode: SimulateMode,
) -> Result<SimulatePlan> {
    let run_dir = resolved.project_root.join(".phoxal").join("run");
    crate::run_view::assemble(&resolved.project_root, &resolved.resolved, &run_dir)?;
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
        )?,
    )
    .with_context(|| format!("failed to write {}", compose_path.display()))?;

    let state_path = resolved.project_root.join(".phoxal/cache/state.yaml");
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

    let mut written_files = collect_files_under(&run_dir)?;
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
    let world = resolve_project_path(&resolved.project_root, &resolved.resolved.sim_world);
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

fn report_plan_only(plan: &SimulatePlan) {
    for path in &plan.written_files {
        println!("wrote {}", path.display());
    }
    println!("compose file: {}", plan.compose_path.display());
    println!("dry-run — no containers or processes started");
}

async fn execute_plan(plan: &SimulatePlan) -> Result<()> {
    compose_up(&plan.compose_path)?;
    wait_for_router().await?;

    let mut processes = crate::process::SpawnedProcesses::new();
    spawn_cached_tool(
        &plan.project_root,
        "simulator_webots_controller",
        &mut processes,
    )?;
    spawn_cached_tool(
        &plan.project_root,
        "simulator_webots_supervisor",
        &mut processes,
    )?;
    if plan.native_tools.iter().any(|tool| tool == "rerun_proxy") {
        spawn_cached_tool(&plan.project_root, "rerun_proxy", &mut processes)?;
    }
    if plan.native_tools.iter().any(|tool| tool == "joypad") {
        spawn_cached_tool(&plan.project_root, "joypad", &mut processes)?;
    }
    spawn_webots(&plan.project_root, &plan.resolved, &mut processes)?;
    processes.write_state(&plan.state_path)?;

    tokio::signal::ctrl_c()
        .await
        .context("failed to listen for Ctrl-C")?;
    drop(processes);
    compose_down(&plan.compose_path)?;
    Ok(())
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

fn native_tool_labels(options: SimulateOptions) -> Vec<String> {
    let mut labels = vec![
        "simulator_webots_controller".to_string(),
        "simulator_webots_supervisor".to_string(),
    ];
    if options.rerun_proxy {
        labels.push("rerun_proxy".to_string());
    }
    if options.joypad {
        labels.push("joypad".to_string());
    }
    labels.push("webots".to_string());
    labels
}

fn compose_service_names(resolved: &ResolvedRobot) -> Vec<String> {
    let mut services = resolved
        .platform_runtimes
        .iter()
        .map(|runtime| runtime.name.clone())
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

fn collect_files_under(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_absolute_files(root, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_absolute_files(path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_absolute_files(&path, files)?;
        } else if path.is_file() {
            files.push(path);
        }
    }
    Ok(())
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
