use std::path::Path;

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::AppContext;
use crate::commands::MessageFormat;

#[derive(Debug, Args)]
pub struct Deploy {
    #[command(subcommand)]
    pub command: DeploySubcommand,
}

#[derive(Debug, Subcommand)]
pub enum DeploySubcommand {
    #[command(
        about = "Build a native deployment release artifact.",
        long_about = "Build a native deployment release artifact.\n\n\
                      This surface is reserved for the native systemd release renderer."
    )]
    Build(Build),
}

#[derive(Debug, Args)]
pub struct Build {
    #[arg(
        long,
        value_name = "ENV",
        help = "Apply a robot.<env>.yaml overlay before building (repeatable)."
    )]
    pub env: Vec<String>,
    #[arg(
        long,
        value_enum,
        default_value_t = MessageFormat::Human,
        help = "Output format for the build summary."
    )]
    pub message_format: MessageFormat,
}

impl Deploy {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        match &self.command {
            DeploySubcommand::Build(command) => command.run(app).await,
        }
    }
}

impl Build {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let _ = self.message_format;
        run_with_ui(
            app.project.root(),
            &self.env,
            app.catalog_source.clone(),
            &app.ui,
        )
    }
}

pub fn run(project_start: &Path, env: &[String]) -> Result<()> {
    run_with_ui(project_start, env, None, &crate::Ui)
}

fn run_with_ui(
    project_start: &Path,
    env: &[String],
    catalog_source: Option<String>,
    ui: &crate::Ui,
) -> Result<()> {
    let Ok(robot_path) = crate::resolver::discover_robot_yaml(project_start) else {
        return Err(crate::native_pending::error(
            "the systemd release renderer (03)",
        ));
    };
    let project_root = robot_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("robot.yaml did not have a parent directory"))?;
    let loaded = if env.is_empty() {
        crate::resolver::load_robot_with_extras(&robot_path)?
    } else {
        crate::resolver::load_robot_with_extras_and_overlays(&robot_path, env)?
    };
    let catalog = crate::commands::load_catalog_for_robot_from_source(
        catalog_source,
        project_root,
        &loaded.extras,
        false,
    )?;
    let resolved = crate::resolver::resolve(
        &loaded.robot,
        project_root,
        catalog.as_ref(),
        crate::resolver::ResolveOptions {
            resolve_external_artifacts: false,
            resolve_source_commits: true,
        },
    )?;
    if !resolved.path_overrides.is_empty() {
        let labels = resolved
            .path_overrides
            .iter()
            .map(|override_| {
                format!(
                    "{} {} from {}",
                    override_.kind.label(),
                    override_.artifact_name,
                    override_.path.display()
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        ui.warn(format!(
            "deploy build resolved local path overrides ({labels}); the plan-03 release record will mark these artifacts locally-built with source-tree and binary checksums"
        ));
        // Plan 03 owns the actual phoxal-release.json locally-built marking.
    }
    Err(crate::native_pending::error(
        "the systemd release renderer (03)",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deploy_build_returns_native_pending() {
        let error = run(Path::new("."), &[]).expect_err("deploy build should be pending");
        assert!(
            error
                .to_string()
                .contains("the systemd release renderer (03)")
        );
    }
}
