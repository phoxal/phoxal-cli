//! `phoxald` - the Phoxal execution supervisor.
//!
//! ```text
//! phoxald <BUNDLE_ROOT>
//! ```
//!
//! That is the entire command line, and deliberately so.
//! There is no `run`, `start`, `attach`, `stop`, `status`, `log`, `build`,
//! `install`, `deploy`, `doctor`, or `upgrade` subcommand; no `--drivers`,
//! `--driver`, or simulation flag; and no execution options of any kind. Clock
//! and participant selection are already written into the finalized manifest by
//! whoever built the bundle, so the bundle root is the daemon's complete input.
//!
//! The one non-executing invocation is `--version`. It exists because `phoxal`
//! and `phoxald` ship as one archive: the client probes its sibling with it to
//! report whether that installation is whole. It is an installation check, not
//! an execution option, and not a compatibility gate - what a bundle is
//! compatible with is the framework train its artifacts carry, which this
//! daemon reads from the bundle itself.
//!
//! Everything an operator does *to* a running execution goes through the
//! supervisor API on the bus - `phoxal attach`, `phoxal status`, `phoxal stop`
//! - not through a second invocation of this binary.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::todo,
        clippy::unimplemented
    )
)]

mod daemon;
mod model;
mod process;
mod router;
mod state;
mod systemd;

use std::path::PathBuf;
use std::process::ExitCode;

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
        Invocation::Version => {
            println!("{VERSION_LINE}");
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
    /// The pair check: print `phoxald <version>` and exit.
    Version,
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
        (Some(root), None) if matches!(root.to_string_lossy().as_ref(), "-V" | "--version") => {
            Invocation::Version
        }
        (Some(root), None) if !root.to_string_lossy().starts_with('-') => {
            Invocation::Run(PathBuf::from(root))
        }
        _ => Invocation::Misuse,
    }
}

/// What `--version` prints. The `phoxal` installed beside this daemon parses
/// exactly this line to report whether its own CLI installation is whole, so
/// the shape is a contract between two halves of one archive. It says nothing
/// about any other machine: a robot and a client agree on the framework
/// compatibility line, never on a product version.
const VERSION_LINE: &str = concat!("phoxald ", env!("CARGO_PKG_VERSION"));

const USAGE: &str = "\
phoxald - the Phoxal execution supervisor

Usage:
    phoxald <BUNDLE_ROOT>
    phoxald --version

<BUNDLE_ROOT> is a compiled bundle directory: runtime.json, assets/, and bin/.
Build one with `phoxal build`. The daemon validates and executes it; it never
builds, and it takes no other options - the clock and the participant set are
already written into runtime.json.

`--version` reports this daemon's own version. The `phoxal` installed beside it
reads that line to report whether its installation is whole; nothing else
compares product versions.";

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

    use super::{Invocation, VERSION_LINE, parse};

    fn arguments<const N: usize>(values: [&str; N]) -> Vec<OsString> {
        values.into_iter().map(OsString::from).collect()
    }

    /// One operand and nothing else. Every subcommand and obsolete flag
    /// entry point carried is gone, so anything shaped like one is a misuse
    /// rather than something this binary quietly ignores.
    #[test]
    fn exactly_one_bundle_root_runs_and_everything_else_is_a_misuse() {
        assert_eq!(
            parse(arguments([".phoxal/release/bundle"])),
            Invocation::Run(".phoxal/release/bundle".into())
        );
        assert_eq!(parse(arguments(["-h"])), Invocation::Usage);
        assert_eq!(parse(arguments(["--help"])), Invocation::Usage);
        assert_eq!(parse(arguments(["-V"])), Invocation::Version);
        assert_eq!(parse(arguments(["--version"])), Invocation::Version);

        for misuse in [
            vec![],
            arguments(["one", "two"]),
            arguments(["--drivers", "off"]),
            arguments(["--offline"]),
            arguments([".phoxal/release/bundle", "--drivers=off"]),
        ] {
            assert_eq!(parse(misuse.clone()), Invocation::Misuse, "{misuse:?}");
        }

        // There are no subcommands to shadow an operand, so a directory that
        // happens to be named `run` is a bundle root like any other.
        assert_eq!(parse(arguments(["run"])), Invocation::Run("run".into()));
    }

    /// The `phoxal` beside this daemon parses this exact line to report
    /// whether its own installation is whole, so the shape is a contract
    /// between the two halves of one archive.
    #[test]
    fn the_version_line_is_the_sibling_probes_contract() {
        assert_eq!(
            VERSION_LINE,
            format!("phoxald {}", env!("CARGO_PKG_VERSION"))
        );
    }
}
