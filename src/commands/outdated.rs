use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use clap::Args;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::AppContext;
use crate::catalog::CATALOG;
use crate::commands::MessageFormat;
use crate::resolver::{
    ResolveOptions, ResolvedTool, discover_robot_yaml, load_robot, resolve,
    resolve_release_asset_sha256,
};
use crate::simulator_staging::cached_tool_path;

#[derive(Debug, Args)]
pub struct Outdated {
    #[arg(
        long,
        value_enum,
        default_value_t = MessageFormat::Human,
        help = "Output format for the drift report."
    )]
    pub message_format: MessageFormat,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OutdatedSummary {
    pub api_version: String,
    pub channel: String,
    pub platform_services: Vec<ImageOutdatedEntry>,
    pub tools: Vec<ToolOutdatedEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImageOutdatedEntry {
    pub name: String,
    pub image_ref: String,
    pub local_digest: Option<String>,
    pub remote_digest: String,
    pub status: OutdatedStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolOutdatedEntry {
    pub name: String,
    pub version: String,
    pub asset: String,
    pub cached_binary_sha256: Option<String>,
    pub cached_asset_sha256: Option<String>,
    pub remote_asset_sha256: Option<String>,
    pub status: OutdatedStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutdatedStatus {
    Current,
    Outdated,
    Missing,
    Unknown,
}

impl Outdated {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let project_root = app.project.root().to_path_buf();
        let summary = tokio::task::spawn_blocking(move || run(&project_root))
            .await
            .context("outdated worker failed")??;
        crate::commands::print_message(&summary, || print_human(&summary), self.message_format)
    }
}

pub fn run(project_start: &Path) -> Result<OutdatedSummary> {
    let robot_path = discover_robot_yaml(project_start)
        .with_context(|| format!("failed to find robot.yaml from {}", project_start.display()))?;
    let project_root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?;
    let robot = load_robot(&robot_path)?;
    let resolved = resolve(
        &robot,
        project_root,
        &CATALOG,
        ResolveOptions {
            resolve_external_artifacts: false,
            resolve_source_commits: false,
        },
    )?;

    let mut platform_services = Vec::new();
    for runtime in &resolved.platform_runtimes {
        let local_digest = local_image_digest(&runtime.image_ref)?;
        let remote_digest = crate::resolver::resolve_image_digest(&runtime.image_ref)
            .with_context(|| {
                format!(
                    "failed to resolve remote digest for service {} ({})",
                    runtime.name, runtime.image_ref
                )
            })?;
        let status = match local_digest.as_deref() {
            None => OutdatedStatus::Missing,
            Some(local) if local == remote_digest => OutdatedStatus::Current,
            Some(_) => OutdatedStatus::Outdated,
        };
        platform_services.push(ImageOutdatedEntry {
            name: runtime.name.clone(),
            image_ref: runtime.image_ref.clone(),
            local_digest,
            remote_digest,
            status,
        });
    }

    let mut tools = Vec::new();
    for tool in &resolved.tools {
        tools.push(tool_outdated_entry(tool)?);
    }

    Ok(OutdatedSummary {
        api_version: resolved.api_version,
        channel: resolved.channel.to_string(),
        platform_services,
        tools,
    })
}

fn print_human(summary: &OutdatedSummary) -> Result<()> {
    let mut any_outdated = false;
    println!(
        "api_version: {} (channel {})",
        summary.api_version, summary.channel
    );
    println!("official services:");
    for runtime in &summary.platform_services {
        any_outdated |= runtime.status != OutdatedStatus::Current;
        println!(
            "  - {}: {:?} local={} remote={}",
            runtime.name,
            runtime.status,
            runtime.local_digest.as_deref().unwrap_or("<missing>"),
            runtime.remote_digest
        );
    }
    println!("tools:");
    for tool in &summary.tools {
        any_outdated |= !matches!(
            tool.status,
            OutdatedStatus::Current | OutdatedStatus::Unknown
        );
        println!(
            "  - {}: {:?} cached={} remote={}",
            tool.name,
            tool.status,
            tool.cached_asset_sha256.as_deref().unwrap_or("<none>"),
            tool.remote_asset_sha256.as_deref().unwrap_or("<unknown>")
        );
    }
    if !any_outdated {
        println!("all cached artifacts are current");
    }
    Ok(())
}

fn local_image_digest(image_ref: &str) -> Result<Option<String>> {
    let output = crate::shell::run_stdout(
        "docker",
        [
            "image",
            "inspect",
            image_ref,
            "--format",
            "{{json .RepoDigests}}",
        ],
        None,
    );
    let output = match output {
        Ok(output) => output,
        Err(_) => return Ok(None),
    };
    let repo_digests: Vec<String> = serde_json::from_str(output.trim())
        .context("docker image inspect RepoDigests was not JSON")?;
    Ok(repo_digests
        .iter()
        .filter_map(|digest_ref| digest_ref.rsplit_once('@').map(|(_, digest)| digest))
        .find(|digest| digest.starts_with("sha256:"))
        .map(ToString::to_string))
}

fn tool_outdated_entry(tool: &ResolvedTool) -> Result<ToolOutdatedEntry> {
    let binary_path = cached_tool_path(&tool.name, &tool.resolved, &tool.binary_name)?;
    let cached_binary_sha256 = if binary_path.is_file() {
        Some(hex::encode(Sha256::digest(
            fs::read(&binary_path)
                .with_context(|| format!("failed to read cached tool {}", binary_path.display()))?,
        )))
    } else {
        None
    };
    let cached_asset_sha256 = crate::tool_provisioning::cached_tool_asset_sha256(tool)?;
    let remote_asset_sha256 =
        resolve_release_asset_sha256(&tool.repo, &tool.resolved, &tool.asset)?;
    let status = match (
        &cached_asset_sha256,
        &remote_asset_sha256,
        &cached_binary_sha256,
    ) {
        (None, _, None) => OutdatedStatus::Missing,
        (Some(local), Some(remote), _) if local == remote => OutdatedStatus::Current,
        (Some(_), Some(_), _) => OutdatedStatus::Outdated,
        (Some(_), None, _) | (None, _, Some(_)) => OutdatedStatus::Unknown,
    };
    Ok(ToolOutdatedEntry {
        name: tool.name.clone(),
        version: tool.resolved.clone(),
        asset: tool.asset.clone(),
        cached_binary_sha256,
        cached_asset_sha256,
        remote_asset_sha256,
        status,
    })
}
