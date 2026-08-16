//! The deployment release: one bundle and the framework supervisor that runs it.
//!
//! A deployment release is what this CLI stages, archives, installs, activates,
//! and rolls back. It holds exactly two entries:
//!
//! ```text
//! phoxal-supervisor    the framework supervisor this release is run by
//! bundle/              manifest.json, assets/, bin/
//! ```
//!
//! Packaging the supervisor beside the bundle is what makes install and rollback
//! atomic over both halves: activation switches one directory, so a supervisor
//! from one release can never end up running another release's bundle.
//!
//! The layout is a fixed convention rather than another descriptor file.
//! Integrity lives in the archive - `phoxal build` writes `build.phoxal`
//! alongside its `build.phoxal.sha256` and `phoxal install` refuses a mismatch -
//! so once a release is on disk it is trusted. Validation here checks only the
//! release shape: both entries are present, the supervisor is executable, and
//! nothing else exists at the root.
//!
//! The supervisor knows nothing of any of this packaging. It takes a bundle root and
//! nothing else; the release is the CLI's packaging around it.

use anyhow::{Context, Result, bail, ensure};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::check::participant_metadata::{ExpectedTarget, ensure_target};

/// The framework supervisor's file name inside a release.
pub const SUPERVISOR_FILE: &str = "phoxal-supervisor";

/// The bundle directory's name inside a release.
pub const BUNDLE_DIR: &str = "bundle";

/// One verified release on disk, resolved to the paths its two halves live at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseLayout {
    pub root: PathBuf,
    pub supervisor: PathBuf,
    pub bundle: PathBuf,
}

/// Complete a release whose `bundle/` is already staged at `root` by copying
/// `supervisor_source` in as the release's framework supervisor.
///
/// The supervisor is proven to be an object file for `expected` before it is
/// copied, so a cross-target release can never package this host's own supervisor,
/// and a wrong-architecture supervisor is refused here rather than by an exec
/// failure on the device.
///
/// # Errors
///
/// When the supervisor cannot be read, is not built for `expected`, cannot be
/// written, or the completed release does not validate.
pub(crate) fn materialize(
    root: &Path,
    supervisor_source: &Path,
    expected: &ExpectedTarget,
) -> Result<ReleaseLayout> {
    let bytes = fs::read(supervisor_source).with_context(|| {
        format!(
            "failed to read the supervisor {} this release must carry",
            supervisor_source.display()
        )
    })?;
    ensure_target(&bytes, &supervisor_source.display().to_string(), expected).with_context(
        || {
            format!(
                "{} cannot execute this release on its target",
                supervisor_source.display()
            )
        },
    )?;
    let supervisor = root.join(SUPERVISOR_FILE);
    fs::write(&supervisor, &bytes)
        .with_context(|| format!("failed to write {}", supervisor.display()))?;
    make_executable(&supervisor)?;
    validate_layout(root)
}

/// Validate a whole deployment release at `root`: the exact release layout, an
/// executable supervisor, and the bundle's own content.
///
/// # Errors
///
/// When either half is missing, something unexpected is at the root, or the
/// bundle does not verify.
pub fn validate_release(root: &Path) -> Result<ReleaseLayout> {
    let layout = validate_layout(root)?;
    phoxal_bundle::RuntimeBundle::open(&layout.bundle).with_context(|| {
        format!(
            "failed to read the bundle {} this release executes",
            layout.bundle.display()
        )
    })?;
    Ok(layout)
}

/// Whether `root` looks like a deployment release at all. The supervisor's
/// presence is the whole test: a release is the pair, so the half that a bundle
/// directory can never provide is what identifies one.
#[must_use]
pub fn is_release_root(root: &Path) -> bool {
    root.join(SUPERVISOR_FILE).is_file()
}

/// Refuse a bundle handed in where a deployment release belongs.
///
/// A `build.phoxal` written before releases owned their supervisor is a bundle
/// at its own root: it would install as one, and the installed unit would then
/// execute it with whatever supervisor the host happened to have. That is
/// exactly the drift releases exist to remove, so the old shape is refused by
/// name rather than adapted.
///
/// # Errors
///
/// When `root` is a bundle root rather than a release root.
pub fn refuse_bundle_shaped_release(root: &Path) -> Result<()> {
    ensure!(
        !root.join(phoxal_bundle::MANIFEST_FILE).is_file(),
        "{} is a bundle, not a deployment release: it carries {} at its root instead of \
         `{SUPERVISOR_FILE}` and `{BUNDLE_DIR}/`. This archive predates supervisor-owning releases, \
         where a release carries the supervisor that runs it; rebuild it with this CLI (`phoxal \
         build`) and install the result",
        root.display(),
        phoxal_bundle::MANIFEST_FILE,
    );
    Ok(())
}

/// The release's layout, without opening the bundle. Callers that verify the
/// bundle their own way (against a declared target signature, say) use this and
/// then verify it themselves.
fn validate_layout(root: &Path) -> Result<ReleaseLayout> {
    refuse_bundle_shaped_release(root)?;
    require_exact_release_entries(root)?;
    let supervisor = root.join(SUPERVISOR_FILE);
    require_executable(&supervisor)?;
    Ok(ReleaseLayout {
        root: root.to_path_buf(),
        supervisor,
        bundle: root.join(BUNDLE_DIR),
    })
}

/// The release root admits exactly the supervisor and the bundle directory - the
/// same strictness the runtime bundle applies to its own root. A release is its
/// layout, so anything else there is content nobody put in deliberately.
fn require_exact_release_entries(root: &Path) -> Result<()> {
    let mut present = BTreeSet::new();
    let entries = fs::read_dir(root)
        .with_context(|| format!("failed to read the release directory {}", root.display()))?;
    for entry in entries {
        let entry =
            entry.with_context(|| format!("failed to read an entry in {}", root.display()))?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_str().with_context(|| {
            format!(
                "release entry {} does not have a UTF-8 name",
                path.display()
            )
        })?;
        let is_dir = fs::symlink_metadata(&path)
            .with_context(|| format!("failed to inspect {}", path.display()))?
            .is_dir();
        if name == SUPERVISOR_FILE {
            ensure!(!is_dir, "{} must be a file", path.display());
        } else if name == BUNDLE_DIR {
            ensure!(is_dir, "{} must be a directory", path.display());
        } else {
            bail!(
                "{} does not belong to a deployment release: a release holds exactly \
                 `{SUPERVISOR_FILE}` and `{BUNDLE_DIR}/`",
                path.display()
            );
        }
        present.insert(name.to_string());
    }
    for required in [SUPERVISOR_FILE, BUNDLE_DIR] {
        ensure!(
            present.contains(required),
            "{} is not a complete deployment release: `{required}` is missing",
            root.display()
        );
    }
    Ok(())
}

/// A release's supervisor has to be runnable, or activating the release would
/// leave a unit that cannot start.
fn require_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to inspect the supervisor {}", path.display()))?;
    ensure!(
        metadata.permissions().mode() & 0o111 != 0,
        "the supervisor {} is not executable; a release carries a supervisor that can run it",
        path.display()
    );
    Ok(())
}

fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("failed to make {} executable", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A supervisor whose bytes are a real object file for this host, so
    /// materialization's target proof runs against something it can accept.
    fn host_supervisor(path: &Path) -> Vec<u8> {
        let bytes = fs::read(std::env::current_exe().expect("the test binary is readable"))
            .expect("the test binary is readable");
        fs::write(path, &bytes).expect("write the fixture supervisor");
        bytes
    }

    /// A release whose bundle directory is empty: enough for every layout rule,
    /// which is what this module owns. The bundle's own content is
    /// phoxal-bundle's contract, proven where that contract lives.
    fn staged_release(root: &Path) -> Vec<u8> {
        fs::create_dir_all(root.join(BUNDLE_DIR)).expect("bundle directory");
        let bytes = host_supervisor(&root.join(SUPERVISOR_FILE));
        make_executable(&root.join(SUPERVISOR_FILE)).expect("executable bit");
        bytes
    }

    /// The whole-release walk: both halves present, the supervisor runnable, and
    /// each half resolved to the path its consumer uses.
    #[test]
    fn a_complete_release_validates_and_resolves_both_halves() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().join("release");
        staged_release(&root);

        let layout = validate_layout(&root).expect("a complete release validates");
        assert_eq!(layout.root, root);
        assert_eq!(layout.supervisor, root.join(SUPERVISOR_FILE));
        assert_eq!(layout.bundle, root.join("bundle"));
        assert!(is_release_root(&root));
    }

    /// A release is exactly its layout. Anything else at the root is content
    /// nobody put there deliberately, and it is named in the refusal.
    #[test]
    fn an_unexpected_entry_at_the_release_root_is_refused_by_name() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().join("release");
        staged_release(&root);
        fs::write(root.join("leftover.tmp"), b"stray").expect("stray file");

        let error = validate_layout(&root)
            .expect_err("a release holds exactly its two entries")
            .to_string();
        assert_eq!(
            error,
            format!(
                "{} does not belong to a deployment release: a release holds exactly `phoxal-supervisor` \
                 and `bundle/`",
                root.join("leftover.tmp").display()
            )
        );
    }

    /// Each half is required, and the refusal says which one is gone.
    #[test]
    fn a_release_missing_either_half_is_refused_naming_it() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().join("release");
        staged_release(&root);
        fs::remove_file(root.join(SUPERVISOR_FILE)).expect("remove the supervisor");
        assert_eq!(
            validate_layout(&root)
                .expect_err("a release without its supervisor is not a release")
                .to_string(),
            format!(
                "{} is not a complete deployment release: `phoxal-supervisor` is missing",
                root.display()
            )
        );
        assert!(!is_release_root(&root));

        let bundleless = temp.path().join("bundleless");
        staged_release(&bundleless);
        fs::remove_dir_all(bundleless.join(BUNDLE_DIR)).expect("remove the bundle");
        assert_eq!(
            validate_layout(&bundleless)
                .expect_err("a release without its bundle is not a release")
                .to_string(),
            format!(
                "{} is not a complete deployment release: `bundle` is missing",
                bundleless.display()
            )
        );
    }

    /// Each half has to be the kind of entry it is, so a file named `bundle` or
    /// a directory named `phoxal-supervisor` is refused rather than half-read.
    #[test]
    fn each_half_must_be_the_kind_of_entry_it_is() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().join("release");
        staged_release(&root);
        fs::remove_dir_all(root.join(BUNDLE_DIR)).expect("remove the bundle");
        fs::write(root.join(BUNDLE_DIR), b"not a directory").expect("bundle file");
        assert_eq!(
            validate_layout(&root)
                .expect_err("a bundle is a directory")
                .to_string(),
            format!("{} must be a directory", root.join(BUNDLE_DIR).display())
        );
    }

    /// A supervisor that cannot be executed is not a usable supervisor: activating such
    /// a release would leave a unit that never starts.
    #[test]
    fn a_release_whose_supervisor_is_not_executable_is_refused() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().join("release");
        staged_release(&root);
        fs::set_permissions(
            root.join(SUPERVISOR_FILE),
            fs::Permissions::from_mode(0o644),
        )
        .expect("drop the executable bit");

        assert_eq!(
            validate_layout(&root)
                .expect_err("a release carries a supervisor that can run")
                .to_string(),
            format!(
                "the supervisor {} is not executable; a release carries a supervisor that can run it",
                root.join(SUPERVISOR_FILE).display()
            )
        );
    }

    /// An archive from before releases owned their supervisor is a bundle at its
    /// root. It is refused by name, with the rebuild that fixes it.
    #[test]
    fn a_bundle_shaped_archive_is_refused_as_predating_supervisor_owning_releases() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().join("extracted");
        fs::create_dir_all(root.join("bin")).expect("bin directory");
        fs::write(root.join(phoxal_bundle::MANIFEST_FILE), b"{}").expect("manifest document");

        let error = refuse_bundle_shaped_release(&root)
            .expect_err("a bundle root is not a release root")
            .to_string();
        assert_eq!(
            error,
            format!(
                "{} is a bundle, not a deployment release: it carries manifest.json at its \
                 root instead of `phoxal-supervisor` and `bundle/`. This archive predates supervisor-owning \
                 releases, where a release carries the supervisor that runs it; rebuild it with this \
                 CLI (`phoxal build`) and install the result",
                root.display()
            )
        );
        // The same refusal fronts the layout walk, so no caller can reach the
        // "unexpected entry" message for an old-shape archive instead.
        assert_eq!(
            validate_layout(&root)
                .expect_err("the layout walk refuses it too")
                .to_string(),
            error
        );
    }

    /// Materialization is the write half of the same contract: it copies the
    /// supervisor in, makes it runnable, and hands back a release that validates.
    #[test]
    fn materializing_a_release_installs_a_runnable_supervisor() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().join("release");
        fs::create_dir_all(root.join(BUNDLE_DIR)).expect("bundle directory");
        let source = temp.path().join(SUPERVISOR_FILE);
        let bytes = host_supervisor(&source);

        let layout = materialize(
            &root,
            &source,
            &crate::check::participant_metadata::expected_target_for_host(),
        )
        .expect("a host supervisor materializes");

        assert_eq!(layout.supervisor, root.join(SUPERVISOR_FILE));
        assert_eq!(
            fs::read(&layout.supervisor).expect("supervisor bytes"),
            bytes
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&layout.supervisor)
                .expect("supervisor metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "the supervisor stays executable");
        }
    }

    /// A release must carry a supervisor its own target can run, so packaging a
    /// foreign-architecture supervisor fails while the release is still a candidate.
    #[test]
    fn a_release_refuses_a_supervisor_built_for_another_target() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().join("release");
        fs::create_dir_all(root.join(BUNDLE_DIR)).expect("bundle directory");
        let source = temp.path().join(SUPERVISOR_FILE);
        host_supervisor(&source);

        let foreign = if cfg!(target_os = "macos") {
            "aarch64-unknown-linux-gnu"
        } else {
            "aarch64-apple-darwin"
        };
        let error = materialize(
            &root,
            &source,
            &crate::check::participant_metadata::expected_target_for_triple(foreign)
                .expect("a known triple"),
        )
        .expect_err("a foreign-target supervisor cannot run this release")
        .to_string();
        assert!(error.contains("cannot execute this release"), "{error}");
        assert!(
            !root.join(SUPERVISOR_FILE).exists(),
            "a refused supervisor is never written into the release"
        );
    }
}
