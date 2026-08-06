//! The client half of `bundle/get`.
//!
//! The supervisor is the only side that can decide the resolution rules -
//! symlink escape, regular-file-ness - because it is the only side holding the
//! bundle. The syntactic rules it applies first are mirrored here so a path the
//! contract already forbids never costs a round trip, and so a local rejection
//! and a remote one carry the same reason.

use phoxal_supervisor_api::text::BundlePath;
use phoxal_supervisor_api::{BundlePathRejection, validate_bundle_path};

use crate::error::AttachError;

/// Check a path locally and turn it into the bounded wire value.
///
/// # Errors
///
/// [`AttachError::InvalidBundlePath`] carrying the exact rule the path broke -
/// the same [`BundlePathRejection`] the supervisor would have answered with.
pub fn check_path(path: &str) -> Result<BundlePath, AttachError> {
    let invalid = |reason| AttachError::InvalidBundlePath {
        path: path.to_string(),
        reason,
    };
    validate_bundle_path(path).map_err(invalid)?;
    BundlePath::try_new(path).map_err(|_| invalid(BundlePathRejection::TooLong))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_forbidden_path_is_refused_before_it_reaches_the_wire() {
        let cases = [
            ("", BundlePathRejection::Empty),
            ("/etc/shadow", BundlePathRejection::Absolute),
            (
                "../../.ssh/id_ed25519",
                BundlePathRejection::ParentTraversal,
            ),
            ("assets/./robot", BundlePathRejection::NotNormalized),
        ];
        for (path, expected) in cases {
            let error = check_path(path).expect_err("a forbidden path must not reach the wire");
            match error {
                AttachError::InvalidBundlePath { reason, .. } => {
                    assert_eq!(reason, expected, "{path:?}");
                }
                other => panic!("{path:?} produced {other}"),
            }
            // The rejected path is quoted back, so a caller can render which
            // input was refused rather than only that something was.
            assert!(check_path(path).unwrap_err().to_string().contains(path) || path.is_empty());
        }
    }

    #[test]
    fn the_paths_the_bundle_actually_serves_pass() {
        for path in [
            "robot.yaml",
            "assets/robot/structure.urdf",
            "assets/components/ddsm115/component.yaml",
            "bin/brain",
        ] {
            assert_eq!(check_path(path).unwrap().as_str(), path);
        }
    }
}
