use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;

use crate::AppContext;

pub mod behavior;
pub mod build;
pub mod check;
pub mod doctor;
pub mod init;
pub mod logs;
pub mod resident;
pub mod run;
pub mod self_cmd;
pub mod service;
pub mod simulate;
pub mod start;
pub mod status;
pub mod update;
pub mod validate;

/// Load the artifact suite for a robot project. There is no `refresh`
/// parameter anymore: [`phoxal_cli_core::project::suite::load_suite`] always fetches the
/// remote suite fresh (no on-disk cache of the fetch) unless an explicit
/// local/URL source is given. Offline resolution requires an explicit local
/// suite and uses only already-vendored artifacts.
pub(crate) fn load_suite_for_robot_from_source(
    suite_source: Option<String>,
    project_root: &std::path::Path,
) -> Result<Option<phoxal_cli_core::project::suite::Suite>> {
    let source = suite_source;
    #[cfg(test)]
    let locked_version = std::env::var("PHOXAL_TEST_LOCKED_TRAIN").ok().or_else(|| {
        source.as_ref().and_then(|source| {
            (!source.starts_with("https://"))
                .then(|| {
                    phoxal_cli_core::project::suite::read_suite_path(std::path::Path::new(source))
                        .ok()
                })
                .flatten()
                .map(|suite| suite.version)
        })
    });
    #[cfg(not(test))]
    let locked_version: Option<String> = None;
    let locked = if let Some(version) = locked_version {
        phoxal_cli_core::project::train::LockedTrain {
            version,
            source: phoxal_cli_core::project::train::TrainSource::Path,
        }
    } else {
        phoxal_cli_core::project::train::resolve_locked_train(project_root)?
    };
    phoxal_cli_core::project::suite::load_suite(
        phoxal_cli_core::project::suite::SuiteLoadOptions {
            cli_source: source,
            offline: false,
        },
        &locked.version,
    )
}

/// The locked framework train version for `project_root`, honoring the test
/// override so unit tests need no on-disk registry. This is the version the
/// vendored suite and artifact blobs are keyed under.
fn locked_train_version(project_root: &std::path::Path) -> Result<String> {
    #[cfg(test)]
    if let Ok(version) = std::env::var("PHOXAL_TEST_LOCKED_TRAIN") {
        return Ok(version);
    }
    Ok(phoxal_cli_core::project::train::resolve_locked_train(project_root)?.version)
}

/// Load the suite the offline execution paths (`run`/`start`/`build`/`watch`)
/// resolve against, strictly without touching the network (#936, finding 1).
///
/// An explicit *local* `--suite` path is honored as a dev/test override and read
/// directly; an HTTPS `--suite` is rejected, because these paths never fetch -
/// `phoxal update` is the only fetcher. With no override the persisted suite that
/// `phoxal update` vendored for the locked train is read from `.phoxal/artifacts`,
/// and its absence is an actionable "run `phoxal update`" error, never a fetch.
pub(crate) fn load_vendored_suite_for_robot(
    suite_source: Option<String>,
    project_root: &std::path::Path,
) -> Result<Option<phoxal_cli_core::project::suite::Suite>> {
    if let Some(source) = &suite_source {
        if source.starts_with("https://") || source.starts_with("http://") {
            bail!(
                "`run`/`start`/`build` never fetch a suite over the network; pass a local --suite path, or run `phoxal update` to vendor the locked train"
            );
        }
        return Ok(Some(phoxal_cli_core::project::suite::read_suite_path(
            std::path::Path::new(source),
        )?));
    }
    let locked_version = locked_train_version(project_root)?;
    let suite = crate::native_artifacts::load_vendored_suite(&locked_version)?.with_context(|| {
        format!(
            "the locked train {locked_version} suite is not vendored for this project; run `phoxal update`"
        )
    })?;
    Ok(Some(suite))
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
pub struct VersionArgs {}

/// What the CLI itself supports, independent of any one robot graph.
///
/// Contract compatibility is per-contract name identity now (D1) - there is
/// no single graph-wide API version ceiling to report. So
/// this reports the CLI's own build identity instead: its version, the wire
/// codec it speaks, and the linker-section names it reads a participant's
/// compiled-in `#[derive(phoxal::Api)]` metadata from (see
/// [`phoxal_cli_core::check::participant_metadata`]).
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
        participant_metadata_sections: &phoxal_cli_core::check::participant_metadata::SECTION_NAMES,
    }
}

impl VersionArgs {
    pub fn run(&self) -> Result<()> {
        println!("phoxal {}", long_version());
        println!(
            "default suite URL: the immutable suite attached to the Cargo.lock-selected framework release"
        );
        println!(
            "suite override env: {}",
            phoxal_cli_core::project::suite::SUITE_SOURCE_ENV
        );
        Ok(())
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "phoxal",
    version = long_version(),
    about = "Build, check, and simulate Phoxal robot projects.",
    long_about = "Build, check, and simulate Phoxal robot projects.\n\n\
                  phoxal reads robot.yaml, resolves the graph against a verified generated artifact suite when official native artifacts are needed, and drives the develop/simulate loop. Start by hand-authoring robot.yaml (see the framework repo's examples/ and getting-started docs), then run `check` and `simulate`."
)]
pub struct Cli {
    #[arg(
        long = "project-path",
        global = true,
        help = "Project path used for robot.yaml discovery. Defaults to current directory."
    )]
    pub project_path: Option<PathBuf>,
    #[arg(
        long = "suite",
        env = phoxal_cli_core::project::suite::SUITE_SOURCE_ENV,
        global = true,
        value_name = "PATH_OR_HTTPS_URL",
        help = "Artifact suite override. Local paths are read directly; HTTPS sources (including the default) are always fetched fresh - there is no on-disk cache of this fetch."
    )]
    pub suite_source: Option<String>,
    #[arg(
        long,
        env = phoxal_cli_core::project::suite::OFFLINE_ENV,
        global = true,
        help = "Disable network access. Resolution requires --suite <local-path> and uses only project-vendored artifacts."
    )]
    pub offline: bool,
    #[command(subcommand)]
    pub command: RootCommand,
}

#[derive(Debug, Subcommand)]
pub enum RootCommand {
    #[command(about = "Create a non-published root Cargo train anchor and committed lockfile.")]
    Init(init::Init),
    #[command(
        about = "Check the robot graph's participants and config against phoxal::check.",
        long_about = "Check the robot graph's participants and config against phoxal::check.\n\n\
                      Resolves robot.yaml and the Cargo workspace, then reads every participant's compiled-in contract metadata (official artifacts from the suite and workspace-built services, tools, and component drivers) and validates the graph with phoxal::check. Contract compatibility is per-contract name identity (D1) - two participants naming the same version-qualified contract are compatible by construction, so there is no wire-shape hash to agree on. This also validates each user service's manifest config against its emitted JSON Schema."
    )]
    Check(check::CheckCmd),
    #[command(
        about = "Stage a runtime layout for a target and archive it as build.phoxal.",
        long_about = "Stage a runtime layout for a target and archive it as a deterministic build.phoxal.\n\n\
                      `build` refreshes staging exactly as `run` would - but for the selected --target rather than the host - validates the staged layout through the shared loader against the declared target architecture (no execution), and archives the staged layout deterministically: identical contents always produce identical archive bytes. The default output is a sibling of the staged directory, <project>/.phoxal/build/<triple>.build.phoxal, and the path plus its sha256 are printed at the end.\n\n\
                      `--builder` selects where compilation happens, never a different output: `local` (the default) compiles on this host with `cargo build --target` (a missing cross toolchain is an actionable `rustup target add` error - the CLI never installs toolchains); `container` compiles inside a per-target toolchain image (the image defines the target, so --target is required) and then reuses the identical host-side staging + archive; `ssh://user@host` is the remote builder, which lands in phase 11 (#930). Every backend produces the identical deterministic build.phoxal. Extract a bundle with `phoxal run <dir>` after `tar -xzf build.phoxal`, or plain `tar` - the archive is ordinary tar.gz."
    )]
    Build(build::Build),
    // Preserved prototype for the parked behavior-orchestration design. Keep it
    // out of the supported command listing until that plan is rewritten.
    #[command(about = "Experimental behavior-orchestration prototype.", hide = true)]
    Behavior(behavior::Behavior),
    #[command(about = "Validate robot.yaml structure and Cargo workspace runtime ownership.")]
    Validate(validate::Validate),
    #[command(about = "Simulate a robot in Webots (see `simulation run`/`simulation join`).")]
    Simulation(simulate::Simulation),
    #[command(about = "Run the resolved robot graph with the host-native supervisor.")]
    Run(run::Run),
    #[command(
        about = "Run a robot instance headless (no TUI); the systemd-facing verb.",
        long_about = "Run a robot instance headless, with no terminal UI - the verb `phoxal.service` invokes.\n\n\
                      Uses the same universal pipeline as `run`: it classifies the root, refreshes staging when it is a buildable source project, and supervises the staged runtime layout. Invoked interactively it behaves like `run -d` - it detaches the resident supervisor, returns once required participants are ready, and prints how to attach or stop. Invoked under systemd (`Type=notify`) it stays the in-process foreground resident, signalling `READY=1` after readiness and pinging the watchdog while it runs."
    )]
    Start(start::Start),
    #[command(about = "Attach a thin terminal client to a running project supervisor.")]
    Attach(resident::Attach),
    #[command(about = "Orderly stop a running project supervisor.")]
    Stop(resident::Stop),
    #[command(about = "Stream participant bus logs from a reachable robot.")]
    Logs(logs::Logs),
    #[command(about = "Inspect live robot state through typed bus contracts.")]
    Status(status::Status),
    #[command(about = "Verify the locked train suite and atomically refresh cached artifacts.")]
    Update(update::Update),
    #[command(about = "Check host prerequisites without modifying the host or project.")]
    Doctor(doctor::Doctor),
    #[command(about = "Inspect the user-service suite.")]
    Service(service::Service),
    #[command(about = "Print the phoxal-cli version and suite source defaults.")]
    Version(VersionArgs),
    #[command(name = "self", about = "Manage this phoxal-cli installation.")]
    SelfCmd(self_cmd::SelfCmd),
}

impl RootCommand {
    /// Whether this invocation drives a [`crate::session::controller::SessionController`]-owned
    /// interactive session (`run`, or `simulation run` without `--dry-run`).
    /// `simulation join` (a stub, no session) and `simulation run --dry-run`
    /// (report-only, no controller) are excluded.
    fn enters_interactive_session(&self) -> bool {
        match self {
            Self::Run(run) => !run.detach,
            Self::Attach(_) => true,
            Self::Simulation(command) => matches!(
                &command.command,
                simulate::SimulationSubcommand::Run(run) if !run.dry_run
            ),
            _ => false,
        }
    }

    pub async fn run(&self, app: &AppContext) -> Result<()> {
        match self {
            Self::Init(command) => command.run(app).await,
            Self::Check(command) => command.run(app).await,
            Self::Build(command) => command.run(app).await,
            Self::Behavior(command) => command.run(app).await,
            Self::Validate(command) => command.run(app).await,
            Self::Simulation(command) => command.run(app).await,
            Self::Run(command) => command.run(app).await,
            Self::Start(command) => command.run(app).await,
            Self::Attach(command) => command.run(app).await,
            Self::Stop(command) => command.run(app).await,
            Self::Logs(command) => command.run(app).await,
            Self::Status(command) => command.run(app).await,
            Self::Update(command) => command.run(app).await,
            Self::Doctor(command) => command.run(app).await,
            Self::Service(command) => command.run(app).await,
            Self::Version(command) => command.run(),
            Self::SelfCmd(command) => command.run(app).await,
        }
    }
}

pub async fn dispatch(cli: Cli, app: &AppContext) -> Result<()> {
    let terminal = std::io::stderr().is_terminal();
    if cli.command.enters_interactive_session()
        && !terminal
        && !matches!(cli.command, RootCommand::Run(_))
    {
        bail!(
            "interactive `run` and `simulation run` sessions require a terminal; run this command in a TTY"
        );
    }
    let output = crate::session::output::OutputContext::compute(terminal);

    // Compute terminal presentation once and thread it into every long-running
    // operation through `AppContext::output` and `AppContext::ui`.
    let app = &AppContext {
        output,
        ui: crate::Ui::new(output.decorated()),
        ..app.clone()
    };

    cli.command.run(app).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_paths::test_support::ScratchPhoxalHome;

    /// A cold vendored store (no `phoxal update` yet) makes the offline suite
    /// loader fail with the actionable "run `phoxal update`" error, without ever
    /// attempting a fetch (#936, finding 1).
    #[test]
    fn offline_suite_loader_points_a_cold_store_at_phoxal_update() {
        let _scratch = ScratchPhoxalHome::new().expect("scratch home");
        let root = crate::host_paths::project_root().expect("project root");

        let error = load_vendored_suite_for_robot(None, &root)
            .expect_err("a cold store has no vendored suite");
        let message = format!("{error:#}");
        assert!(message.contains("phoxal update"), "{message}");
    }

    /// The offline suite loader never fetches: an HTTPS `--suite` is rejected
    /// outright rather than downloaded (#936, finding 1).
    #[test]
    fn offline_suite_loader_rejects_an_https_suite_source() {
        let _scratch = ScratchPhoxalHome::new().expect("scratch home");
        let root = crate::host_paths::project_root().expect("project root");

        let error = load_vendored_suite_for_robot(
            Some("https://example.com/suite.json".to_string()),
            &root,
        )
        .expect_err("run/start/build never fetch a suite");
        assert!(format!("{error:#}").contains("never fetch"), "{error:#}");
    }

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
            phoxal_cli_core::check::participant_metadata::SECTION_NAMES
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
                "participant_metadata_sections": phoxal_cli_core::check::participant_metadata::SECTION_NAMES,
            })
        );
    }

    /// `enters_interactive_session` decides whether dispatch must require a
    /// terminal. Dry-run and join commands never build a session controller.
    #[test]
    fn enters_interactive_session_covers_run_and_live_simulate_only() {
        let run = Cli::try_parse_from(["phoxal", "run"]).unwrap();
        assert!(run.command.enters_interactive_session());

        let simulate_live = Cli::try_parse_from(["phoxal", "simulation", "run", "default"])
            .expect("simulation run should parse");
        assert!(simulate_live.command.enters_interactive_session());

        let simulate_dry_run =
            Cli::try_parse_from(["phoxal", "simulation", "run", "default", "--dry-run"])
                .expect("simulation run --dry-run should parse");
        assert!(!simulate_dry_run.command.enters_interactive_session());

        let simulate_join = Cli::try_parse_from(["phoxal", "simulation", "join"])
            .expect("simulation join should parse");
        assert!(!simulate_join.command.enters_interactive_session());

        let check = Cli::try_parse_from(["phoxal", "check"]).unwrap();
        assert!(!check.command.enters_interactive_session());
    }

    /// The `phoxal build` clap surface parses the full flag set: the project
    /// positional, `--target`, `--builder` (local/container/ssh), `--output`,
    /// `--container-engine`, and `--builder-image`.
    #[test]
    fn build_command_parses_its_full_flag_surface() {
        let bare = Cli::try_parse_from(["phoxal", "build"]).expect("bare build should parse");
        let RootCommand::Build(build) = bare.command else {
            panic!("expected a build command");
        };
        assert_eq!(build.builder, "local");
        assert!(build.target.is_none());
        assert_eq!(
            build.container_engine,
            build::container::ContainerEngine::Docker
        );

        let full = Cli::try_parse_from([
            "phoxal",
            "build",
            "project",
            "--target",
            "aarch64-unknown-linux-gnu",
            "--builder",
            "container",
            "--output",
            "out/bundle.build.phoxal",
            "--container-engine",
            "podman",
            "--builder-image",
            "ghcr.io/example/custom:latest",
        ])
        .expect("full build flags should parse");
        let RootCommand::Build(build) = full.command else {
            panic!("expected a build command");
        };
        assert_eq!(
            build.project.as_deref(),
            Some(std::path::Path::new("project"))
        );
        assert_eq!(build.target.as_deref(), Some("aarch64-unknown-linux-gnu"));
        assert_eq!(build.builder, "container");
        assert_eq!(
            build.output.as_deref(),
            Some(std::path::Path::new("out/bundle.build.phoxal"))
        );
        assert_eq!(
            build.container_engine,
            build::container::ContainerEngine::Podman
        );
        assert_eq!(
            build.builder_image.as_deref(),
            Some("ghcr.io/example/custom:latest")
        );
    }

    /// An `ssh://` builder is accepted by clap (it is a free-form string) and
    /// rejected only at run time; the value round-trips through parsing.
    #[test]
    fn build_command_accepts_the_ssh_builder_string() {
        let cli =
            Cli::try_parse_from(["phoxal", "build", "--builder", "ssh://dev@jetson-nano-orin"])
                .expect("ssh builder string should parse");
        let RootCommand::Build(build) = cli.command else {
            panic!("expected a build command");
        };
        assert_eq!(build.builder, "ssh://dev@jetson-nano-orin");
    }

    /// `phoxal start` is headless: it never drives the interactive
    /// `SessionController`/TUI, so it must never be classified as an interactive
    /// session (that classification is exactly what mounts the TUI for `run`).
    #[test]
    fn start_is_headless_and_never_enters_the_interactive_session() {
        let start = Cli::try_parse_from(["phoxal", "start"]).expect("start should parse");
        assert!(!start.command.enters_interactive_session());
        let start = Cli::try_parse_from(["phoxal", "start", "/var/phoxal"])
            .expect("start with a root should parse");
        assert!(!start.command.enters_interactive_session());
    }
}
