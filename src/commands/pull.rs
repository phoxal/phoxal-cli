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
    pub target_generation: String,
    pub channel: String,
    pub catalog_revision: Option<String>,
    pub tool_count: usize,
    /// Official native service artifacts cannot be fetched yet; the generated
    /// catalog + native-asset pipeline lands with the native distribution work
    /// (phoxal/organization tmp/framework-rewrite follow-up 06).
    pub official_services_pending: bool,
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
                    "refreshed catalog and {} host tools for target generation {} (channel {})",
                    summary.tool_count, summary.target_generation, summary.channel
                );
                if let Some(revision) = &summary.catalog_revision {
                    println!("catalog revision: {revision}");
                }
                println!(
                    "official native service artifacts are pending (native distribution work 06)"
                );
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
        refresh: true,
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
            resolve_external_artifacts: false,
            resolve_source_commits: false,
        },
    )?;
    let tool_names = resolved
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    let tool_count = tool_names.len();
    crate::tool_provisioning::ensure_tool_binaries_with_mode(
        ui,
        &resolved,
        tool_names,
        crate::tool_provisioning::ProvisioningMode::Refresh,
    )?;

    // Host tools are native host binaries and refresh today; official service
    // artifacts need the generated catalog + native-asset pipeline (06), which
    // is not built yet. Report that honestly rather than erroring after the
    // tool refresh already ran.
    Ok(PullSummary {
        target_generation: resolved.target_generation.clone(),
        channel: resolved.channel.to_string(),
        catalog_revision: resolved.catalog_revision.clone(),
        tool_count,
        official_services_pending: true,
    })
}
