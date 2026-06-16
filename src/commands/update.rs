use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;

use crate::AppContext;
use crate::catalog::CATALOG;
use crate::host_paths;
use crate::lockfile::{LOCKFILE_NAME, Lockfile};
use crate::releases::ReleasesSnapshot;
use crate::resolver::{
    ReleaseSource, ResolveOptions, discover_robot_yaml, load_robot, resolve_with_release_source,
    resolve_with_releases,
};

#[derive(Debug, Args)]
pub struct Update {
    /// Resolve image digests, component git commits, and tool asset hashes
    /// from their upstream sources (requires Docker + network). Off by
    /// default during the pre-publish recovery period — see
    /// `.plans/framework-restructure-installable-cli/` for the rollout.
    #[arg(long)]
    pub pin_digests: bool,
    #[arg(long, help = "Force-refresh the cached phoxal/framework release list.")]
    pub refresh_releases: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UpdateOptions {
    pub resolve_external_artifacts: bool,
    pub refresh_releases: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateSummary {
    pub lockfile_path: PathBuf,
    pub platform_runtime_count: usize,
    pub component_count: usize,
    pub tool_count: usize,
}

impl Update {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let project_root = app.project.root().to_path_buf();
        let options = UpdateOptions {
            resolve_external_artifacts: self.pin_digests,
            refresh_releases: self.refresh_releases,
        };
        let summary = tokio::task::spawn_blocking(move || run(&project_root, options))
            .await
            .context("update worker failed")??;
        println!(
            "locked {} platform runtimes, {} components, {} tools in {}",
            summary.platform_runtime_count,
            summary.component_count,
            summary.tool_count,
            summary.lockfile_path.display()
        );
        Ok(())
    }
}

pub fn run(project_start: &Path, options: UpdateOptions) -> Result<UpdateSummary> {
    run_with_release_source(project_start, options, None)
}

pub fn run_with_releases(
    project_start: &Path,
    options: UpdateOptions,
    releases: &ReleasesSnapshot,
) -> Result<UpdateSummary> {
    run_with_release_source(project_start, options, Some(releases))
}

fn run_with_release_source(
    project_start: &Path,
    options: UpdateOptions,
    releases: Option<&ReleasesSnapshot>,
) -> Result<UpdateSummary> {
    let robot_path = discover_robot_yaml(project_start)
        .with_context(|| format!("failed to find robot.yaml from {}", project_start.display()))?;
    let project_root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?;
    let robot = load_robot(&robot_path)?;
    let resolve_options = ResolveOptions {
        locked: false,
        allow_floating: true,
        resolve_external_artifacts: options.resolve_external_artifacts,
    };
    let resolved = if let Some(releases) = releases {
        resolve_with_releases(&robot, project_root, &CATALOG, resolve_options, releases)?
    } else {
        let cache_dir = host_paths::cache_dir()?;
        let source = if options.refresh_releases {
            ReleaseSource::RefreshCache(&cache_dir)
        } else {
            ReleaseSource::CacheDir(&cache_dir)
        };
        resolve_with_release_source(&robot, project_root, &CATALOG, resolve_options, source)?
    };
    let lockfile = Lockfile::from_resolved(&resolved);
    let lockfile_path = project_root.join(LOCKFILE_NAME);
    lockfile.write(&lockfile_path)?;

    Ok(UpdateSummary {
        lockfile_path,
        platform_runtime_count: resolved.platform_runtimes.len(),
        component_count: resolved.components.len(),
        tool_count: resolved.tools.len(),
    })
}
