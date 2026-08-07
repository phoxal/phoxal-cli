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
//! and `phoxald` ship as one exact pair: the client probes
//! its sibling with it and refuses to build anything for a missing or
//! mismatched daemon. It is a pair check, not an execution option.
//!
//! Everything an operator does *to* a running execution goes through the
//! supervisor API on the bus - `phoxal attach`, `phoxal status`, `phoxal stop`
//! - not through a second invocation of this binary.

#![allow(clippy::module_name_repetitions)]

use std::time::{Duration, Instant};

// Four bytes per character preserves the observation crate's 4,096-character
// routed-log bound even for maximum-width UTF-8 input.
const MAX_CAPTURED_LINE_BYTES: usize = 16 * 1024;

/// A readiness/stage-wait budget for one supervised session.
///
/// This is explicit rather than a large duration sentinel: an interactive
/// operator may wait indefinitely, while a headless invocation needs a
/// deterministic deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitBudget {
    /// No deadline is applied.
    Unbounded,
    /// Fail after the supplied duration.
    Bounded(Duration),
}

impl Default for WaitBudget {
    /// Default to an already-elapsed bounded wait so a derived default can
    /// never silently turn a missing policy into an unbounded wait.
    fn default() -> Self {
        Self::Bounded(Duration::default())
    }
}

impl WaitBudget {
    /// Resolve this policy into the absolute deadline used by a wait loop.
    #[must_use]
    pub fn deadline_from(self, now: Instant) -> Option<Instant> {
        match self {
            Self::Unbounded => None,
            Self::Bounded(duration) => Some(now + duration),
        }
    }
}

mod daemon;
mod process;
mod router;
mod state;
mod systemd;

pub(crate) use phoxal_cli_core::runtime::format_duration;
pub(crate) use phoxal_cli_core::runtime::{ParticipantSpec, ProcessState};

pub(crate) use process::child::ManagedChild;
pub(crate) use process::spec::{SupervisorAction, SupervisorActionReceiver, SupervisorOptions};
pub(crate) use process::stages::{SupervisionStage, stages_for_run};
pub(crate) use process::supervise::supervise_until_shutdown;
pub(crate) use router::{RouterLost, start_embedded_router};
pub(crate) use state::SupervisorState;

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

/// What `--version` prints. `phoxal` parses exactly this to decide whether the
/// installed pair is exact.
const VERSION_LINE: &str = concat!("phoxald ", env!("CARGO_PKG_VERSION"));

const USAGE: &str = "\
phoxald - the Phoxal execution supervisor

Usage:
    phoxald <BUNDLE_ROOT>
    phoxald --version

<BUNDLE_ROOT> is a finalized bundle directory: robot.yaml, assets/, and bin/.
Build one with `phoxal build`. The daemon validates and executes it; it never
builds, and it takes no other options - the clock and the participant set are
already written into the bundle's robot.yaml.

`--version` reports this daemon's version so `phoxal` can confirm the two ship
as the exact pair they must be.";

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
        assert_eq!(parse(arguments(["-V"])), Invocation::Version);
        assert_eq!(parse(arguments(["--version"])), Invocation::Version);

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

    /// `phoxal` parses this exact line to decide whether the installed pair is
    /// exact, so its shape is a contract between the two binaries
    ///.
    #[test]
    fn the_version_line_is_the_pair_probes_contract() {
        assert_eq!(
            VERSION_LINE,
            format!("phoxald {}", env!("CARGO_PKG_VERSION"))
        );
    }
}
