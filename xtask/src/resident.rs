//! The robot a TUI scenario attaches to.
//!
//! The TUI only exists against a running resident, so a scenario that wants to
//! render it has to start one. That is host state and it is slow, which is
//! exactly why this lives in a developer tool rather than in `cargo test`.
//!
//! Drivers are off: this harness runs wherever a developer is, which is not
//! on the robot, and a scenario that only passes next to real hardware is a
//! scenario nobody runs.

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, bail};

/// A detached resident, stopped when this drops so a failed scenario cannot
/// leave a robot running behind it.
pub struct Resident {
    binary: std::path::PathBuf,
    project: std::path::PathBuf,
}

impl Resident {
    /// Start a resident and return once its graph is up.
    pub fn start(binary: &Path, project: &Path) -> Result<Self> {
        // A resident left over from an earlier run would make the next scenario
        // attach to the wrong robot, so clear the ground first.
        let _ = Command::new(binary)
            .args(["stop"])
            .current_dir(project)
            .output();

        println!(
            "  starting a resident in {} (drivers off)…",
            project.display()
        );
        let started = Instant::now();
        let output = Command::new(binary)
            .args(["run", "--detach", "--drivers", "off"])
            .current_dir(project)
            .output()
            .context("failed to run `phoxal run --detach`")?;

        if !output.status.success() {
            bail!(
                "the resident did not start ({}):\n{}{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
        println!("  resident ready in {:?}", started.elapsed());
        Ok(Self {
            binary: binary.to_path_buf(),
            project: project.to_path_buf(),
        })
    }
}

impl Drop for Resident {
    fn drop(&mut self) {
        let stopped = Command::new(&self.binary)
            .args(["stop"])
            .current_dir(&self.project)
            .output();
        match stopped {
            Ok(output) if output.status.success() => println!("  resident stopped"),
            Ok(output) => eprintln!(
                "  WARNING: `phoxal stop` failed ({}); a resident may still be running in {}",
                output.status,
                self.project.display()
            ),
            Err(error) => eprintln!(
                "  WARNING: could not run `phoxal stop` ({error}); a resident may still be \
                 running in {}",
                self.project.display()
            ),
        }
    }
}
