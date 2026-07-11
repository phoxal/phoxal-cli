use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use crate::AppContext;
use crate::resolver::RobotManifestExtras;

pub mod cache;
pub mod check;
pub mod deploy;
pub mod doctor;
pub mod init;
pub mod logs;
pub mod run;
pub mod self_cmd;
pub mod service;
pub mod simulate;
pub mod status;
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

/// Load the artifact catalog for a robot project. There is no `refresh`
/// parameter anymore: [`crate::catalog::load_catalog`] always fetches the
/// remote catalog fresh (no on-disk cache of the fetch) unless an explicit
/// local/URL source is given - see its docs.
pub(crate) fn load_catalog_for_robot(
    app: &AppContext,
    project_root: &std::path::Path,
    channel: phoxal::model::robot::v0::Channel,
    manifest_extras: &RobotManifestExtras,
) -> Result<Option<crate::catalog::Catalog>> {
    load_catalog_for_robot_from_source(
        app.catalog_source.clone(),
        project_root,
        channel,
        manifest_extras,
    )
}

pub(crate) fn load_catalog_for_robot_from_source(
    catalog_source: Option<String>,
    project_root: &std::path::Path,
    channel: phoxal::model::robot::v0::Channel,
    manifest_extras: &RobotManifestExtras,
) -> Result<Option<crate::catalog::Catalog>> {
    let robot_source = manifest_extras.catalog_source.as_ref().map(|source| {
        if source.is_absolute() {
            source.clone()
        } else {
            project_root.join(source)
        }
    });
    catalog_or_vendored(crate::catalog::load_pinned_catalog(
        crate::catalog::CatalogLoadOptions {
            cli_source: catalog_source,
            robot_source,
            offline: false,
        },
        crate::catalog::selection_channel(channel),
    ))
}

pub(crate) fn catalog_or_vendored(
    loaded: Result<Option<crate::catalog::Catalog>>,
) -> Result<Option<crate::catalog::Catalog>> {
    match loaded {
        Ok(catalog) => Ok(catalog),
        Err(error) if crate::host_paths::binaries_dir().is_ok_and(|path| path.is_dir()) => {
            if std::env::var_os("PHOXAL_QUIET").is_none() {
                eprintln!(
                    "warning: catalog unreachable, continuing with project-vendored files: {error:#}"
                );
            }
            Ok(None)
        }
        Err(error) => Err(error),
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
/// Contract compatibility is per-contract name identity now (D1) - there is
/// no single graph-wide API version or generation ceiling left to report. So
/// this reports the CLI's own build identity instead: its version, the wire
/// codec it speaks, and the linker-section names it reads a participant's
/// compiled-in `#[derive(phoxal::Api)]` metadata from (see
/// [`crate::participant_metadata`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VersionSummary {
    pub cli_version: &'static str,
    pub wire_codec: String,
    pub participant_metadata_sections: &'static [&'static str],
}

pub fn version_summary() -> VersionSummary {
    VersionSummary {
        cli_version: env!("CARGO_PKG_VERSION"),
        wire_codec: phoxal::bus::encoding_string(phoxal::bus::CodecId::MessagePack),
        participant_metadata_sections: &crate::participant_metadata::SECTION_NAMES,
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
                  phoxal-cli reads robot.yaml, resolves the graph against a verified generated artifact catalog when official native artifacts are needed, and drives the develop/simulate/deploy loop. Start by hand-authoring robot.yaml (see the framework repo's examples/ and getting-started docs), then run `check`, `simulate`, and `deploy --dry-run --target aarch64`."
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
        help = "Artifact catalog override. Local paths are read directly; HTTPS sources (including the default) are always fetched fresh - there is no on-disk cache of this fetch."
    )]
    pub catalog_source: Option<String>,
    #[arg(
        long,
        env = crate::catalog::OFFLINE_ENV,
        global = true,
        help = "Use only project-vendored artifacts and skip catalog probes."
    )]
    pub offline: bool,
    #[arg(long, global = true, help = "Suppress update notices.")]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: RootCommand,
}

#[derive(Debug, Subcommand)]
pub enum RootCommand {
    #[command(about = "Initialize a robot project and ignore project-local Phoxal state.")]
    Init(init::Init),
    #[command(
        about = "Check the robot graph's participants and config against phoxal::check.",
        long_about = "Check the robot graph's participants and config against phoxal::check.\n\n\
                      Resolves robot.yaml, then reads each available participant's compiled-in contract metadata (official artifacts from the catalog, host tools and locally built user services/component drivers from their own built binary) and validates the graph with phoxal::check. Contract compatibility is per-contract name identity (D1) - two participants naming the same generation-qualified contract are compatible by construction, so there is no wire-shape hash to agree on. This still validates each user service's manifest config against its emitted JSON Schema. Official artifact readiness comes from the configured generated catalog; git component commits resolve live unless pinned to a commit SHA in robot.yaml."
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
    #[command(about = "Resolve channel heads and atomically update project-vendored artifacts.")]
    Update(update::Update),
    #[command(about = "Clean selected project-local .phoxal state.")]
    Cache(cache::CacheCmd),
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
    fn update_notice_format(&self) -> Option<MessageFormat> {
        match self {
            Self::Check(command) => Some(command.message_format),
            Self::Simulate(command) => Some(command.message_format),
            Self::Run(command) => Some(command.message_format),
            Self::Deploy(command) => Some(command.message_format),
            _ => None,
        }
    }

    pub async fn run(&self, app: &AppContext) -> Result<()> {
        match self {
            Self::Init(command) => command.run(app).await,
            Self::Check(command) => command.run(app).await,
            Self::Validate(command) => command.run(app).await,
            Self::Simulate(command) => command.run(app).await,
            Self::Run(command) => command.run(app).await,
            Self::Logs(command) => command.run(app).await,
            Self::Status(command) => command.run(app).await,
            Self::Deploy(command) => command.run(app).await,
            Self::Update(command) => command.run(app).await,
            Self::Cache(command) => command.run(app).await,
            Self::Doctor(command) => command.run(app).await,
            Self::Service(command) => command.run(app).await,
            Self::Version(command) => command.run(),
            Self::SelfCmd(command) => command.run(app).await,
        }
    }
}

pub async fn dispatch(cli: Cli, app: &AppContext) -> Result<()> {
    crate::update_notice::begin(crate::update_notice::NoticePolicy {
        artifact_consuming: cli.command.update_notice_format().is_some(),
        message_format: cli
            .command
            .update_notice_format()
            .unwrap_or(MessageFormat::Human),
        quiet: cli.quiet,
        interactive: std::io::stderr().is_terminal(),
    });
    let result = cli.command.run(app).await;
    crate::update_notice::finish();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_summary_reports_cli_support_not_a_graph_wide_api_version() {
        let summary = version_summary();

        assert_eq!(summary.cli_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(
            summary.wire_codec,
            phoxal::bus::encoding_string(phoxal::bus::CodecId::MessagePack)
        );
        assert_eq!(
            summary.participant_metadata_sections,
            crate::participant_metadata::SECTION_NAMES
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
                "wire_codec": phoxal::bus::encoding_string(phoxal::bus::CodecId::MessagePack),
                "participant_metadata_sections": crate::participant_metadata::SECTION_NAMES,
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
