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
pub mod runtime;
pub mod self_cmd;
pub mod simulate;
pub mod update;
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
                  phoxal-cli reads robot.yaml, resolves the graph against its compiled-in official runtime catalog, and drives the develop/simulate/deploy loop. Start with `robot new`, then `check`, `simulate`, and `deploy build`."
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
        about = "Check the robot graph's single api_version and topology via emit-apis.",
        long_about = "Check the robot graph's single api_version and topology via emit-apis.\n\n\
                      Resolves robot.yaml, then runs each participant's emit-apis (official images, host tools, and locally built user runtimes/component drivers) and fails if any normal participant reports a different api_version or if the producer/consumer topology is unsatisfied. Offline by default: cached official images are used and pulled only when missing (pass --pull to refresh first)."
    )]
    Check(check::CheckCmd),
    #[command(about = "Validate robot.yaml structure and user-runtime phoxal dependencies.")]
    Validate(validate::Validate),
    #[command(about = "Resolve robot.yaml and write phoxal.sources.lock.")]
    Update(update::Update),
    #[command(about = "Resolve, generate the run bundle, and launch the Webots simulation stack.")]
    Simulate(simulate::Simulate),
    #[command(about = "Build an immutable, digest-pinned deployment artifact.")]
    Deploy(deploy::Deploy),
    #[command(about = "Scaffold and manage robot projects.")]
    Robot(robot::Robot),
    #[command(about = "Refresh cached official runtime images and host tools.")]
    Pull(pull::Pull),
    #[command(about = "Report cached official images/tools that have newer remote digests.")]
    Outdated(outdated::Outdated),
    #[command(about = "Check host prerequisites without modifying the host or project.")]
    Doctor(doctor::Doctor),
    #[command(about = "Scaffold and run user runtime crates.")]
    Runtime(runtime::Runtime),
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
            Self::Update(command) => command.run(app).await,
            Self::Simulate(command) => command.run(app).await,
            Self::Deploy(command) => command.run(app).await,
            Self::Robot(command) => command.run(app).await,
            Self::Pull(command) => command.run(app).await,
            Self::Outdated(command) => command.run(app).await,
            Self::Doctor(command) => command.run(app).await,
            Self::Runtime(command) => command.run(app).await,
            Self::Version => {
                println!("phoxal-cli {}", long_version());
                let mut api_versions = crate::catalog::CATALOG
                    .entries
                    .iter()
                    .flat_map(|entry| entry.api_versions.iter().copied())
                    .collect::<Vec<_>>();
                api_versions.sort_unstable();
                api_versions.dedup();
                println!("official runtime API versions: {}", api_versions.join(", "));
                Ok(())
            }
            Self::SelfCmd(command) => command.run(app).await,
        }
    }
}

pub async fn dispatch(cli: Cli, app: &AppContext) -> Result<()> {
    cli.command.run(app).await
}
