//! CLI-owned official runtime catalog.
//!
//! The immutable suite is only an inventory of bytes. Runtime membership
//! belongs to this CLI release and is deliberately not read from suite
//! profiles. Launch planning applies the CLI-internal failure policy.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use semver::Version;

use super::suite::{ArtifactKind, Kind, Suite};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OfficialRuntime {
    pub package: &'static str,
    pub kind: ArtifactKind,
}

pub const NATIVE: &[OfficialRuntime] = &[
    OfficialRuntime {
        package: "phoxal/infrastructure-router",
        kind: ArtifactKind::Infrastructure,
    },
    OfficialRuntime {
        package: "phoxal/tool-bus",
        kind: ArtifactKind::Tool,
    },
    OfficialRuntime {
        package: "phoxal/tool-device",
        kind: ArtifactKind::Tool,
    },
    OfficialRuntime {
        package: "phoxal/tool-joypad",
        kind: ArtifactKind::Tool,
    },
    OfficialRuntime {
        package: "phoxal/tool-log",
        kind: ArtifactKind::Tool,
    },
    OfficialRuntime {
        package: "phoxal/tool-telemetry",
        kind: ArtifactKind::Tool,
    },
    OfficialRuntime {
        package: "phoxal/service-asset",
        kind: ArtifactKind::Service,
    },
    OfficialRuntime {
        package: "phoxal/service-behavior",
        kind: ArtifactKind::Service,
    },
    OfficialRuntime {
        package: "phoxal/service-drive",
        kind: ArtifactKind::Service,
    },
    OfficialRuntime {
        package: "phoxal/service-frame",
        kind: ArtifactKind::Service,
    },
    OfficialRuntime {
        package: "phoxal/service-joint",
        kind: ArtifactKind::Service,
    },
    OfficialRuntime {
        package: "phoxal/service-localize",
        kind: ArtifactKind::Service,
    },
    OfficialRuntime {
        package: "phoxal/service-map",
        kind: ArtifactKind::Service,
    },
    OfficialRuntime {
        package: "phoxal/service-motion",
        kind: ArtifactKind::Service,
    },
    OfficialRuntime {
        package: "phoxal/service-navigation",
        kind: ArtifactKind::Service,
    },
    OfficialRuntime {
        package: "phoxal/service-odometry",
        kind: ArtifactKind::Service,
    },
    OfficialRuntime {
        package: "phoxal/service-perception",
        kind: ArtifactKind::Service,
    },
    OfficialRuntime {
        package: "phoxal/service-power",
        kind: ArtifactKind::Service,
    },
    OfficialRuntime {
        package: "phoxal/service-safety",
        kind: ArtifactKind::Service,
    },
    OfficialRuntime {
        package: "phoxal/service-video",
        kind: ArtifactKind::Service,
    },
];

pub const WEBOTS: &[OfficialRuntime] = &[
    OfficialRuntime {
        package: "phoxal/simulator-webots-controller",
        kind: ArtifactKind::Simulator,
    },
    OfficialRuntime {
        package: "phoxal/simulator-webots-supervisor",
        kind: ArtifactKind::Simulator,
    },
];

/// First framework train whose inventory is defined by this complete catalog.
/// This advances with the CLI's exact `phoxal` dependency; newer trains remain
/// subject to the same completeness guarantee.
const COMPLETE_CATALOG_SINCE: &str = phoxal::VERSION;

pub fn for_webots(webots: bool) -> impl Iterator<Item = &'static OfficialRuntime> {
    NATIVE
        .iter()
        .chain(webots.then_some(WEBOTS).into_iter().flatten())
}

/// Refuse a train whose official inventory this CLI cannot interpret.
pub fn validate_suite_inventory(suite: &Suite) -> Result<()> {
    let known = for_webots(true)
        .map(|runtime| (runtime.package, runtime.kind))
        .collect::<BTreeMap<_, _>>();
    let unknown = suite
        .artifacts
        .iter()
        .filter(|artifact| artifact.kind != Kind::Component)
        .filter(|artifact| !known.contains_key(artifact.id.as_str()))
        .map(|artifact| artifact.id.as_str())
        .collect::<Vec<_>>();
    if !unknown.is_empty() {
        bail!(
            "framework train {} contains unknown official package(s): {}. Update phoxal-cli before using this train",
            suite.version,
            unknown.join(", ")
        );
    }
    let mismatched = suite
        .artifacts
        .iter()
        .filter(|artifact| artifact.kind != Kind::Component)
        .filter_map(|artifact| {
            let expected = known.get(artifact.id.as_str())?;
            let actual = suite_kind(*expected);
            (artifact.kind != actual).then(|| {
                format!(
                    "{} is {:?}, expected {:?}",
                    artifact.id, artifact.kind, actual
                )
            })
        })
        .collect::<Vec<_>>();
    if !mismatched.is_empty() {
        bail!(
            "framework train {} has incorrect official package kind(s): {}",
            suite.version,
            mismatched.join(", ")
        );
    }
    let present = suite
        .artifacts
        .iter()
        .filter(|artifact| artifact.kind != Kind::Component)
        .map(|artifact| artifact.id.as_str())
        .collect::<BTreeSet<_>>();
    let missing = known
        .keys()
        .copied()
        .collect::<BTreeSet<_>>()
        .difference(&present)
        .copied()
        .collect::<Vec<_>>();
    let version = Version::parse(&suite.version)
        .with_context(|| format!("suite version {} is not semantic", suite.version))?;
    let baseline =
        Version::parse(COMPLETE_CATALOG_SINCE).expect("phoxal package version is semantic");
    if version >= baseline && !missing.is_empty() {
        bail!(
            "framework train {} is missing CLI-required official package(s): {}",
            suite.version,
            missing.join(", ")
        );
    }
    Ok(())
}

const fn suite_kind(kind: ArtifactKind) -> Kind {
    match kind {
        ArtifactKind::Service => Kind::Service,
        ArtifactKind::Tool => Kind::Tool,
        ArtifactKind::Simulator => Kind::Simulator,
        ArtifactKind::Infrastructure => Kind::Infrastructure,
        ArtifactKind::ComponentAssets | ArtifactKind::ComponentDriver => Kind::Component,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::suite::{fixture_blob_for_tests, fixture_suite_for_tests};
    use phoxal::suite::v1::Artifact;

    #[test]
    fn unknown_official_package_requests_cli_update() {
        let mut suite = fixture_suite_for_tests(Vec::new());
        suite.artifacts.push(Artifact {
            id: "phoxal/tool-future".to_string(),
            kind: Kind::Tool,
            targets: [(
                "x86_64-unknown-linux-gnu".to_string(),
                fixture_blob_for_tests("https://example.invalid/future.tar.gz", &"0".repeat(64), 1),
            )]
            .into_iter()
            .collect(),
            assets: None,
        });
        let error = validate_suite_inventory(&suite).unwrap_err();
        assert!(error.to_string().contains("Update phoxal-cli"));
        assert!(error.to_string().contains("phoxal/tool-future"));
    }

    #[test]
    fn current_train_requires_every_catalog_package() {
        let mut suite = fixture_suite_for_tests(Vec::new());
        suite.version = COMPLETE_CATALOG_SINCE.to_string();
        let error = validate_suite_inventory(&suite).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("missing CLI-required official package")
        );
        assert!(error.to_string().contains("phoxal/service-drive"));
        assert!(error.to_string().contains("phoxal/tool-joypad"));
        assert!(
            error
                .to_string()
                .contains("phoxal/simulator-webots-controller")
        );
    }

    #[test]
    fn newer_train_keeps_the_catalog_completeness_gate() {
        let mut suite = fixture_suite_for_tests(Vec::new());
        let mut next = Version::parse(COMPLETE_CATALOG_SINCE).unwrap();
        next.patch += 1;
        suite.version = next.to_string();
        let error = validate_suite_inventory(&suite).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("missing CLI-required official package")
        );
    }

    #[test]
    fn official_package_kind_must_match_the_catalog() {
        let mut suite = fixture_suite_for_tests(Vec::new());
        suite.artifacts.push(Artifact {
            id: "phoxal/service-drive".to_string(),
            kind: Kind::Tool,
            targets: [(
                "x86_64-unknown-linux-gnu".to_string(),
                fixture_blob_for_tests("https://example.invalid/drive.tar.gz", &"0".repeat(64), 1),
            )]
            .into_iter()
            .collect(),
            assets: None,
        });
        let error = validate_suite_inventory(&suite).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("incorrect official package kind")
        );
        assert!(error.to_string().contains("phoxal/service-drive"));
    }
}
