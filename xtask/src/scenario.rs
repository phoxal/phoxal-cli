//! What the harness drives.
//!
//! A scenario is a launch plus the marker that says the first usable frame has
//! arrived. Waiting on a *marker* rather than a fixed sleep is what keeps the
//! snapshots stable: the screen is read when the TUI says it is ready, not
//! when a timer guessed it might be.

use std::path::Path;
use std::time::Duration;

use anyhow::{Result, bail};

use crate::FIRST_FRAME_BUDGET;
use crate::pty::{Session, TerminalSize};
use crate::resident::Resident;

pub struct Scenario {
    pub name: &'static str,
    pub description: &'static str,
    /// Whether this scenario drives a real robot project.
    pub needs_project: bool,
    /// Whether a resident must be running for this scenario to mean anything.
    /// The TUI only exists against one.
    pub needs_resident: bool,
    args: &'static [&'static str],
    /// Text that must appear before the screen is worth reading.
    ready_marker: &'static str,
}

impl Scenario {
    /// Bring up whatever this scenario attaches to.
    ///
    /// Returned rather than started inside [`Self::launch`] so one resident
    /// serves a whole terminal matrix: starting a robot per screen would make
    /// the matrix cost minutes for nothing.
    pub fn prepare(&self, binary: &Path, project: &Path) -> Result<Option<Resident>> {
        if self.needs_resident {
            Ok(Some(Resident::start(binary, project)?))
        } else {
            Ok(None)
        }
    }

    /// Start the scenario and return once its first usable frame is up.
    pub fn launch(&self, binary: &Path, project: &Path, size: TerminalSize) -> Result<Session> {
        let args = self
            .args
            .iter()
            .map(|arg| (*arg).to_string())
            .collect::<Vec<_>>();
        let mut session = Session::spawn(binary, &args, project, size)?;
        session.wait_for(self.ready_marker, FIRST_FRAME_BUDGET)?;
        // One more settle so a marker that arrives mid-paint does not snapshot
        // a half-drawn frame.
        session.settle(Duration::from_millis(250), Duration::from_secs(2))?;
        Ok(session)
    }
}

pub const ALL: &[Scenario] = &[
    Scenario {
        name: "attach",
        description: "the TUI attached to a live robot - the main event",
        needs_project: true,
        needs_resident: true,
        args: &["attach"],
        ready_marker: "Runtimes",
    },
    Scenario {
        name: "attach-no-resident",
        description: "attaching to nothing: the error a user gets, not a TUI",
        needs_project: true,
        needs_resident: false,
        args: &["attach"],
        ready_marker: "project is not running",
    },
    Scenario {
        name: "version",
        description: "the non-TUI baseline that proves the harness itself works",
        needs_project: false,
        needs_resident: false,
        args: &["--version"],
        ready_marker: "phoxal",
    },
];

pub fn find(name: &str) -> Result<&'static Scenario> {
    match ALL.iter().find(|scenario| scenario.name == name) {
        Some(scenario) => Ok(scenario),
        None => bail!(
            "unknown scenario `{name}`. Known: {}",
            ALL.iter()
                .map(|scenario| scenario.name)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}
