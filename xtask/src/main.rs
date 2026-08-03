//! Developer tooling for `phoxal-cli`.
//!
//! # Why this is not a `#[test]`
//!
//! Driving the shipped binary in a terminal is exactly what the organization's
//! AI assistant guide keeps out of repository tests and CI: it launches an
//! external program and depends on host state. That rule is right; a suite
//! that spawns processes is the kind of thing that goes flaky and then gets
//! ignored.
//!
//! The same guide *requires* that behaviour needing built artifacts is run on
//! the host and its outcome recorded as evidence. That run is what this is
//! for: a tool a developer (or an agent) invokes to produce the result
//! evidence a TUI change is reviewed on.
//!
//! It defines no `#[test]`s, so nothing here can execute under `cargo test` -
//! the harness runs only through `cargo xtask`.
//!
//! ```text
//! cargo xtask tui screens --scenario attach-no-resident
//! cargo xtask tui screens --scenario attach-no-resident --bless
//! ```

mod pty;
mod resident;
mod scenario;
mod snapshot;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use pty::TerminalSize;
use snapshot::Snapshots;

/// The terminal matrix every screen scenario runs at.
///
/// The supported minimum is the one that actually catches layout bugs; the
/// other two are the shapes a person really uses.
const TERMINAL_MATRIX: [TerminalSize; 4] = [
    TerminalSize::new(80, 24),  // the supported minimum
    TerminalSize::new(120, 32), // a normal terminal
    TerminalSize::new(200, 50), // a wide terminal
    TerminalSize::new(40, 12),  // deliberately too small
];

#[derive(Parser)]
#[command(name = "xtask", about = "Developer tooling for phoxal-cli.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Drive the real TUI in a terminal.
    Tui {
        #[command(subcommand)]
        command: TuiCommand,
    },
}

#[derive(Subcommand)]
enum TuiCommand {
    /// Render a scenario across the terminal matrix and compare snapshots.
    Screens(ScreensArgs),
    /// Render one scenario at one size and print the screen. No snapshots -
    /// this is the "what does it look like right now" verb.
    Show(ShowArgs),
    /// List the scenarios this harness knows.
    List,
}

#[derive(Args)]
struct ScreensArgs {
    /// Which scenario to run. `--help` on `list` shows them all.
    #[arg(long)]
    scenario: String,
    /// Rewrite snapshots instead of failing on a change.
    #[arg(long)]
    bless: bool,
    /// The robot project scenarios that need one run against.
    #[arg(long)]
    project: Option<PathBuf>,
    /// The `phoxal` binary to drive. Defaults to this workspace's release build.
    #[arg(long)]
    binary: Option<PathBuf>,
}

#[derive(Args)]
struct ShowArgs {
    #[arg(long)]
    scenario: String,
    #[arg(long, default_value = "120")]
    cols: u16,
    #[arg(long, default_value = "32")]
    rows: u16,
    #[arg(long)]
    project: Option<PathBuf>,
    #[arg(long)]
    binary: Option<PathBuf>,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Tui { command } => match command {
            TuiCommand::List => {
                println!("scenarios:");
                for scenario in scenario::ALL {
                    println!("  {:<24} {}", scenario.name, scenario.description);
                }
                Ok(())
            }
            TuiCommand::Show(args) => show(args),
            TuiCommand::Screens(args) => screens(args),
        },
    }
}

fn show(args: ShowArgs) -> Result<()> {
    let scenario = scenario::find(&args.scenario)?;
    let binary = resolve_binary(args.binary)?;
    let project = resolve_project(args.project, scenario)?;
    let size = TerminalSize::new(args.cols, args.rows);

    let _resident = scenario.prepare(&binary, &project)?;
    let session = scenario.launch(&binary, &project, size)?;
    println!("--- {} at {size} ---", scenario.name);
    println!("{}", session.screen());
    session.shutdown()
}

fn screens(args: ScreensArgs) -> Result<()> {
    let scenario = scenario::find(&args.scenario)?;
    let binary = resolve_binary(args.binary)?;
    let project = resolve_project(args.project, scenario)?;
    let mut snapshots = Snapshots::new(snapshot_root()?.join(scenario.name), args.bless);

    println!("{} · {}", scenario.name, scenario.description);
    // One resident for the whole matrix.
    let _resident = scenario.prepare(&binary, &project)?;
    for size in TERMINAL_MATRIX {
        let session = scenario.launch(&binary, &project, size)?;
        snapshots.check(&size.label(), &session.screen())?;
        session.shutdown()?;
    }
    snapshots.finish()
}

fn snapshot_root() -> Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("screens"))
}

/// Locate the binary to drive. The release build is the default because that
/// is what a user runs; a debug TUI can differ in timing enough to matter.
fn resolve_binary(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(binary) = explicit {
        anyhow::ensure!(binary.is_file(), "{} is not a file", binary.display());
        return Ok(binary);
    }
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("xtask has no parent directory")?
        .to_path_buf();
    let release = workspace.join("target/release/phoxal");
    anyhow::ensure!(
        release.is_file(),
        "{} does not exist - run `cargo build --release` first, or pass --binary",
        release.display()
    );
    Ok(release)
}

fn resolve_project(explicit: Option<PathBuf>, scenario: &scenario::Scenario) -> Result<PathBuf> {
    match explicit {
        Some(project) => {
            anyhow::ensure!(
                project.join("robot.yaml").is_file(),
                "{} has no robot.yaml",
                project.display()
            );
            Ok(project)
        }
        None => {
            anyhow::ensure!(
                !scenario.needs_project,
                "scenario `{}` drives a robot project - pass --project <dir>",
                scenario.name
            );
            std::env::current_dir().context("no current directory")
        }
    }
}

/// How long a scenario may take to paint its first usable frame. Generous:
/// a first `run` materializes packages, and a harness that flakes under load
/// is worse than one that waits.
const FIRST_FRAME_BUDGET: Duration = Duration::from_secs(120);
