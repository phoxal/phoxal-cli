//! Bounds every supervisor document is held to, and the validation that
//! enforces the ones a type cannot.
//!
//! The bus already refuses to decode a body over
//! [`phoxal_bus::codec::MAX_DECODE_BODY_BYTES`]. These bounds sit under that
//! ceiling so a conforming daemon never composes a document its own clients
//! must refuse: a snapshot's worst case is bounded by construction, and a
//! bundle chunk is bounded by an explicit `TooLarge` outcome rather than by a
//! decode failure with no diagnosis.

use crate::model::{BundlePathRejection, Snapshot};
use crate::text::{BundlePath, Detail, Name, StderrTail};

/// Supervised processes one execution may report.
pub const MAX_PROCESSES: usize = 64;

/// Startup steps a snapshot may carry - one per [`crate::model::StartupStepKind`].
pub const MAX_STARTUP_STEPS: usize = 5;

/// Largest single bundle file `bundle/get` returns. Above this the daemon
/// answers `TooLarge` with the real size, which is a diagnosable answer; a
/// larger response would be an undiagnosable decode failure at the client.
pub const MAX_BUNDLE_CHUNK_BYTES: u64 = 8 * 1024 * 1024;

/// Longest bundle-relative path accepted by `bundle/get`.
pub const MAX_BUNDLE_PATH_BYTES: usize = BundlePath::MAX_BYTES;

/// Log records one snapshot or page may carry.
pub const MAX_LOG_RECORDS: usize = 2048;

/// Telemetry records one snapshot or page may carry.
pub const MAX_TELEMETRY_RECORDS: usize = 512;

/// Runtime rows one telemetry record may carry, excluding the overflow row.
pub const MAX_RUNTIME_TOPICS: usize = 128;

/// A document that exceeds a bound this contract cannot express in a type.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum BoundsError {
    #[error("a snapshot reports {count} processes; the limit is {MAX_PROCESSES}")]
    TooManyProcesses { count: usize },
    #[error("a snapshot carries {count} startup steps; the limit is {MAX_STARTUP_STEPS}")]
    TooManyStartupSteps { count: usize },
    #[error("a snapshot repeats the process key at index {index}")]
    DuplicateProcess { index: usize },
    #[error("a snapshot's process set is not ordered by key at index {index}")]
    UnorderedProcesses { index: usize },
}

/// Check the bounds a [`Snapshot`]'s own types do not carry.
///
/// # Errors
///
/// Returns the first bound the snapshot violates.
pub fn validate_snapshot(snapshot: &Snapshot) -> Result<(), BoundsError> {
    if snapshot.processes.len() > MAX_PROCESSES {
        return Err(BoundsError::TooManyProcesses {
            count: snapshot.processes.len(),
        });
    }
    if snapshot.startup.len() > MAX_STARTUP_STEPS {
        return Err(BoundsError::TooManyStartupSteps {
            count: snapshot.startup.len(),
        });
    }
    for (index, pair) in snapshot.processes.windows(2).enumerate() {
        match pair[0].key.cmp(&pair[1].key) {
            std::cmp::Ordering::Less => {}
            std::cmp::Ordering::Equal => {
                return Err(BoundsError::DuplicateProcess { index: index + 1 });
            }
            std::cmp::Ordering::Greater => {
                return Err(BoundsError::UnorderedProcesses { index: index + 1 });
            }
        }
    }
    Ok(())
}

/// The worst-case encoded size of one maximal snapshot, used to prove the
/// bounds above stay under the bus decode ceiling.
#[must_use]
pub const fn worst_case_snapshot_bytes() -> usize {
    // Every bounded text field a process row can carry, plus generous
    // structural headroom per row for the keys, enumerations, and integers.
    const PROCESS_BYTES: usize =
        3 * Name::MAX_BYTES + Detail::MAX_BYTES + StderrTail::MAX_BYTES + 1024;
    const FIXED_BYTES: usize =
        2 * Name::MAX_BYTES + MAX_STARTUP_STEPS * (Detail::MAX_BYTES + 128) + Detail::MAX_BYTES;
    MAX_PROCESSES * PROCESS_BYTES + FIXED_BYTES + 8 * 1024
}

/// Reject a bundle-relative path client-side, with the same rules the
/// supervisor applies before it touches the filesystem.
///
/// This is the syntactic half only. The rules a path's *resolution* decides -
/// symlink escape, regular-file-ness - stay with the supervisor, which is the
/// only side holding the bundle.
///
/// # Errors
///
/// Returns the specific rule the path broke, which is the same enumeration the
/// wire carries, so a local rejection and a remote one read identically.
pub fn validate_bundle_path(path: &str) -> Result<(), BundlePathRejection> {
    if path.is_empty() {
        return Err(BundlePathRejection::Empty);
    }
    if path.len() > MAX_BUNDLE_PATH_BYTES {
        return Err(BundlePathRejection::TooLong);
    }
    if path.starts_with('/') {
        return Err(BundlePathRejection::Absolute);
    }
    if path.contains('\\') || path.contains('\0') {
        return Err(BundlePathRejection::NotNormalized);
    }
    for segment in path.split('/') {
        match segment {
            ".." => return Err(BundlePathRejection::ParentTraversal),
            "" | "." => return Err(BundlePathRejection::NotNormalized),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        ComponentBinding, DesiredState, ExecutionMode, Lifecycle, Process, ProcessKey,
        ProcessState, RobotIdentity, StartupRequirement,
    };
    use phoxal_bus::codec::MAX_DECODE_BODY_BYTES;

    fn process(key: ProcessKey) -> Process {
        Process {
            component: key.component_instance().map(|instance| ComponentBinding {
                instance: instance.clone(),
                component_type: Name::new("ddsm115"),
            }),
            key,
            startup: StartupRequirement::Required,
            desired: DesiredState::Running,
            state: ProcessState::Ready,
            pid: Some(4242),
            producer: None,
            restarts: 0,
            failure: None,
        }
    }

    fn snapshot(processes: Vec<Process>) -> Snapshot {
        Snapshot {
            revision: 1,
            robot: RobotIdentity {
                id: Name::new("rover"),
                namespace: Name::new("demo"),
            },
            mode: ExecutionMode::Real,
            lifecycle: Lifecycle::Ready,
            startup: Vec::new(),
            processes,
            failure: None,
        }
    }

    #[test]
    fn a_snapshots_process_set_must_be_ordered_and_unique() {
        let brain = process(ProcessKey::Brain);
        let drive = process(ProcessKey::Service {
            id: Name::new("drive"),
        });

        assert_eq!(
            validate_snapshot(&snapshot(vec![brain.clone(), drive.clone()])),
            Ok(())
        );
        assert_eq!(
            validate_snapshot(&snapshot(vec![drive.clone(), brain.clone()])),
            Err(BoundsError::UnorderedProcesses { index: 1 })
        );
        assert_eq!(
            validate_snapshot(&snapshot(vec![drive.clone(), drive])),
            Err(BoundsError::DuplicateProcess { index: 1 })
        );
        assert_eq!(
            validate_snapshot(&snapshot(vec![brain; MAX_PROCESSES + 1])),
            Err(BoundsError::TooManyProcesses {
                count: MAX_PROCESSES + 1
            })
        );
    }

    #[test]
    fn the_snapshot_bounds_stay_under_the_bus_decode_ceiling() {
        assert!(
            worst_case_snapshot_bytes() < MAX_DECODE_BODY_BYTES,
            "worst case {} exceeds the bus ceiling {MAX_DECODE_BODY_BYTES}",
            worst_case_snapshot_bytes()
        );
        assert!(MAX_BUNDLE_CHUNK_BYTES < MAX_DECODE_BODY_BYTES as u64);
    }

    #[test]
    fn bundle_paths_are_rejected_by_rule_not_normalized_into_silence() {
        let cases = [
            ("", BundlePathRejection::Empty),
            ("/etc/passwd", BundlePathRejection::Absolute),
            ("assets/../../secret", BundlePathRejection::ParentTraversal),
            ("..", BundlePathRejection::ParentTraversal),
            ("./robot.yaml", BundlePathRejection::NotNormalized),
            ("assets//robot", BundlePathRejection::NotNormalized),
            ("assets\\robot", BundlePathRejection::NotNormalized),
            ("robot.yaml\0", BundlePathRejection::NotNormalized),
        ];
        for (path, expected) in cases {
            assert_eq!(validate_bundle_path(path), Err(expected), "{path:?}");
        }
        assert_eq!(
            validate_bundle_path(&"a".repeat(MAX_BUNDLE_PATH_BYTES + 1)),
            Err(BundlePathRejection::TooLong)
        );

        for path in ["robot.yaml", "assets/robot/structure.urdf", "bin/brain"] {
            assert_eq!(validate_bundle_path(path), Ok(()), "{path:?}");
        }
    }
}
