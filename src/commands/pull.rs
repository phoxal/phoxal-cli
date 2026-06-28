use std::path::Path;

use anyhow::{Context, Result};
use clap::Args;
use serde::Serialize;

use crate::AppContext;
use crate::catalog::CATALOG;
use crate::commands::MessageFormat;
use crate::resolver::{ResolveOptions, discover_robot_yaml, load_robot, resolve};

#[derive(Debug, Args)]
pub struct Pull {
    #[arg(long, value_enum, default_value_t = MessageFormat::Human)]
    pub message_format: MessageFormat,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PullSummary {
    pub api_version: String,
    pub channel: String,
    pub platform_runtime_count: usize,
    pub tool_count: usize,
}

impl Pull {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let project_root = app.project.root().to_path_buf();
        let ui = app.ui;
        let summary = tokio::task::spawn_blocking(move || run(&project_root, &ui))
            .await
            .context("pull worker failed")??;
        crate::commands::print_message(
            &summary,
            || {
                println!(
                    "pulled {} platform runtimes and {} tools for api_version {} (channel {})",
                    summary.platform_runtime_count,
                    summary.tool_count,
                    summary.api_version,
                    summary.channel
                );
                Ok(())
            },
            self.message_format,
        )
    }
}

pub fn run(project_start: &Path, ui: &crate::Ui) -> Result<PullSummary> {
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
            locked: false,
            resolve_external_artifacts: false,
        },
    )?;
    let platform_refs = resolved
        .platform_runtimes
        .iter()
        .map(|runtime| (runtime.name.clone(), runtime.tag_ref()))
        .collect::<Vec<_>>();
    crate::local_build::pull_platform_image_refs(&platform_refs)?;
    let tool_names = resolved
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    crate::tool_provisioning::ensure_tool_binaries(ui, &resolved, tool_names)?;

    Ok(PullSummary {
        api_version: resolved.api_version,
        channel: resolved.channel.to_string(),
        platform_runtime_count: resolved.platform_runtimes.len(),
        tool_count: resolved.tools.len(),
    })
}
