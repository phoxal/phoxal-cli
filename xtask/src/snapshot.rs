//! Screen snapshots, owned rather than depended on.
//!
//! This is deliberately a handful of lines instead of a snapshot library:
//! the whole contract is "compare this text to that file, or rewrite it under
//! `--bless`", and owning it keeps the harness free of a testing framework it
//! would otherwise have to be bent around.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub struct Snapshots {
    root: PathBuf,
    bless: bool,
    mismatches: Vec<String>,
    written: usize,
    matched: usize,
}

impl Snapshots {
    pub fn new(root: PathBuf, bless: bool) -> Self {
        Self {
            root,
            bless,
            mismatches: Vec::new(),
            written: 0,
            matched: 0,
        }
    }

    /// Compare `screen` against the snapshot named `name`.
    ///
    /// A missing snapshot is recorded rather than failed: the first run of a
    /// new scenario should produce something to read, not a wall of errors.
    pub fn check(&mut self, name: &str, screen: &str) -> Result<()> {
        let path = self.root.join(format!("{name}.txt"));
        let recorded = std::fs::read_to_string(&path).ok();
        let screen = format!("{}\n", screen.trim_end());

        match recorded {
            Some(recorded) if recorded == screen => {
                self.matched += 1;
                println!("  ok       {name}");
            }
            Some(recorded) if self.bless => {
                write(&path, &screen)?;
                self.written += 1;
                println!(
                    "  blessed  {name} ({} bytes -> {})",
                    recorded.len(),
                    screen.len()
                );
            }
            Some(recorded) => {
                self.mismatches.push(name.to_string());
                println!("  CHANGED  {name}");
                println!("{}", diff(&recorded, &screen));
            }
            None => {
                write(&path, &screen)?;
                self.written += 1;
                println!("  new      {name} (recorded {} bytes)", screen.len());
            }
        }
        Ok(())
    }

    /// Report the run. Changed snapshots fail; new ones do not.
    pub fn finish(self) -> Result<()> {
        println!(
            "\n{} matched, {} written, {} changed",
            self.matched,
            self.written,
            self.mismatches.len()
        );
        anyhow::ensure!(
            self.mismatches.is_empty(),
            "screen changed: {}. Review the diffs above, then re-run with --bless to accept.",
            self.mismatches.join(", ")
        );
        Ok(())
    }
}

fn write(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))
}

/// A line-oriented diff, enough to see what moved on a screen.
fn diff(recorded: &str, actual: &str) -> String {
    let recorded = recorded.lines().collect::<Vec<_>>();
    let actual = actual.lines().collect::<Vec<_>>();
    let mut out = String::new();
    for row in 0..recorded.len().max(actual.len()) {
        match (recorded.get(row), actual.get(row)) {
            (Some(before), Some(after)) if before == after => {}
            (before, after) => {
                if let Some(before) = before {
                    out.push_str(&format!("    -{row:>3} |{before}\n"));
                }
                if let Some(after) = after {
                    out.push_str(&format!("    +{row:>3} |{after}\n"));
                }
            }
        }
    }
    out
}
