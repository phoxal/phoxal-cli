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
        let _ = (app, &self.env, self.message_format);
        run(app.project.root(), &self.env)
    }
}

pub fn run(_project_start: &Path, _env: &[String]) -> Result<()> {
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
