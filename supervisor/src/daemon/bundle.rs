//! The daemon's one input: a finalized bundle root.
//!
//! `phoxald <BUNDLE_ROOT>` takes a canonical finalized bundle root and nothing
//! else. Everything a source project needs - discovery,
//! `extends` composition, Cargo, registries, staging - happened in `phoxal`
//! before the bundle existed, so the daemon's job here is to refuse anything
//! that is not already a bundle, and to say precisely what it was handed
//! instead.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use phoxal_cli_core::project::layout::{ASSETS_DIR, BIN_DIR, ROBOT_FILE};

/// The `.phoxal/bundle` suffix a project-local bundle sits at, relative to the
/// project root that owns its volatile run directory.
const PROJECT_BUNDLE_SUFFIX: [&str; 2] = [".phoxal", "bundle"];

/// Canonicalize `requested` and require it to be a finalized bundle root.
///
/// The three shapes an operator actually reaches for - a source project, a
/// `build.phoxal` archive, and a directory that is neither - are each named
/// rather than collapsed into "not a bundle".
pub(crate) fn require_finalized_bundle_root(requested: &Path) -> Result<PathBuf> {
    let metadata = std::fs::symlink_metadata(requested).with_context(|| {
        format!(
            "phoxald takes a finalized bundle root; {} does not exist",
            requested.display()
        )
    })?;
    if !requested.is_dir() {
        if metadata.file_type().is_file() {
            bail!(
                "{} is a file, not a finalized bundle root; phoxald never extracts or builds - \
                 install or extract the archive first and pass the resulting bundle directory",
                requested.display()
            );
        }
        bail!(
            "{} is not a directory, so it cannot be a finalized bundle root",
            requested.display()
        );
    }
    let root = requested.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize the bundle root {}",
            requested.display()
        )
    })?;

    let robot = root.join(ROBOT_FILE).is_file();
    let assets = root.join(ASSETS_DIR).is_dir();
    let bin = root.join(BIN_DIR).is_dir();
    if robot && assets && bin {
        return Ok(root);
    }
    if robot {
        // A source project has the authored document and no staged output at
        // all, which is the one wrong input worth naming a fix for.
        bail!(
            "{} carries {ROBOT_FILE} but no finalized bundle ({} missing): phoxald validates and \
             executes, it never builds - run `phoxal build` and start the daemon on the published \
             bundle root",
            root.display(),
            missing(assets, bin),
        );
    }
    bail!(
        "{} is not a finalized bundle root: a bundle carries {ROBOT_FILE}, {ASSETS_DIR}/, and \
         {BIN_DIR}/",
        root.display(),
    )
}

fn missing(assets: bool, bin: bool) -> String {
    match (assets, bin) {
        (false, false) => format!("{ASSETS_DIR}/ and {BIN_DIR}/"),
        (false, true) => format!("{ASSETS_DIR}/"),
        (true, false) => format!("{BIN_DIR}/"),
        (true, true) => String::new(),
    }
}

/// The root whose volatile run directory owns this bundle's locks, socket, and
/// log.
///
/// A project-local bundle lives at `<project>/.phoxal/bundle`, so its owner is
/// the project root; an installed release directory owns itself, and
/// `RuntimePaths` maps it onto the installed FHS paths.
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

    fn finalized(root: &Path) {
        std::fs::write(root.join(ROBOT_FILE), "schema: phoxal/robot/v0\n").expect("robot.yaml");
        std::fs::create_dir_all(root.join(ASSETS_DIR)).expect("assets");
        std::fs::create_dir_all(root.join(BIN_DIR)).expect("bin");
    }

    #[test]
    fn a_finalized_bundle_root_is_accepted_and_canonicalized() {
        let dir = tempfile::tempdir().expect("temp dir");
        finalized(dir.path());
        let accepted = require_finalized_bundle_root(&dir.path().join("."))
            .expect("a finalized bundle root is accepted");
        assert_eq!(accepted, dir.path().canonicalize().expect("canonical"));
    }

    #[test]
    fn a_source_project_is_refused_by_name() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join(ROBOT_FILE), "schema: phoxal/robot/v0\n")
            .expect("robot.yaml");
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").expect("Cargo.toml");
        let error = require_finalized_bundle_root(dir.path())
            .expect_err("a source project is not a daemon input");
        let rendered = format!("{error:#}");
        assert!(rendered.contains("phoxal build"), "{rendered}");
        assert!(rendered.contains("assets/ and bin/"), "{rendered}");
    }

    #[test]
    fn a_partial_bundle_names_exactly_what_is_missing() {
        let dir = tempfile::tempdir().expect("temp dir");
        finalized(dir.path());
        std::fs::remove_dir_all(dir.path().join(BIN_DIR)).expect("remove bin");
        let error = require_finalized_bundle_root(dir.path())
            .expect_err("a bundle without bin/ is not one");
        let rendered = format!("{error:#}");
        assert!(rendered.contains("bin/ missing"), "{rendered}");
        assert!(!rendered.contains("assets/ and"), "{rendered}");
    }

    #[test]
    fn an_archive_and_an_unrelated_directory_are_refused_distinctly() {
        let dir = tempfile::tempdir().expect("temp dir");
        let archive = dir.path().join("build.phoxal");
        std::fs::write(&archive, b"not a directory").expect("archive");
        let error = require_finalized_bundle_root(&archive).expect_err("an archive is not a root");
        assert!(format!("{error:#}").contains("never extracts"), "{error:#}");

        let unrelated = dir.path().join("elsewhere");
        std::fs::create_dir_all(&unrelated).expect("directory");
        let error = require_finalized_bundle_root(&unrelated)
            .expect_err("an empty directory is not a root");
        assert!(
            format!("{error:#}").contains("not a finalized bundle root"),
            "{error:#}"
        );

        let absent = dir.path().join("nowhere");
        let error = require_finalized_bundle_root(&absent).expect_err("a missing path is refused");
        assert!(format!("{error:#}").contains("does not exist"), "{error:#}");
    }

    #[test]
    fn a_project_local_bundle_is_owned_by_its_project_root() {
        assert_eq!(
            owning_root(Path::new("/work/rover/.phoxal/bundle")),
            Path::new("/work/rover")
        );
        // An installed release directory owns itself; RuntimePaths maps it onto
        // the installed FHS volatile root from there.
        assert_eq!(
            owning_root(Path::new("/var/lib/phoxal/releases/20260806T000000Z-abc")),
            Path::new("/var/lib/phoxal/releases/20260806T000000Z-abc")
        );
    }
}
