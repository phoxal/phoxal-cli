use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;

use crate::AppContext;

pub mod check;
pub mod deploy;
pub mod doctor;
pub mod outdated;
pub mod pull;
pub mod robot;
pub mod self_cmd;
pub mod service;
pub mod simulate;
pub mod validate;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum MessageFormat {
    #[default]
    Human,
    Json,
}

pub fn print_message<T: Serialize>(
    value: &T,
    human: impl FnOnce() -> Result<()>,
    format: MessageFormat,
) -> Result<()> {
    match format {
        MessageFormat::Human => human(),
        MessageFormat::Json => {
            println!("{}", serde_json::to_string_pretty(value)?);
            Ok(())
        }
    }
}

/// Version string shared by the `--version` flag and the `version` subcommand,
/// e.g. `0.5.0 (macos-aarch64)`. clap prefixes `--version` with the binary name
/// and the subcommand prefixes it explicitly, so both render identically.
/// clap's `version` needs a `&'static str`, so the computed value is cached.
pub fn long_version() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| {
        format!(
            "{} ({}-{})",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })
}

#[derive(Debug, Parser)]
#[command(
    name = "phoxal-cli",
    version = long_version(),
    about = "Build, check, simulate, and deploy Phoxal robot projects.",
    long_about = "Build, check, simulate, and deploy Phoxal robot projects.\n\n\
                  phoxal-cli reads robot.yaml, resolves the graph against its official service catalog, and drives the develop/simulate/deploy loop. Start with `robot new`, then `check`, `simulate`, and `deploy build`."
)]
pub struct Cli {
    #[arg(
        long = "project-path",
        global = true,
        help = "Project path used for robot.yaml discovery. Defaults to current directory."
    )]
    pub project_path: Option<PathBuf>,

    #[command(subcommand)]
    pub command: RootCommand,
}

#[derive(Debug, Subcommand)]
pub enum RootCommand {
    #[command(
        about = "Check the robot graph's per-contract wire-shape agreement and topology via emit-apis.",
        long_about = "Check the robot graph's per-contract wire-shape agreement and topology via emit-apis.\n\n\
                      Resolves robot.yaml, then runs each available participant's emit-apis (host tools and locally built user services/component drivers) and validates the graph with phoxal::check. It fails if participants sharing a contract disagree on its schema_id (wire shape) or if the producer/consumer topology is unsatisfied. Mixed api_versions are allowed as long as shared contracts' schema_ids agree. Official artifact metadata lands with the native distribution work; git component commits resolve live unless pinned to a commit SHA in robot.yaml."
    )]
    Check(check::CheckCmd),
    #[command(about = "Validate robot.yaml structure and user-service phoxal dependencies.")]
    Validate(validate::Validate),
    #[command(about = "Resolve and stage a Webots simulation run.")]
    Simulate(simulate::Simulate),
    #[command(about = "Build a native deployment bundle.")]
    Deploy(deploy::Deploy),
    #[command(about = "Scaffold and manage robot projects.")]
    Robot(robot::Robot),
    #[command(about = "Refresh host tools; native service artifacts are pending.")]
    Pull(pull::Pull),
    #[command(about = "Report native artifact drift.")]
    Outdated(outdated::Outdated),
    #[command(about = "Check host prerequisites without modifying the host or project.")]
    Doctor(doctor::Doctor),
    #[command(about = "Scaffold and run user service crates.")]
    Service(service::Service),
    #[command(about = "Print the phoxal-cli version and supported api_versions.")]
    Version,
    #[command(name = "self", about = "Manage this phoxal-cli installation.")]
    SelfCmd(self_cmd::SelfCmd),
}

impl RootCommand {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        match self {
            Self::Check(command) => command.run(app).await,
            Self::Validate(command) => command.run(app).await,
            Self::Simulate(command) => command.run(app).await,
            Self::Deploy(command) => command.run(app).await,
            Self::Robot(command) => command.run(app).await,
            Self::Pull(command) => command.run(app).await,
            Self::Outdated(command) => command.run(app).await,
            Self::Doctor(command) => command.run(app).await,
            Self::Service(command) => command.run(app).await,
            Self::Version => {
                println!("phoxal-cli {}", long_version());
                let mut api_versions = crate::catalog::CATALOG
                    .entries
                    .iter()
                    .flat_map(|entry| entry.api_versions.iter().copied())
                    .collect::<Vec<_>>();
                api_versions.sort_unstable();
                api_versions.dedup();
                println!("official service API versions: {}", api_versions.join(", "));
                Ok(())
            }
            Self::SelfCmd(command) => command.run(app).await,
        }
    }
}

pub async fn dispatch(cli: Cli, app: &AppContext) -> Result<()> {
    cli.command.run(app).await
}
