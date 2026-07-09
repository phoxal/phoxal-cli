use std::path::Path;

use anyhow::{Context, Result};
use clap::Args;
use serde::Serialize;

use crate::AppContext;
use crate::commands::MessageFormat;
use crate::resolver::{ResolveOptions, discover_robot_yaml, load_robot_with_extras, resolve};

#[derive(Debug, Args)]
pub struct Pull {
    #[arg(
        long,
        value_enum,
        default_value_t = MessageFormat::Human,
        help = "Output format for the pull summary."
    )]
    pub message_format: MessageFormat,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PullSummary {
    pub channel: String,
    pub catalog_revision: Option<String>,
    pub tool_count: usize,
    pub artifact_count: usize,
}

impl Pull {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let project_root = app.project.root().to_path_buf();
        let catalog_source = app.catalog_source.clone();
        let ui = app.ui;
        let summary = tokio::task::spawn_blocking(move || run(&project_root, catalog_source, &ui))
            .await
            .context("pull worker failed")??;
        crate::commands::print_message(
            &summary,
            || {
                println!(
                    "refreshed catalog and {} native artifact(s) (channel {})",
                    summary.artifact_count, summary.channel
                );
                if let Some(revision) = &summary.catalog_revision {
                    println!("catalog revision: {revision}");
                }
                Ok(())
            },
            self.message_format,
        )
    }
}

pub fn run(
    project_start: &Path,
    catalog_source: Option<String>,
    ui: &crate::Ui,
) -> Result<PullSummary> {
    let robot_path = discover_robot_yaml(project_start)
        .with_context(|| format!("failed to find robot.yaml from {}", project_start.display()))?;
    let project_root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?;
    let loaded = load_robot_with_extras(&robot_path)?;
    let robot = loaded.robot;
    let catalog = crate::catalog::load_catalog(crate::catalog::CatalogLoadOptions {
        cli_source: catalog_source,
        robot_source: loaded.extras.catalog_source.as_ref().map(|source| {
            if source.is_absolute() {
                source.clone()
            } else {
                project_root.join(source)
            }
        }),
    })?;
    let resolved = resolve(
        &robot,
        project_root,
        catalog.as_ref(),
        ResolveOptions {
            // pull refreshes official artifacts and host tools only; it never
            // reads component commits, so it stays off the network for git refs.
            resolve_source_commits: false,
            resolve_component_asset_commits: false,
            ..ResolveOptions::default()
        },
    )?;
    let tool_names = resolved
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    let tool_count = tool_names.len();
    let artifact_count = crate::native_artifacts::stage_resolved_artifacts(
        Some(ui),
        &resolved,
        crate::native_artifacts::ProvisioningMode::Refresh,
    )?;

    Ok(PullSummary {
        channel: resolved.channel.to_string(),
        catalog_revision: resolved.catalog_revision.clone(),
        tool_count,
        artifact_count,
    })
}
