//! Atomic publication of a fully validated runtime-layout candidate.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use phoxal_cli_core::project::launch_plan::runtime_layout_dir;
use phoxal_cli_core::project::resolver::ResolvedRobot;

use super::candidate::StagedCandidate;

const PREVIOUS_LAYOUT_SUFFIX: &str = ".previous";

/// The staged runtime layout directory for this resolved robot under
/// `project_root`. `run`, live simulation, and `build` all stage and execute
/// this one root.
#[must_use]
pub(crate) fn layout_path(project_root: &Path, _resolved: &ResolvedRobot) -> PathBuf {
    runtime_layout_dir(project_root)
}

/// Atomically publish `candidate` as the live `.phoxal/bundle/`, replacing
/// any previous layout. Call this ONLY after every install, source build,
/// metadata read, and loader validation against `candidate.path()` has
/// already succeeded - this is the exact promise the module docs make, and
/// the only step allowed to touch the live path.
pub(crate) fn publish_runtime_layout(
    candidate: StagedCandidate,
    resolved: &ResolvedRobot,
) -> Result<PathBuf> {
    let StagedCandidate {
        dir: candidate,
        project_root,
    } = candidate;
    let target = layout_path(&project_root, resolved);
    let parent = target
        .parent()
        .context("runtime bundle directory has no parent")?;
    let previous = parent.join(format!(".bundle{PREVIOUS_LAYOUT_SUFFIX}"));
    remove_if_present(&previous)?;
    let candidate = candidate.keep();
    let had_previous = fs::symlink_metadata(&target).is_ok();
    if had_previous {
        fs::rename(&target, &previous).with_context(|| {
            format!(
                "failed to move previous runtime layout {} aside",
                target.display()
            )
        })?;
    }

    if let Err(error) = fs::rename(&candidate, &target) {
        if had_previous {
            let _ = fs::rename(&previous, &target);
        }
        let _ = remove_if_present(&candidate);
        return Err(error).with_context(|| {
            format!(
                "failed to atomically publish runtime layout {}",
                target.display()
            )
        });
    }
    remove_if_present(&previous)?;
    Ok(target)
}

pub(super) fn remove_if_present(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    };
    if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
    .with_context(|| format!("failed to remove stale runtime state {}", path.display()))
}
