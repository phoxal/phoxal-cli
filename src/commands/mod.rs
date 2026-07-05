use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};
use phoxal::bus::ApiVersion;
use serde::Serialize;

use crate::AppContext;
use crate::resolver::RobotManifestExtras;

pub mod check;
pub mod deploy;
pub mod doctor;
pub mod generations;
pub mod logs;
pub mod outdated;
pub mod pull;
pub mod run;
pub mod self_cmd;
pub mod service;
pub mod simulate;
pub mod status;
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

pub(crate) fn load_catalog_for_robot(
    app: &AppContext,
    project_root: &std::path::Path,
    manifest_extras: &RobotManifestExtras,
    refresh: bool,
) -> Result<Option<crate::catalog::CatalogRevision>> {
    load_catalog_for_robot_from_source(
        app.catalog_source.clone(),
        project_root,
        manifest_extras,
        refresh,
    )
}

pub(crate) fn load_catalog_for_robot_from_source(
    catalog_source: Option<String>,
    project_root: &std::path::Path,
    manifest_extras: &RobotManifestExtras,
    refresh: bool,
) -> Result<Option<crate::catalog::CatalogRevision>> {
    let robot_source = manifest_extras.catalog_source.as_ref().map(|source| {
        if source.is_absolute() {
            source.clone()
        } else {
            project_root.join(source)
        }
    });
    crate::catalog::load_catalog(crate::catalog::CatalogLoadOptions {
        refresh,
        cli_source: catalog_source,
        robot_source,
    })
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

#[derive(Debug, Args)]
pub struct VersionArgs {
    #[arg(
        long,
        value_enum,
        default_value_t = MessageFormat::Human,
        help = "Output format for the version report."
    )]
    pub message_format: MessageFormat,
}

/// What the CLI itself supports, independent of any one robot graph.
///
/// `api_version` compatibility is per-contract (`schema_id`, see emit-apis), not
/// a single graph-wide version, so this reports the CLI's own build identity
/// instead: its version, the `bus_abi` wire envelope it links, the emit-apis
/// metadata schema it understands, and the API generations it can resolve or
/// check against a catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VersionSummary {
    pub cli_version: &'static str,
    pub framework_compatibility: FrameworkCompatibility,
    pub metadata_schema: &'static str,
    pub supported_generations: Vec<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FrameworkCompatibility {
    pub bus_abi: &'static str,
}

/// The API generations this build can resolve/check against a catalog, i.e.
/// the version modules compiled into the linked `phoxal-api`. Only `y2026_1`
/// is published today; extend this list when the CLI links a `phoxal-api`
/// with more generation modules compiled in.
const SUPPORTED_GENERATIONS: &[&str] = &[phoxal_api::y2026_1::Api::ID];

pub fn version_summary() -> VersionSummary {
    VersionSummary {
        cli_version: env!("CARGO_PKG_VERSION"),
        framework_compatibility: FrameworkCompatibility {
            bus_abi: phoxal::bus::BUS_ABI.id(),
        },
        metadata_schema: phoxal::participant::emit::EMIT_SCHEMA,
        supported_generations: SUPPORTED_GENERATIONS.to_vec(),
    }
}

impl VersionArgs {
    pub fn run(&self) -> Result<()> {
        let summary = version_summary();
        print_message(
            &summary,
            || {
                println!("phoxal-cli {}", long_version());
                println!(
                    "default catalog URL: {}",
                    crate::catalog::DEFAULT_CATALOG_URL
                );
                println!(
                    "catalog override env: {}",
                    crate::catalog::CATALOG_SOURCE_ENV
                );
                Ok(())
            },
            self.message_format,
        )
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "phoxal-cli",
    version = long_version(),
    about = "Build, check, simulate, and deploy Phoxal robot projects.",
    long_about = "Build, check, simulate, and deploy Phoxal robot projects.\n\n\
                  phoxal-cli reads robot.yaml, resolves the graph against a verified generated artifact catalog when official native artifacts are needed, and drives the develop/simulate/deploy loop. Start by hand-authoring robot.yaml (see the framework repo's examples/ and getting-started docs), then run `check`, `generations status`, `simulate`, and `deploy --dry-run --target aarch64`."
)]
pub struct Cli {
    #[arg(
        long = "project-path",
        global = true,
        help = "Project path used for robot.yaml discovery. Defaults to current directory."
    )]
    pub project_path: Option<PathBuf>,
    #[arg(
        long = "catalog",
        env = crate::catalog::CATALOG_SOURCE_ENV,
        global = true,
        value_name = "PATH_OR_HTTPS_URL",
        help = "Artifact catalog override. Local paths are verified directly; HTTPS sources use the cache and refresh with --pull."
    )]
    pub catalog_source: Option<String>,

    #[command(subcommand)]
    pub command: RootCommand,
}

#[derive(Debug, Subcommand)]
pub enum RootCommand {
    #[command(
        about = "Check the robot graph's per-contract wire-shape agreement and topology via emit-apis.",
        long_about = "Check the robot graph's per-contract wire-shape agreement and topology via emit-apis.\n\n\
                      Resolves robot.yaml, then runs each available participant's emit-apis (host tools and locally built user services/component drivers) and validates the graph with phoxal::check. It fails if participants sharing a contract disagree on its schema_id (wire shape) or if the producer/consumer topology is unsatisfied. Mixed participant api_versions are allowed as long as shared contracts' schema_ids agree. Official artifact readiness comes from the configured generated catalog; git component commits resolve live unless pinned to a commit SHA in robot.yaml."
    )]
    Check(check::CheckCmd),
    #[command(about = "Validate robot.yaml structure and user-service phoxal dependencies.")]
    Validate(validate::Validate),
    #[command(about = "Resolve and report a Webots simulation launch plan.")]
    Simulate(simulate::Simulate),
    #[command(about = "Run the resolved robot graph with the host-native supervisor.")]
    Run(run::Run),
    #[command(about = "Stream participant bus logs from a reachable robot.")]
    Logs(logs::Logs),
    #[command(about = "Show the local supervisor board snapshot.")]
    Status(status::Status),
    #[command(about = "Deploy the checked graph as a native systemd payload.")]
    Deploy(deploy::Deploy),
    #[command(about = "Refresh the catalog and native artifact cache.")]
    Pull(pull::Pull),
    #[command(about = "Report native artifact drift.")]
    Outdated(outdated::Outdated),
    #[command(about = "Inspect API generation readiness from the artifact catalog.")]
    Generations(generations::Generations),
    #[command(about = "Check host prerequisites without modifying the host or project.")]
    Doctor(doctor::Doctor),
    #[command(about = "Inspect the user-service catalog.")]
    Service(service::Service),
    #[command(about = "Print the phoxal-cli version and catalog source defaults.")]
    Version(VersionArgs),
    #[command(name = "self", about = "Manage this phoxal-cli installation.")]
    SelfCmd(self_cmd::SelfCmd),
}

impl RootCommand {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        match self {
            Self::Check(command) => command.run(app).await,
            Self::Validate(command) => command.run(app).await,
            Self::Simulate(command) => command.run(app).await,
            Self::Run(command) => command.run(app).await,
            Self::Logs(command) => command.run(app).await,
            Self::Status(command) => command.run(app).await,
            Self::Deploy(command) => command.run(app).await,
            Self::Pull(command) => command.run(app).await,
            Self::Outdated(command) => command.run(app).await,
            Self::Generations(command) => command.run(app).await,
            Self::Doctor(command) => command.run(app).await,
            Self::Service(command) => command.run(app).await,
            Self::Version(command) => command.run(),
            Self::SelfCmd(command) => command.run(app).await,
        }
    }
}

pub async fn dispatch(cli: Cli, app: &AppContext) -> Result<()> {
    cli.command.run(app).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_summary_reports_cli_support_not_a_graph_wide_api_version() {
        let summary = version_summary();

        assert_eq!(summary.cli_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(
            summary.framework_compatibility.bus_abi,
            phoxal::bus::BUS_ABI.id()
        );
        assert_eq!(
            summary.metadata_schema,
            phoxal::participant::emit::EMIT_SCHEMA
        );
        assert_eq!(
            summary.supported_generations,
            vec![phoxal_api::y2026_1::Api::ID]
        );
    }

    #[test]
    fn version_summary_serializes_to_the_documented_json_shape() {
        let summary = version_summary();

        let value = serde_json::to_value(&summary).expect("summary should serialize");
        assert_eq!(
            value,
            serde_json::json!({
                "cli_version": env!("CARGO_PKG_VERSION"),
                "framework_compatibility": { "bus_abi": phoxal::bus::BUS_ABI.id() },
                "metadata_schema": phoxal::participant::emit::EMIT_SCHEMA,
                "supported_generations": [phoxal_api::y2026_1::Api::ID],
            })
        );
    }

    #[test]
    fn version_args_json_mode_prints_only_the_summary_document() -> Result<()> {
        let args = VersionArgs {
            message_format: MessageFormat::Json,
        };
        let summary = version_summary();
        let mut printed = String::new();
        print_message(
            &summary,
            || {
                printed.push_str("human path should not run in json mode");
                Ok(())
            },
            args.message_format,
        )?;
        assert!(printed.is_empty());
        Ok(())
    }

    #[test]
    fn version_args_human_mode_runs_the_human_closure_not_json() -> Result<()> {
        let args = VersionArgs {
            message_format: MessageFormat::Human,
        };
        let summary = version_summary();
        let mut ran_human = false;
        print_message(
            &summary,
            || {
                ran_human = true;
                Ok(())
            },
            args.message_format,
        )?;
        assert!(ran_human);
        Ok(())
    }
}
