//! Atomic publication of a fully validated deployment-release candidate.

use std::fs;
use std::path::{Path, PathBuf};

use super::candidate::StagedCandidate;
use anyhow::{Context, Result};

const PREVIOUS_LAYOUT_SUFFIX: &str = ".previous";

/// The staged deployment release under `project_root`. `run`, live simulation,
/// and `build` all stage and execute this one root - it is a function of the
/// project alone, not of what was resolved into it.
#[must_use]
pub(crate) fn layout_path(project_root: &Path) -> PathBuf {
    crate::paths::runtime::runtime_release_root(project_root)
}

/// Atomically publish `candidate` as the live `.phoxal/release/`, replacing any
/// previous release. Call this ONLY after every install, source build, metadata
/// read, loader validation, and the release step that adds the executor have
/// already succeeded against the candidate - this is the exact promise the
/// module docs make, and the only step allowed to touch the live path. The
/// executor and the bundle therefore change together or not at all.
pub(crate) fn publish_runtime_layout(candidate: StagedCandidate) -> Result<PathBuf> {
    let StagedCandidate {
        dir: candidate,
        bundle: _,
        project_root,
    } = candidate;
    let target = layout_path(&project_root);
    let parent = target
        .parent()
        .context("deployment release directory has no parent")?;
    let previous = parent.join(format!(".release{PREVIOUS_LAYOUT_SUFFIX}"));
    remove_if_present(&previous)?;
    let candidate = candidate.keep();
    let had_previous = fs::symlink_metadata(&target).is_ok();
    if had_previous {
        fs::rename(&target, &previous).with_context(|| {
            format!(
                "failed to move the previous deployment release {} aside",
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
                "failed to atomically publish the deployment release {}",
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
