//! Loading the daemon's sole persisted input.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use phoxal_bundle::RuntimeBundle;

const PROJECT_BUNDLE_SUFFIX: [&str; 2] = [".phoxal", "bundle"];

/// Open and integrity-check the canonical `runtime.json + assets/ + bin/`
/// boundary. Authored source documents are never consulted here.
pub(crate) fn open(requested: &Path) -> Result<RuntimeBundle> {
    let metadata = std::fs::symlink_metadata(requested).with_context(|| {
        format!(
            "phoxald takes a compiled runtime bundle; {} does not exist",
            requested.display()
        )
    })?;
    if !requested.is_dir() {
        if metadata.file_type().is_file() {
            bail!(
                "{} is a file, not a runtime bundle; install or extract it first",
                requested.display()
            );
        }
        bail!("{} is not a runtime bundle directory", requested.display());
    }
    RuntimeBundle::open_verified(requested).with_context(|| {
        format!(
            "{} is not a valid compiled runtime bundle",
            requested.display()
        )
    })
}

/// The root whose volatile run directory owns this bundle.
pub(crate) fn owning_root(bundle_root: &Path) -> PathBuf {
    let mut components = bundle_root.components().rev();
    let tail: Vec<_> = components
        .by_ref()
        .take(PROJECT_BUNDLE_SUFFIX.len())
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect();
    let expected: Vec<_> = PROJECT_BUNDLE_SUFFIX
        .iter()
        .rev()
        .map(ToString::to_string)
        .collect();
    if tail == expected {
        return components.rev().collect::<PathBuf>();
    }
    bundle_root.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_source_directory_is_not_a_runtime_bundle() {
        let dir = tempfile::tempdir().expect("temporary directory");
        std::fs::write(dir.path().join("robot.yaml"), "schema: phoxal/robot/v0\n")
            .expect("source fixture");
        let error = open(dir.path()).expect_err("authored YAML is not runtime authority");
        assert!(format!("{error:#}").contains("runtime.json"));
    }

    #[test]
    fn a_project_local_bundle_is_owned_by_its_project_root() {
        assert_eq!(
            owning_root(Path::new("/work/rover/.phoxal/bundle")),
            Path::new("/work/rover")
        );
        assert_eq!(
            owning_root(Path::new("/var/lib/phoxal/releases/current")),
            Path::new("/var/lib/phoxal/releases/current")
        );
    }
}
