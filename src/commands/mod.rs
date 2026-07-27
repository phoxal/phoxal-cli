use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::AppContext;

pub mod build;
pub mod bus_target;
pub mod deploy;
pub mod doctor;
pub mod init;
pub mod install;
pub mod logs;
pub mod resident;
pub mod run;
pub mod self_cmd;
pub mod service;
pub mod simulate;
pub mod start;
pub mod status;
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

#[derive(Debug, Args)]
pub struct VersionArgs {}

impl VersionArgs {
    pub fn run(&self) -> Result<()> {
        println!("phoxal {}", long_version());
        println!(
            "official packages: cargo install --registry {} at the Cargo.lock-selected framework train",
            phoxal_cli_core::project::catalog::REGISTRY_NAME
        );
        println!(
            "registry index: {}",
            phoxal_cli_core::project::catalog::REGISTRY_INDEX
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
                  phoxal reads robot.yaml and materializes official services, tools, the infrastructure router, and component drivers with `cargo install` against the phoxal registry, pinned exactly to the Cargo.lock-selected framework train, then drives the develop/simulate loop. Start by hand-authoring robot.yaml (see the framework repo's examples/ and getting-started docs), then run `build`, `run`, or `simulation webots run` - each validates the graph and every participant's config before it executes."
)]
pub struct Cli {
    #[arg(
        long = "project-path",
        global = true,
        help = "Project path used for robot.yaml discovery. Defaults to current directory."
    )]
    pub project_path: Option<PathBuf>,
    #[arg(
        long,
        env = crate::context::OFFLINE_ENV,
        global = true,
        help = "Pass --offline to every cargo install/metadata invocation this command makes."
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
        about = "Stage a runtime layout for a target and archive it as build.phoxal.",
        long_about = "Stage a runtime layout for a target and archive it as a deterministic build.phoxal.\n\n\
                      `build` refreshes staging exactly as `run` would - but for the selected --target rather than the host - validates the staged layout through the shared loader against the declared target architecture (no execution), and archives the staged layout deterministically: identical contents always produce identical archive bytes. The default output is a sibling of the staged directory, <project>/.phoxal/<triple>.build.phoxal, and the path plus its sha256 are printed at the end.\n\n\
                      `--builder` selects where compilation happens, never a different output: `local` (the default) compiles on this host with `cargo build --target`; `container` compiles natively inside the pinned official rust image for the target platform; `ssh://user@host` snapshots the source, compiles in a remote temporary directory, and pulls back the same archive. Every backend produces the identical deterministic build.phoxal."
    )]
    Build(build::Build),
    #[command(about = "Build and install a runtime on a prepared robot over SSH.")]
    Deploy(deploy::Deploy),
    #[command(about = "Install one compiled runtime archive atomically.")]
    Install(install::Install),
    #[command(about = "Activate an older installed runtime release.")]
    Rollback(install::Rollback),
    #[command(about = "Validate robot.yaml structure and Cargo workspace runtime ownership.")]
    Validate(validate::Validate),
    #[command(about = "Simulate a robot with `simulation webots run`.")]
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
    #[command(about = "Check host prerequisites without modifying the host or project.")]
    Doctor(doctor::Doctor),
    #[command(about = "Manage the systemd phoxal.service and inspect the official catalog.")]
    Service(service::Service),
    #[command(about = "Print the phoxal version and the official registry it installs from.")]
    Version(VersionArgs),
    #[command(name = "self", about = "Manage this phoxal installation.")]
    SelfCmd(self_cmd::SelfCmd),
}

impl RootCommand {
    /// Whether this invocation drives a [`crate::session::controller::SessionController`]-owned
    /// interactive session (`run` or foreground `simulation webots run`).
    fn enters_interactive_session(&self) -> bool {
        match self {
            Self::Run(run) => !run.detach,
            Self::Attach(_) => true,
            Self::Simulation(command) => match &command.command {
                simulate::SimulationSubcommand::Webots(webots) => match &webots.command {
                    simulate::WebotsSubcommand::Run(run) => !run.detach,
                },
            },
            _ => false,
        }
    }

    pub async fn run(&self, app: &AppContext) -> Result<()> {
        match self {
            Self::Init(command) => command.run(app).await,
            Self::Build(command) => command.run(app).await,
            Self::Deploy(command) => command.run(app).await,
            Self::Install(command) => command.run(app).await,
            Self::Rollback(command) => command.run(app).await,
            Self::Validate(command) => command.run(app).await,
            Self::Simulation(command) => command.run(app).await,
            Self::Run(command) => command.run(app).await,
            Self::Start(command) => command.run(app).await,
            Self::Attach(command) => command.run(app).await,
            Self::Stop(command) => command.run(app).await,
            Self::Logs(command) => command.run(app).await,
            Self::Status(command) => command.run(app).await,
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
            "interactive `run` and `simulation webots run` sessions require a terminal; run this command in a TTY"
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
mod parse_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// `enters_interactive_session` decides whether dispatch must require a
    /// terminal. Detached Webots sessions and finite commands do not.
    #[test]
    fn enters_interactive_session_covers_run_and_live_simulate_only() {
        let run = Cli::try_parse_from(["phoxal", "run"]).unwrap();
        assert!(run.command.enters_interactive_session());

        let simulate_live =
            Cli::try_parse_from(["phoxal", "simulation", "webots", "run", "default"])
                .expect("simulation webots run should parse");
        assert!(simulate_live.command.enters_interactive_session());

        let simulate_detached = Cli::try_parse_from([
            "phoxal",
            "simulation",
            "webots",
            "run",
            "default",
            "--detach",
        ])
        .expect("detached simulation webots run should parse");
        assert!(!simulate_detached.command.enters_interactive_session());
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
