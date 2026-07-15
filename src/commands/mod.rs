use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use crate::AppContext;
use crate::resolver::RobotManifestExtras;

pub mod behavior;
pub mod check;
pub mod deploy;
pub mod doctor;
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
            let value = json_message_value(value)?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
    }
}

fn json_message_value<T: Serialize>(value: &T) -> Result<serde_json::Value> {
    let mut value = serde_json::to_value(value)?;
    if let Some(updates) = crate::update_notice::take_json_updates()
        && let Some(object) = value.as_object_mut()
    {
        object.insert("updates_available".to_string(), updates);
    }
    Ok(value)
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
        app.output.mode,
    )
}

pub(crate) fn load_catalog_for_robot_from_source(
    catalog_source: Option<String>,
    project_root: &std::path::Path,
    channel: phoxal::model::robot::v0::Channel,
    manifest_extras: &RobotManifestExtras,
    mode: crate::output_mode::OutputMode,
) -> Result<Option<crate::catalog::Catalog>> {
    let robot_source = manifest_extras.catalog_source.as_ref().map(|source| {
        if source.is_absolute() {
            source.clone()
        } else {
            project_root.join(source)
        }
    });
    catalog_or_vendored(
        crate::catalog::load_pinned_catalog(
            crate::catalog::CatalogLoadOptions {
                cli_source: catalog_source,
                robot_source,
                offline: false,
            },
            crate::catalog::selection_channel(channel),
            mode,
        ),
        mode,
    )
}

pub(crate) fn catalog_or_vendored(
    loaded: Result<Option<crate::catalog::Catalog>>,
    mode: crate::output_mode::OutputMode,
) -> Result<Option<crate::catalog::Catalog>> {
    match loaded {
        Ok(catalog) => Ok(catalog),
        Err(error) if crate::host_paths::artifacts_dir().is_ok_and(|path| path.is_dir()) => {
            if std::env::var_os("PHOXAL_QUIET").is_none() && !mode.is_json() {
                let message = format!(
                    "catalog unreachable, continuing with project-vendored files: {error:#}"
                );
                // Finding A2: routed through an active session's diagnostics
                // instead of a raw stderr write whenever one is installed -
                // a direct `eprintln!` here could corrupt an active TUI
                // frame. Falls back to the original direct write only when
                // no session is active (matches `Ui::warn`'s own fallback).
                if matches!(
                    crate::session::diagnostics::try_route(
                        crate::session::event::DiagnosticSource::Cli,
                        crate::session::event::DiagnosticLevel::Warn,
                        &message,
                    ),
                    crate::session::diagnostics::RouteResult::NoSession
                ) {
                    eprintln!("warning: {message}");
                }
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
/// no single graph-wide API version ceiling to report. So
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
    #[arg(
        long,
        env = "PHOXAL_QUIET",
        global = true,
        value_parser = clap::builder::BoolishValueParser::new(),
        help = "Suppress update notices (recommended for CI)."
    )]
    pub quiet: bool,
    #[arg(
        long,
        global = true,
        help = "Force append-only stderr output: no spinner/progress redraw, no interactive TUI beyond the plain welcome card and log lines."
    )]
    pub plain: bool,

    #[command(subcommand)]
    pub command: RootCommand,
}

#[derive(Debug, Subcommand)]
pub enum RootCommand {
    #[command(
        about = "Check the robot graph's participants and config against phoxal::check.",
        long_about = "Check the robot graph's participants and config against phoxal::check.\n\n\
                      Resolves robot.yaml, then reads each available participant's compiled-in contract metadata (official artifacts from the catalog, host tools and locally built user services/component drivers from their own built binary) and validates the graph with phoxal::check. Contract compatibility is per-contract name identity (D1) - two participants naming the same version-qualified contract are compatible by construction, so there is no wire-shape hash to agree on. This still validates each user service's manifest config against its emitted JSON Schema. Official artifact readiness comes from the configured generated catalog; git component commits resolve live unless pinned to a commit SHA in robot.yaml."
    )]
    Check(check::CheckCmd),
    // Preserved prototype for the parked behavior-orchestration design. Keep it
    // out of the supported command listing until that plan is rewritten.
    #[command(about = "Experimental behavior-orchestration prototype.", hide = true)]
    Behavior(behavior::Behavior),
    #[command(about = "Validate robot.yaml structure and user-service phoxal dependencies.")]
    Validate(validate::Validate),
    #[command(about = "Simulate a robot in Webots (see `simulation run`/`simulation join`).")]
    Simulation(simulate::Simulation),
    #[command(about = "Run the resolved robot graph with the host-native supervisor.")]
    Run(run::Run),
    #[command(about = "Stream participant bus logs from a reachable robot.")]
    Logs(logs::Logs),
    #[command(about = "Inspect live robot state through typed bus contracts.")]
    Status(status::Status),
    #[command(about = "Deploy the checked graph as a native systemd payload.")]
    Deploy(deploy::Deploy),
    #[command(about = "Resolve channel heads and atomically update project-vendored artifacts.")]
    Update(update::Update),
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
            Self::Simulation(command) => command.update_notice_format(),
            Self::Run(command) => Some(command.message_format),
            Self::Deploy(command) => Some(command.message_format),
            _ => None,
        }
    }

    /// A "machine verb" answers a terse, scriptable question (the CLI's own
    /// version, a log stream, a status snapshot, the service catalog, or
    /// self-management) rather than driving the develop/simulate/deploy loop
    /// against a robot project. The welcome card is suppressed for these (see
    /// [`crate::identity::IdentityPolicy`]) so it never precedes output a
    /// script might be parsing.
    fn is_machine_verb(&self) -> bool {
        matches!(
            self,
            Self::Version(_)
                | Self::Logs(_)
                | Self::Status(_)
                | Self::Service(_)
                | Self::SelfCmd(_)
        )
    }

    /// Whether this invocation drives a [`crate::session::controller::SessionController`]-owned
    /// interactive session (`run`, or `simulation run` without `--dry-run`).
    /// Under a real TTY those two verbs render the welcome card INSIDE the
    /// TUI's own startup frame (`crate::tui::render::draw_startup`) instead
    /// of `dispatch`'s own [`crate::identity::print`] call - see
    /// [`dispatch`]'s use of this method. `simulation join` (a stub, no
    /// session) and `simulation run --dry-run` (report-only, no controller)
    /// are excluded: nothing else would ever render their card, so they keep
    /// going through `dispatch`.
    fn enters_interactive_session(&self) -> bool {
        match self {
            Self::Run(_) => true,
            Self::Simulation(command) => matches!(
                &command.command,
                simulate::SimulationSubcommand::Run(run) if !run.dry_run
            ),
            _ => false,
        }
    }

    /// Label the welcome card with the invocation's terminal shape. A dry
    /// run is always `plan only`; a live session is `full` when the TUI owns
    /// the terminal and `plain` otherwise; finite commands are `one shot`.
    fn welcome_terminal_mode(
        &self,
        output_mode: crate::output_mode::OutputMode,
    ) -> crate::identity::TerminalMode {
        let plan_only = match self {
            Self::Simulation(command) => matches!(
                &command.command,
                simulate::SimulationSubcommand::Run(run) if run.dry_run
            ),
            Self::Deploy(command) => command.dry_run,
            Self::Update(command) => command.dry_run,
            _ => false,
        };
        if plan_only {
            crate::identity::TerminalMode::PlanOnly
        } else if self.enters_interactive_session() {
            if output_mode == crate::output_mode::OutputMode::Rich {
                crate::identity::TerminalMode::Full
            } else {
                crate::identity::TerminalMode::Plain
            }
        } else {
            crate::identity::TerminalMode::OneShot
        }
    }

    /// The `--message-format` every verb answers with, defaulting to
    /// [`MessageFormat::Human`] for the handful of verbs (`validate`,
    /// `logs`, `doctor`) that have no format flag of their own. Broader than
    /// [`Self::update_notice_format`] (which only covers the four
    /// artifact-consuming verbs): this feeds [`crate::output_mode::OutputMode`]
    /// and [`crate::identity::IdentityPolicy`], which both need to know
    /// `--json` was requested regardless of which verb it was.
    fn message_format(&self) -> MessageFormat {
        match self {
            Self::Check(command) => command.message_format,
            Self::Behavior(command) => command.message_format(),
            Self::Validate(_) => MessageFormat::Human,
            Self::Simulation(command) => command.message_format(),
            Self::Run(command) => command.message_format,
            Self::Logs(_) => MessageFormat::Human,
            Self::Status(command) => command.message_format,
            Self::Deploy(command) => command.message_format,
            Self::Update(command) => command.message_format,
            Self::Doctor(_) => MessageFormat::Human,
            Self::Service(command) => {
                let service::ServiceSubcommand::Catalog(catalog) = &command.command;
                catalog.message_format
            }
            Self::Version(command) => command.message_format,
            Self::SelfCmd(command) => {
                let self_cmd::SelfSubcommand::Upgrade(upgrade) = &command.command;
                upgrade.message_format
            }
        }
    }

    pub async fn run(&self, app: &AppContext) -> Result<()> {
        match self {
            Self::Check(command) => command.run(app).await,
            Self::Behavior(command) => command.run(app).await,
            Self::Validate(command) => command.run(app).await,
            Self::Simulation(command) => command.run(app).await,
            Self::Run(command) => command.run(app).await,
            Self::Logs(command) => command.run(app).await,
            Self::Status(command) => command.run(app).await,
            Self::Deploy(command) => command.run(app).await,
            Self::Update(command) => command.run(app).await,
            Self::Doctor(command) => command.run(app).await,
            Self::Service(command) => command.run(app).await,
            Self::Version(command) => command.run(),
            Self::SelfCmd(command) => command.run(app).await,
        }
    }
}

impl Cli {
    /// The invocation-wide output contract, exposed so the binary can silence
    /// tracing and its top-level error reporter before dispatch starts.
    #[must_use]
    pub fn message_format(&self) -> MessageFormat {
        self.command.message_format()
    }
}

pub async fn dispatch(cli: Cli, app: &AppContext) -> Result<()> {
    let interactive = std::io::stderr().is_terminal();
    let message_format = cli.command.message_format();
    let output = crate::session::output::OutputContext::compute(
        interactive,
        cli.plain,
        cli.quiet,
        message_format,
    );

    let policy = crate::update_notice::NoticePolicy {
        artifact_consuming: cli.command.update_notice_format().is_some(),
        message_format: cli
            .command
            .update_notice_format()
            .unwrap_or(MessageFormat::Human),
        quiet: cli.quiet,
        interactive,
        tui: cli.command.enters_interactive_session()
            && output.mode == crate::output_mode::OutputMode::Rich,
    };
    crate::update_notice::begin(policy);
    if policy.artifact_consuming && !policy.quiet && policy.interactive {
        crate::update_notice::start_cli_check();
    }

    // The explicit `OutputContext` (Target design part 4): built once, here,
    // from the same inputs the notice policy above uses, plus the theme
    // detected off the same stream, and threaded explicitly into every
    // long-running operation below (catalog fetch, artifact download, git
    // resolve, cargo builds, the simulate readiness wait, `AppContext::ui`'s
    // own JSON gate) via `AppContext::output`/`AppContext::ui` - no
    // process-global mode cell.
    let app = &AppContext {
        output,
        ui: crate::Ui::new(output.mode),
        ..app.clone()
    };

    // The welcome card: gated independently of the output mode above (see
    // `crate::identity::IdentityPolicy` - `--plain` is deliberately not part
    // of this rule). A Rich-mode `run`/`simulation run` session renders the
    // SAME card inside its own TUI startup frame instead (Product decision
    // 4) - printing it here too would show it twice, once on the outgoing
    // primary screen and once inside the alternate screen the session then
    // opens.
    let identity_policy = crate::identity::IdentityPolicy {
        interactive,
        quiet: cli.quiet,
        message_format,
        machine_verb: cli.command.is_machine_verb(),
    };
    let card_rendered_by_the_session_itself = cli.command.enters_interactive_session()
        && output.mode == crate::output_mode::OutputMode::Rich;
    if !card_rendered_by_the_session_itself {
        crate::identity::print(
            identity_policy,
            app.project.root(),
            cli.command.welcome_terminal_mode(output.mode),
            crate::theme::Theme::detect_stderr(output.mode),
            version_summary().cli_version,
        );
    }

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

    #[test]
    fn update_and_non_artifact_verbs_are_excluded_from_update_notices() {
        let update = Cli::try_parse_from(["phoxal-cli", "update"]).unwrap();
        assert_eq!(update.command.update_notice_format(), None);

        let version = Cli::try_parse_from(["phoxal-cli", "version"]).unwrap();
        assert_eq!(version.command.update_notice_format(), None);
    }

    /// `enters_interactive_session` decides whether `dispatch` skips its own
    /// welcome-card print in favor of the TUI's own startup frame (see
    /// `dispatch`'s use of it) - `run` and a live `simulation run` must gate
    /// it, while a dry-run simulate or `simulation join` (neither ever builds
    /// a `SessionController`) must not, or their card would never render
    /// anywhere at all.
    #[test]
    fn enters_interactive_session_covers_run_and_live_simulate_only() {
        let run = Cli::try_parse_from(["phoxal-cli", "run"]).unwrap();
        assert!(run.command.enters_interactive_session());

        let simulate_live = Cli::try_parse_from(["phoxal-cli", "simulation", "run", "default"])
            .expect("simulation run should parse");
        assert!(simulate_live.command.enters_interactive_session());

        let simulate_dry_run =
            Cli::try_parse_from(["phoxal-cli", "simulation", "run", "default", "--dry-run"])
                .expect("simulation run --dry-run should parse");
        assert!(!simulate_dry_run.command.enters_interactive_session());

        let simulate_join = Cli::try_parse_from(["phoxal-cli", "simulation", "join"])
            .expect("simulation join should parse");
        assert!(!simulate_join.command.enters_interactive_session());

        let check = Cli::try_parse_from(["phoxal-cli", "check"]).unwrap();
        assert!(!check.command.enters_interactive_session());
    }

    #[test]
    fn welcome_terminal_mode_distinguishes_full_plain_plan_and_one_shot() {
        let live = Cli::try_parse_from(["phoxal-cli", "simulation", "run", "default"])
            .expect("live simulation should parse");
        assert_eq!(
            live.command
                .welcome_terminal_mode(crate::output_mode::OutputMode::Rich),
            crate::identity::TerminalMode::Full
        );
        assert_eq!(
            live.command
                .welcome_terminal_mode(crate::output_mode::OutputMode::Plain),
            crate::identity::TerminalMode::Plain
        );

        let plan = Cli::try_parse_from(["phoxal-cli", "simulation", "run", "default", "--dry-run"])
            .expect("simulation plan should parse");
        assert_eq!(
            plan.command
                .welcome_terminal_mode(crate::output_mode::OutputMode::Rich),
            crate::identity::TerminalMode::PlanOnly
        );

        let check = Cli::try_parse_from(["phoxal-cli", "check"]).expect("check should parse");
        assert_eq!(
            check
                .command
                .welcome_terminal_mode(crate::output_mode::OutputMode::Rich),
            crate::identity::TerminalMode::OneShot
        );
    }

    #[test]
    fn artifact_consuming_verb_exposes_its_message_format_to_notice_gate() {
        let check =
            Cli::try_parse_from(["phoxal-cli", "check", "--message-format", "json"]).unwrap();
        assert_eq!(
            check.command.update_notice_format(),
            Some(MessageFormat::Json)
        );
    }

    #[test]
    fn json_output_path_embeds_the_structured_updates_available_field() {
        crate::update_notice::begin(crate::update_notice::NoticePolicy {
            artifact_consuming: true,
            message_format: MessageFormat::Json,
            quiet: false,
            interactive: true,
            tui: false,
        });
        crate::update_notice::offer(crate::update_notice::UpdateNotice::Artifacts(vec![
            "phoxal/service-drive 1.0.0 -> 1.1.0".to_string(),
        ]));

        let value = json_message_value(&serde_json::json!({ "status": "ok" })).unwrap();

        assert_eq!(value["status"], "ok");
        assert_eq!(
            value["updates_available"][0],
            "phoxal/service-drive 1.0.0 -> 1.1.0"
        );
        crate::update_notice::finish();
    }
}
