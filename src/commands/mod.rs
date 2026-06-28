use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::AppContext;

pub mod check;
pub mod doctor;
pub mod runtime;
pub mod self_cmd;
pub mod simulate;
pub mod update;
pub mod validate;

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
    about = "Resolve, validate, simulate, and doctor Phoxal robot projects."
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
    #[command(about = "Validate the robot graph's API version + topology via emit-apis.")]
    Check(check::CheckCmd),
    #[command(about = "Validate the robot project discovered from robot.yaml.")]
    Validate(validate::Validate),
    #[command(about = "Resolve robot.yaml and refresh phoxal.lock.")]
    Update(update::Update),
    #[command(about = "Resolve and launch the local Webots simulation stack.")]
    Simulate(simulate::Simulate),
    #[command(about = "Check host prerequisites without modifying the host or project.")]
    Doctor(doctor::Doctor),
    #[command(about = "Manage user runtime crates.")]
    Runtime(runtime::Runtime),
    #[command(about = "Print the phoxal-cli version.")]
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
