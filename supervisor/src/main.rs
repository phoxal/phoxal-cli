//! `phoxald` - the Phoxal execution supervisor.
//!
//! ```text
//! phoxald <BUNDLE_ROOT>
//! ```
//!
//! That is the entire command line, and deliberately so (organization#978).
//! There is no `run`, `start`, `attach`, `stop`, `status`, `log`, `build`,
//! `install`, `deploy`, `doctor`, or `upgrade` subcommand; no `--drivers`,
//! `--driver`, or simulation flag; and no execution options of any kind. Clock
//! and participant selection are already written into the finalized manifest by
//! whoever built the bundle, so the bundle root is the daemon's complete input.
//!
//! Everything an operator does *to* a running execution goes through the
//! supervisor API on the bus - `phoxal attach`, `phoxal status`, `phoxal stop`
//! - not through a second invocation of this binary.

use std::path::PathBuf;
use std::process::ExitCode;

use phoxal_cli_supervisor::daemon;
use tracing_subscriber::EnvFilter;

/// Multi-thread: Zenoh refuses to run on Tokio's current-thread scheduler, and
/// the router runs in this process.
#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();
    let root = match bundle_root() {
        Invocation::Run(root) => root,
        Invocation::Usage => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Invocation::Misuse => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match daemon::run(&root).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // Stderr is the daemon's diagnostic channel under systemd, where it
            // is the journal. One rendered chain, not a panic.
            eprintln!("phoxald: {error:#}");
            ExitCode::from(1)
        }
    }
}

/// What this invocation asked for.
#[derive(Debug, Eq, PartialEq)]
enum Invocation {
    Run(PathBuf),
    /// Help was asked for, which is a successful invocation.
    Usage,
    /// Anything else: no operand, several operands, or a flag this binary does
    /// not have.
    Misuse,
}

/// Parse the one argument by hand.
///
/// A derive-based parser would advertise a surface this binary does not have -
/// options to list, a version to print, subcommands to suggest - and the point
/// of this entry point is that there is nothing to choose.
fn bundle_root() -> Invocation {
    parse(std::env::args_os().skip(1))
}

fn parse(arguments: impl IntoIterator<Item = std::ffi::OsString>) -> Invocation {
    let mut arguments = arguments.into_iter();
    match (arguments.next(), arguments.next()) {
        (Some(root), None) if matches!(root.to_string_lossy().as_ref(), "-h" | "--help") => {
            Invocation::Usage
        }
        (Some(root), None) if !root.to_string_lossy().starts_with('-') => {
            Invocation::Run(PathBuf::from(root))
        }
        _ => Invocation::Misuse,
    }
}

const USAGE: &str = "\
phoxald - the Phoxal execution supervisor

Usage:
    phoxald <BUNDLE_ROOT>

<BUNDLE_ROOT> is a finalized bundle directory: robot.yaml, assets/, and bin/.
Build one with `phoxal build`. The daemon validates and executes it; it never
builds, and it takes no other options - the clock and the participant set are
already written into the bundle's robot.yaml.";

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // Under systemd, stderr is the journal; interactively it is the terminal
    // the operator launched from. Either way it is the only diagnostic channel
    // the daemon has before the bus exists.
    // `JOURNAL_STREAM` means stderr is the journal, which does not render
    // escape codes - only a real terminal gets colour.
    let ansi = std::env::var_os("JOURNAL_STREAM").is_none()
        && std::io::IsTerminal::is_terminal(&std::io::stderr());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(ansi)
        .init();
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::{Invocation, parse};

    fn arguments<const N: usize>(values: [&str; N]) -> Vec<OsString> {
        values.into_iter().map(OsString::from).collect()
    }

    /// One operand and nothing else. Every subcommand and flag the old resident
    /// entry point carried is gone, so anything shaped like one is a misuse
    /// rather than something this binary quietly ignores.
    #[test]
    fn exactly_one_bundle_root_runs_and_everything_else_is_a_misuse() {
        assert_eq!(
            parse(arguments([".phoxal/bundle"])),
            Invocation::Run(".phoxal/bundle".into())
        );
        assert_eq!(parse(arguments(["-h"])), Invocation::Usage);
        assert_eq!(parse(arguments(["--help"])), Invocation::Usage);

        for misuse in [
            vec![],
            arguments(["one", "two"]),
            arguments(["--drivers", "off"]),
            arguments(["--offline"]),
            arguments([".phoxal/bundle", "--drivers=off"]),
        ] {
            assert_eq!(parse(misuse.clone()), Invocation::Misuse, "{misuse:?}");
        }

        // There are no subcommands to shadow an operand, so a directory that
        // happens to be named `run` is a bundle root like any other.
        assert_eq!(parse(arguments(["run"])), Invocation::Run("run".into()));
    }
}
