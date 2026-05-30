use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use clap::Args;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::AppContext;
use crate::catalog::CATALOG;
use crate::host_paths;
use crate::lockfile::{LOCKFILE_NAME, LOCKFILE_SCHEMA_VERSION, Lockfile};
use crate::releases::ReleasesSnapshot;
use crate::resolver::{
    ImagePin, ReleaseSource, ResolveOptions, ResolvedComponentSource, ResolvedRobot,
    resolve_with_release_source, resolve_with_releases,
};
use crate::shell;
use crate::world;

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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SimulateOptions {
    pub world: String,
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
    world_path: PathBuf,
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
            world: self.world.clone(),
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
            let project_root = app.project.root().to_path_buf();
            let plan = tokio::task::spawn_blocking(move || prepare(&project_root, options))
                .await
                .context("simulate dry-run worker failed")??;
            report_plan_only(&plan);
            Ok(plan)
        }
        SimulateMode::Live => {
            let project_root = app.project.root().to_path_buf();
            let resolve_options = options.clone();
            let resolved = tokio::task::spawn_blocking(move || {
                resolve_project(&project_root, resolve_options)
            })
            .await
            .context("simulate resolver worker failed")??;
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
    let resolved = resolve_project(project_start, options.clone())?;
    write_simulation_files(resolved, options, &BTreeMap::new(), SimulateMode::DryRun)
}

pub fn prepare_with_releases(
    project_start: &Path,
    options: SimulateOptions,
    releases: &ReleasesSnapshot,
) -> Result<SimulatePlan> {
    let resolved = resolve_project_with_releases(project_start, options.clone(), Some(releases))?;
    write_simulation_files(resolved, options, &BTreeMap::new(), SimulateMode::DryRun)
}

fn resolve_project(project_start: &Path, options: SimulateOptions) -> Result<ResolvedSimulation> {
    resolve_project_with_releases(project_start, options, None)
}

fn resolve_project_with_releases(
    project_start: &Path,
    options: SimulateOptions,
    releases: Option<&ReleasesSnapshot>,
) -> Result<ResolvedSimulation> {
    let robot_path = crate::resolver::discover_robot_yaml(project_start)
        .with_context(|| format!("failed to find robot.yaml from {}", project_start.display()))?;
    let project_root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?
        .to_path_buf();
    let world_path = world::resolve_world(&project_root, &options.world)?;
    let robot = crate::resolver::load_robot(&robot_path)?;
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
        let snapshot = releases_snapshot_from_lockfile(lockfile)?;
        let mut resolved = resolve_with_releases(
            &robot,
            &CATALOG,
            ResolveOptions {
                locked: true,
                allow_floating: true,
                resolve_external_artifacts: false,
            },
            &snapshot,
        )?;
        apply_lockfile(lockfile, &mut resolved)
            .with_context(|| format!("{} is stale", lock_path.display()))?;
        return Ok(ResolvedSimulation {
            robot_path,
            project_root,
            world_path,
            resolved,
            lockfile_written: None,
        });
    }

    let resolve_options = ResolveOptions {
        locked: false,
        allow_floating: true,
        resolve_external_artifacts: options.resolve_external_artifacts,
    };
    let resolved = if let Some(releases) = releases {
        resolve_with_releases(&robot, &CATALOG, resolve_options, releases)?
    } else {
        let cache_dir = host_paths::cache_dir()?;
        resolve_with_release_source(
            &robot,
            &CATALOG,
            resolve_options,
            ReleaseSource::CacheDir(&cache_dir),
        )?
    };
    if let Some(lockfile) = &lockfile {
        let mut locked_resolved = resolved.clone();
        if apply_lockfile(lockfile, &mut locked_resolved).is_ok() {
            return Ok(ResolvedSimulation {
                robot_path,
                project_root,
                world_path,
                resolved: locked_resolved,
                lockfile_written: None,
            });
        }
    }
    let lockfile_written = reconcile_lockfile(&project_root, &resolved, false)?;
    Ok(ResolvedSimulation {
        robot_path,
        project_root,
        world_path,
        resolved,
        lockfile_written,
    })
}

fn releases_snapshot_from_lockfile(lockfile: &Lockfile) -> Result<ReleasesSnapshot> {
    let resolved_version = lockfile
        .phoxal_runtimes
        .resolved
        .parse::<semver::Version>()?;
    Ok(ReleasesSnapshot {
        fetched_at: lockfile
            .phoxal_runtimes
            .releases_fetched_at
            .map(SystemTime::from)
            .unwrap_or(SystemTime::UNIX_EPOCH),
        versions: vec![resolved_version.to_string()],
    })
}

fn apply_lockfile(lockfile: &Lockfile, resolved: &mut ResolvedRobot) -> Result<()> {
    if !lockfile.has_current_schema() {
        bail!(
            "lockfile schema_version {} is stale; regenerate {} with schema_version {}",
            lockfile.schema_version,
            LOCKFILE_NAME,
            LOCKFILE_SCHEMA_VERSION
        );
    }
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
        if let Some((image_repo, image_digest)) = image.split_once('@') {
            // Reproducible content pin: repo@sha256:…
            runtime.image_repo = image_repo.to_string();
            runtime.pin = ImagePin::Digest(image_digest.to_string());
        } else {
            // Recovery tag ref: repo:version (the last colon separates the tag,
            // leaving an optional registry-port colon in the repo intact).
            let (image_repo, version) = image.rsplit_once(':').with_context(|| {
                format!(
                    "lockfile image '{image}' for runtime {} is neither a digest pin \
                     (repo@sha256:…) nor a tag ref (repo:version)",
                    runtime.name
                )
            })?;
            runtime.image_repo = image_repo.to_string();
            runtime.version = version.parse().with_context(|| {
                format!(
                    "lockfile image tag '{version}' for runtime {} is not a semver version",
                    runtime.name
                )
            })?;
            runtime.pin = ImagePin::Unpinned;
        }
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
        if !locked.repo.is_empty() {
            tool.repo = locked.repo.clone();
        }
        tool.asset = locked.asset.clone();
        if !locked.binary_name.is_empty() {
            tool.binary_name = locked.binary_name.clone();
        }
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

    let mut written_files = collect_files_under(&run_dir)?;
    let webots_dir = resolved.project_root.join(".phoxal").join("webots");
    if webots_dir.is_dir() {
        written_files.extend(collect_files_under(&webots_dir)?);
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

fn report_plan_only(plan: &SimulatePlan) {
    for path in &plan.written_files {
        println!("wrote {}", path.display());
    }
    println!(
        "runtime set: {} (requested {})",
        plan.resolved.runtime_set_version, plan.resolved.requested_runtime_set
    );
    println!(
        "platform runtimes ({}):",
        plan.resolved.platform_runtimes.len()
    );
    for runtime in &plan.resolved.platform_runtimes {
        println!(
            "  - {} -> {}:{}",
            runtime.name, runtime.image_repo, runtime.version
        );
    }
    println!("compose file: {}", plan.compose_path.display());
    println!("dry-run — no containers or processes started");
}

async fn execute_plan(plan: &SimulatePlan) -> Result<()> {
    bring_up_stack(&plan.compose_path)?;

    let mut processes = crate::process::SpawnedProcesses::new();
    if plan.native_tools.iter().any(|tool| tool == "rerun_proxy") {
        spawn_cached_tool(&plan.resolved, "rerun_proxy", &mut processes)?;
    }
    if plan.native_tools.iter().any(|tool| tool == "joypad") {
        spawn_cached_tool(&plan.resolved, "joypad", &mut processes)?;
    }
    spawn_webots(&plan.project_root, &plan.resolved, &mut processes)?;
    processes.write_state(&plan.state_path)?;

    tokio::signal::ctrl_c()
        .await
        .context("failed to listen for Ctrl-C")?;
    drop(processes);
    tear_down_stack(&plan.compose_path)?;
    Ok(())
}

fn pull_platform_images(app: &AppContext, resolved: &ResolvedRobot) -> Result<()> {
    for runtime in &resolved.platform_runtimes {
        // `deploy_ref` is a real digest pin or an honest tag ref — never a
        // fabricated `sha256:` — so this can't attempt to pull a fake digest.
        let image = runtime.deploy_ref();
        app.ui.info(format!("pulling {image}"));
        shell::run_status("docker", ["pull", image.as_str()], None).with_context(|| {
            if runtime.digest_pin().is_some() {
                format!("failed to pull pinned runtime image {image}")
            } else {
                format!(
                    "failed to pull runtime image {image} by tag. The phoxal/framework GHCR \
                     runtime images may not be published for this runtime set yet. Publish the \
                     runtime images, then run `phoxal-cli update --pin-digests` to pin real \
                     digests, or `phoxal-cli update --refresh-releases` to pick up a newer set."
                )
            }
        })?;
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
    let host_cache_dir = host_paths::cache_dir()?;
    for component in &resolved.components {
        if !component.has_driver {
            continue;
        }
        let driver_dir = match &component.source {
            ResolvedComponentSource::Path { path } => {
                resolve_project_path(project_root, path).join("driver")
            }
            ResolvedComponentSource::Git { commit, .. } => host_cache_dir
                .join("components")
                .join(format!("{}-{commit}", component.source_name))
                .join("driver"),
        };
        if driver_dir.is_dir() {
            shell::run_status("cargo", ["build", "--release"], Some(&driver_dir))?;
        }
    }
    Ok(())
}

fn bring_up_stack(compose_path: &Path) -> Result<()> {
    ensure_link_network()?;
    // phoxal-local-zenoh is a singleton; concurrent `phoxal simulate` runs are
    // unsupported because either teardown can stop the shared container. A
    // future `phoxal local up/down` command will expose explicit lifecycle.
    crate::local_zenoh::start_if_absent()?;
    compose_up(compose_path)
}

fn tear_down_stack(compose_path: &Path) -> Result<()> {
    compose_down(compose_path)?;
    crate::local_zenoh::stop()?;
    remove_link_network_best_effort();
    Ok(())
}

fn ensure_link_network() -> Result<()> {
    let network = crate::local_zenoh::LOCAL_ZENOH_NETWORK;
    let output = shell::run_output("docker", ["network", "inspect", network], None)
        .context("failed to inspect phoxal link network")?;
    if output.status.success() {
        return Ok(());
    }
    shell::run_status(
        "docker",
        ["network", "create", "--driver", "bridge", network],
        None,
    )
    .context("failed to create phoxal link network")
}

fn remove_link_network_best_effort() {
    let network = crate::local_zenoh::LOCAL_ZENOH_NETWORK;
    let Ok(output) = shell::run_output("docker", ["network", "rm", network], None) else {
        tracing::debug!("failed to run docker network rm {network}");
        return;
    };
    if output.status.success() {
        return;
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("has active endpoints") || stderr.contains("No such network") {
        tracing::debug!("leaving docker network {network}: {}", stderr.trim());
        return;
    }

    tracing::debug!(
        "`docker network rm {}` failed with status {}\nstdout:\n{}\nstderr:\n{}",
        network,
        output.status,
        String::from_utf8_lossy(&output.stdout),
        stderr
    );
}

fn compose_up(compose_path: &Path) -> Result<()> {
    let compose_arg = compose_path.to_string_lossy().to_string();
    shell::run_status(
        "docker",
        ["compose", "-f", compose_arg.as_str(), "up", "-d", "--wait"],
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

fn spawn_cached_tool(
    resolved: &ResolvedRobot,
    tool_name: &str,
    processes: &mut crate::process::SpawnedProcesses,
) -> Result<()> {
    let cache_dir = host_paths::cache_dir()?.join("tools").join(tool_name);
    if !cache_dir.is_dir() {
        bail!(
            "cached tool {tool_name} is missing under {}; run doctor --fix",
            cache_dir.display()
        );
    }
    let binary = newest_cached_binary(&cache_dir, tool_name)
        .with_context(|| format!("failed to find cached binary for {tool_name}"))?;
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
    let mut command = ProcessCommand::new("webots");
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

fn newest_cached_binary(cache_dir: &Path, tool_name: &str) -> Result<PathBuf> {
    let mut candidates = Vec::new();
    let normalized_tool_name = tool_name.replace('_', "-");
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
                .is_some_and(|name| name.replace('_', "-").contains(&normalized_tool_name))
            {
                candidates.push(path);
            }
        }
    }
    candidates.sort();
    candidates.pop().context("no cached binary found")
}

fn native_tool_labels(options: SimulateOptions) -> Vec<String> {
    // Webots owns simulator controller/supervisor processes via
    // `.phoxal/webots/controllers/<name>/<name>`, so state.yaml only records
    // processes phoxal-cli starts directly.
    let mut labels = Vec::new();
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
