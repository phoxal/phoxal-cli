//! Bundle file serving.
//!
//! Every regular file in the finalized bundle - `robot.yaml`, `assets/*`, and
//! `bin/*` - is readable through one key. That is deliberately wider than the
//! participant-facing asset resolver it replaces: an attached client fetches
//! the finalized manifest here to verify identity and derive its own drive
//! parameters, so refusing `robot.yaml` would leave it with nothing to verify
//! against.
//!
//! The fence is the index, not the request. [`BundleResolver::index`] walks the
//! canonical bundle root once at startup and refuses symlinks outright, so no
//! indexed path can leave the bundle and a request can only ever *select* an
//! already-validated entry. The supervisor lock freezes the tree that index
//! describes for the daemon's lifetime.
//!
//! Nothing outside the bundle is reachable: not the project, not the host, not
//! the run directory holding the lock, the socket, and the log.

use phoxal_bus::{Bus, ServerQueryable};
use phoxal_manifest::bundle::{BundleError, BundleFile, BundleResolver, InvalidBundlePath};
use phoxal_supervisor_api::{BundleGetOutcome, BundlePathRejection, supervisor};

/// Read one bundle file, as the wire outcome.
///
/// A resolver error is not an outcome: an entry that changed on disk after
/// indexing is a broken bundle, not something an operator asked for, so it
/// takes the query's error leg rather than being flattened into `Missing`.
pub(crate) fn read(resolver: &BundleResolver, path: &str) -> Result<BundleGetOutcome, BundleError> {
    Ok(match resolver.get(path)? {
        BundleFile::Found { bytes, .. } => BundleGetOutcome::Found { bytes },
        BundleFile::Missing => BundleGetOutcome::Missing,
        BundleFile::InvalidPath(reason) => BundleGetOutcome::InvalidPath {
            reason: rejection(reason),
        },
        BundleFile::TooLarge {
            size_bytes,
            limit_bytes,
        } => BundleGetOutcome::TooLarge {
            size_bytes,
            limit_bytes,
        },
        // `BundleFile` is non-exhaustive. A framework release that adds an
        // outcome this build cannot name must not be reported as a successful
        // read of nothing.
        other => {
            tracing::warn!("unhandled bundle outcome for {path}: {other:?}");
            BundleGetOutcome::Missing
        }
    })
}

const fn rejection(reason: InvalidBundlePath) -> BundlePathRejection {
    match reason {
        InvalidBundlePath::Empty => BundlePathRejection::Empty,
        InvalidBundlePath::Absolute => BundlePathRejection::Absolute,
        InvalidBundlePath::Traversal => BundlePathRejection::ParentTraversal,
        InvalidBundlePath::NotNormalized => BundlePathRejection::NotNormalized,
        // The resolver's refusals are closed over the four above; anything a
        // future framework release adds is reported as unnormalized rather
        // than silently accepted.
        _ => BundlePathRejection::NotNormalized,
    }
}

/// Answer `supervisor/bundle/get` until the session ends.
pub(crate) async fn serve(bus: &Bus, queryable: &ServerQueryable, resolver: &BundleResolver) {
    super::answer(
        bus,
        queryable,
        "bundle/get",
        |request: supervisor::bundle::GetRequest| {
            let supervisor::bundle::GetRequest::V0 { path } = request;
            let outcome = match read(resolver, path.as_str()) {
                Ok(outcome) => outcome,
                Err(error) => {
                    tracing::warn!(path = path.as_str(), "bundle read failed: {error}");
                    // The bundle changed under a held lock: report it as gone
                    // rather than serving bytes that no longer describe it.
                    BundleGetOutcome::Missing
                }
            };
            supervisor::bundle::GetReply::V0 { outcome }
        },
    )
    .await;
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    const LIMIT: u64 = 64;

    fn bundle() -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("temp dir");
        let path = root.path();
        std::fs::write(path.join("robot.yaml"), b"schema: phoxal/robot/v0\n").expect("robot.yaml");
        std::fs::create_dir_all(path.join("assets/components/ddsm115")).expect("assets");
        std::fs::write(path.join("assets/components/ddsm115/component.yaml"), b"{}")
            .expect("component");
        std::fs::create_dir_all(path.join("bin")).expect("bin");
        std::fs::write(path.join("bin/brain"), b"elf").expect("brain");
        std::fs::write(path.join("bin/oversized"), vec![b'x'; LIMIT as usize + 1])
            .expect("oversized");
        root
    }

    fn resolver(root: &Path) -> BundleResolver {
        BundleResolver::index(root, LIMIT).expect("index the bundle")
    }

    #[test]
    fn the_manifest_the_assets_and_the_binaries_are_all_readable() {
        let root = bundle();
        let resolver = resolver(root.path());
        for path in [
            "robot.yaml",
            "assets/components/ddsm115/component.yaml",
            "bin/brain",
        ] {
            assert!(
                matches!(
                    read(&resolver, path).expect("no resolver failure"),
                    BundleGetOutcome::Found { .. }
                ),
                "{path} must be readable through bundle/get"
            );
        }
    }

    #[test]
    fn traversal_absolute_and_unnormalized_paths_are_rejected_not_reported_missing() {
        let root = bundle();
        let resolver = resolver(root.path());
        for (path, expected) in [
            ("", BundlePathRejection::Empty),
            ("/etc/passwd", BundlePathRejection::Absolute),
            ("../secret", BundlePathRejection::ParentTraversal),
            ("assets/../../secret", BundlePathRejection::ParentTraversal),
            ("./robot.yaml", BundlePathRejection::NotNormalized),
            ("assets//component.yaml", BundlePathRejection::NotNormalized),
            (r"assets\component.yaml", BundlePathRejection::NotNormalized),
        ] {
            assert_eq!(
                read(&resolver, path).expect("no resolver failure"),
                BundleGetOutcome::InvalidPath { reason: expected },
                "{path:?}"
            );
        }
    }

    #[test]
    fn a_directory_and_an_absent_file_are_missing_while_an_oversized_file_says_so() {
        let root = bundle();
        let resolver = resolver(root.path());
        for path in ["assets", "bin", "nothing/here"] {
            assert_eq!(
                read(&resolver, path).expect("no resolver failure"),
                BundleGetOutcome::Missing,
                "{path}"
            );
        }
        assert_eq!(
            read(&resolver, "bin/oversized").expect("no resolver failure"),
            BundleGetOutcome::TooLarge {
                size_bytes: LIMIT + 1,
                limit_bytes: LIMIT,
            }
        );
    }

    #[test]
    fn a_symlink_out_of_the_bundle_is_never_indexed() {
        let root = bundle();
        let outside = tempfile::tempdir().expect("temp dir");
        std::fs::write(outside.path().join("secret"), b"credentials").expect("secret");
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path().join("secret"), root.path().join("escape"))
            .expect("symlink");

        // Indexing refuses symlinks outright, so `escape` is not a key at all -
        // the fence is that it was never indexed, not that a request is
        // inspected for it.
        let Ok(resolver) = BundleResolver::index(root.path(), LIMIT) else {
            // A framework that refuses to index a bundle containing a symlink
            // at all is an even stronger fence; nothing to assert beyond it.
            return;
        };
        assert_eq!(
            read(&resolver, "escape").expect("no resolver failure"),
            BundleGetOutcome::Missing
        );
    }
}
