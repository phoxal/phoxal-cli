//! Loading the daemon's sole persisted input.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use phoxal_bundle::RuntimeBundle;
use phoxal_runtime_contract::version::{CompatibilityLine, FrameworkVersion};

const PROJECT_BUNDLE_SUFFIX: [&str; 2] = [".phoxal", "bundle"];

/// A bundle built on a framework compatibility line this daemon does not
/// execute.
///
/// Two Phoxal binaries speak the same contracts exactly when they share a
/// compatibility line, so `phoxald` refuses a bundle whose line is not its own
/// before it launches a single process. Agreement among the bundle's own
/// participants is already proven when its document is read; this is the other
/// half of that invariant, between the bundle and its executor.
///
/// The daemon names its exact train because that is the provenance an operator
/// reports; the bundle has only a line to name, since its artifacts may have
/// been built from different trains on it. The train behind each artifact stays
/// readable from the document itself.
#[derive(Debug, thiserror::Error)]
#[error(
    "this bundle was built on the phoxal framework {bundle} line, but this phoxald executes \
     framework {daemon}, on the {0} line; rebuild the bundle on {0}, or run a phoxald from the \
     {bundle} line",
    daemon.compatibility_line()
)]
pub(crate) struct IncompatibleBundle {
    bundle: CompatibilityLine,
    daemon: FrameworkVersion,
}

/// Open and integrity-check the canonical `runtime.json + assets/ + bin/`
/// boundary, then require its framework compatibility line to be exactly this
/// daemon's. Authored source documents are never consulted here.
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
    let bundle = RuntimeBundle::open_verified(requested).with_context(|| {
        format!(
            "{} is not a valid compiled runtime bundle",
            requested.display()
        )
    })?;
    let (bundle_line, daemon) = (
        bundle.document().framework_line(),
        FrameworkVersion::CURRENT,
    );
    if bundle_line != daemon.compatibility_line() {
        return Err(IncompatibleBundle {
            bundle: bundle_line,
            daemon,
        })
        .with_context(|| format!("{} cannot be executed here", requested.display()));
    }
    Ok(bundle)
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
    use std::collections::BTreeMap;

    use phoxal_bundle::{
        AssetIndex, BinaryReference, BinarySource, BundlePath, BundleWriter, ParticipantClock,
        Runtime, RuntimeDocument, RuntimeParticipant,
    };
    use phoxal_model::RobotBuilder;
    use phoxal_runtime_contract::identity::{ParticipantArtifactId, ParticipantId};
    use phoxal_runtime_contract::metadata::{ParticipantContract, ParticipantKind};

    use super::*;

    /// Write a one-brain bundle whose artifact records `framework`, and hand
    /// back its root. The temporary directory is returned with it because it
    /// owns the bundle for as long as the test needs it.
    fn bundle_built_from(framework: FrameworkVersion) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().expect("temporary bundle parent");
        let source = BinarySource::open(std::env::current_exe().expect("test executable"))
            .expect("test executable source");
        let artifact = ParticipantArtifactId::new("brain").expect("artifact id");
        let binary_path = BundlePath::new("bin/brain").expect("binary path");
        let reference = BinaryReference::from_source(
            binary_path.clone(),
            ParticipantContract {
                framework,
                id: artifact.clone(),
                kind: ParticipantKind::Brain,
                requirement: None,
                config_schema: serde_json::json!({"type": "null"}),
            },
            &source,
        )
        .expect("binary reference");
        let runtime = Runtime::new(
            RobotBuilder::new("rover").build().expect("robot"),
            BTreeMap::from([(artifact.clone(), reference)]),
            vec![RuntimeParticipant::new(
                ParticipantId::new("brain").expect("participant id"),
                artifact,
                None,
                None,
                ParticipantClock::Real,
            )],
            AssetIndex::from_bytes(&BTreeMap::new()).expect("asset index"),
            None,
        )
        .expect("runtime");
        let bundle_root = root.path().join("bundle");
        BundleWriter::write(
            bundle_root.clone(),
            &RuntimeDocument::new(runtime),
            &BTreeMap::new(),
            &BTreeMap::from([(binary_path, source)]),
        )
        .expect("bundle");
        (root, bundle_root)
    }

    /// A bundle from another compatibility line is refused before a single
    /// process starts, and the refusal names both lines and the daemon's own
    /// train.
    #[test]
    fn a_bundle_from_another_line_is_refused_naming_both_lines() {
        let (_root, path) = bundle_built_from(FrameworkVersion::new(9, 9, 9));
        let message = format!(
            "{:#}",
            open(&path).expect_err("a foreign line has no valid launch here")
        );
        assert!(message.contains("9.x line"), "{message}");
        let daemon = FrameworkVersion::CURRENT;
        assert!(
            message.contains(&format!("{} line", daemon.compatibility_line())),
            "{message}"
        );
        assert!(message.contains(&daemon.to_string()), "{message}");
    }

    /// A bundle another train on this daemon's line recorded is executed as it
    /// stands. The trains differing is provenance, not an incompatibility, so
    /// there is nothing here for an operator to rebuild.
    #[test]
    fn a_bundle_from_another_train_on_this_line_is_accepted() {
        let daemon = FrameworkVersion::CURRENT;
        let neighbour = FrameworkVersion::new(
            daemon.major(),
            daemon.minor(),
            daemon.patch().wrapping_add(1),
        );
        assert_ne!(neighbour, daemon);
        let (_root, path) = bundle_built_from(neighbour);
        let bundle = open(&path).expect("a train on this line executes here");
        assert_eq!(
            bundle.document().framework_line(),
            daemon.compatibility_line()
        );
    }

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
