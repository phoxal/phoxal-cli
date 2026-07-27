//! CLI-owned official runtime catalog.
//!
//! Official services, tools, and the infrastructure router are never
//! discovered from a network inventory: they are compiled into this CLI
//! release. `robot.yaml` declares USER services/tools and component
//! instances; the official set comes from here alone (organization#951 WS4).
//!
//! ## One catalog, no per-train history (organization#951 WS4 review, medium 3)
//!
//! [`NATIVE`]/[`WEBOTS`] are a single, current snapshot - deliberately not a
//! table keyed by train. The design this replaced enumerated official
//! membership per train (a suite fetched per project); the whole point of
//! the cutover was to stop carrying that history. But applying today's
//! snapshot unchanged to an arbitrarily old locked train is not "no
//! history", it is silently pretending history does not matter: an old
//! train can be missing a package the catalog added since, or have carried
//! one the catalog has since removed (concretely, `phoxal
//! /simulator-webots-supervisor` existed in a past release's set and does
//! not today). [`ensure_train_supported`] is the deliberately narrow fix -
//! not per-train history, just a single floor below which this catalog is
//! known NOT to apply, so a robot locked below it gets a clear, actionable
//! error naming its train and the floor instead of a resolved set that never
//! existed for it.

use anyhow::{Context, Result, ensure};

use crate::project::train::{LockedTrain, TrainSource};

/// The oldest locked framework train this catalog snapshot is known to
/// faithfully represent - the exact `phoxal` version phoxal-cli itself is
/// pinned to (see the workspace `Cargo.toml`). Bump this alongside any
/// change to [`NATIVE`]/[`WEBOTS`] (an official package added, removed, or
/// renamed) and alongside every `phoxal` dependency bump, so it always
/// names the true floor rather than lagging it.
pub const CATALOG_FLOOR_TRAIN: &str = "0.42.0";

/// Reject a locked framework train this catalog cannot faithfully represent:
/// anything older than [`CATALOG_FLOOR_TRAIN`]. Every resolution entry point
/// calls this immediately after reading the locked train and before
/// applying [`NATIVE`]/[`WEBOTS`] to it, so a stale lock fails loudly -
/// naming both the train it found and the floor it needs - instead of
/// silently resolving an official set that never existed for that train.
///
/// A [`TrainSource::Path`] lock (a local `phoxal` path dependency - framework
/// development, never a real deployed robot) is exempt: its version is
/// whatever placeholder the local checkout happens to carry, not a real
/// release the catalog needs to have existed for, and the developer working
/// against a local framework checkout is already responsible for keeping it
/// in sync.
pub fn ensure_train_supported(train: &LockedTrain) -> Result<()> {
    if matches!(train.source, TrainSource::Path) {
        return Ok(());
    }
    let version = &train.version;
    let locked = semver::Version::parse(version).with_context(|| {
        format!("locked framework train `{version}` is not a valid semantic version")
    })?;
    let floor = semver::Version::parse(CATALOG_FLOOR_TRAIN)
        .expect("CATALOG_FLOOR_TRAIN is a valid semantic version constant");
    ensure!(
        locked >= floor,
        "the locked framework train {version} is older than this phoxal-cli release's official \
         catalog supports (floor: {CATALOG_FLOOR_TRAIN}); resolving today's official package \
         set against it would not faithfully represent what train {version} actually shipped. \
         Upgrade the project's locked framework train to {CATALOG_FLOOR_TRAIN} or newer, or \
         use a phoxal-cli release published for train {version}."
    );
    Ok(())
}

/// The registry name every `cargo install`/`cargo metadata` invocation
/// against official packages registers under. Robot projects carry no
/// registry configuration of their own - the CLI supplies it on every
/// invocation via `--config`.
pub const REGISTRY_NAME: &str = "phoxal";

/// The static margo registry official packages publish to
/// (organization#951 WS1/WS3).
pub const REGISTRY_INDEX: &str = "sparse+https://phoxal.github.io/registry/";

/// The `--config` argument that injects the registry index into a `cargo`
/// invocation that never reads a project's own `.cargo/config.toml` for it.
#[must_use]
pub fn registry_config_arg() -> String {
    format!("registries.{REGISTRY_NAME}.index=\"{REGISTRY_INDEX}\"")
}

/// Project the catalog's provider-qualified package identity
/// (`phoxal/service-drive`) to the Cargo package name it is published under
/// (`phoxal-service-drive`). Catalog identities are not Cargo package names.
#[must_use]
pub fn cargo_package_name(catalog_id: &str) -> String {
    catalog_id.replace('/', "-")
}

#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Service,
    ComponentAssets,
    ComponentDriver,
    Tool,
    Simulator,
    Infrastructure,
}

impl ArtifactKind {
    #[must_use]
    pub const fn wire_kind(self) -> &'static str {
        match self {
            Self::Service => "service",
            Self::ComponentAssets => "component_assets",
            Self::ComponentDriver => "driver",
            Self::Tool => "tool",
            Self::Simulator => "simulator",
            Self::Infrastructure => "infrastructure",
        }
    }
}

impl std::fmt::Display for ArtifactKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.wire_kind())
    }
}

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

pub const WEBOTS: &[OfficialRuntime] = &[OfficialRuntime {
    package: "phoxal/simulator-webots-controller",
    kind: ArtifactKind::Simulator,
}];

pub fn for_webots(webots: bool) -> impl Iterator<Item = &'static OfficialRuntime> {
    NATIVE
        .iter()
        .chain(webots.then_some(WEBOTS).into_iter().flatten())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_package_name_projects_catalog_identity() {
        assert_eq!(
            cargo_package_name("phoxal/service-drive"),
            "phoxal-service-drive"
        );
        assert_eq!(
            cargo_package_name("phoxal/component-ddsm115"),
            "phoxal-component-ddsm115"
        );
    }

    fn registry_train(version: &str) -> LockedTrain {
        LockedTrain {
            version: version.to_string(),
            source: TrainSource::Registry,
        }
    }

    #[test]
    fn ensure_train_supported_accepts_the_floor_and_newer() {
        ensure_train_supported(&registry_train(CATALOG_FLOOR_TRAIN))
            .expect("the floor itself is supported");
        ensure_train_supported(&registry_train("99.0.0"))
            .expect("anything newer than the floor is supported");
    }

    #[test]
    fn ensure_train_supported_rejects_a_train_older_than_the_floor() {
        let error = ensure_train_supported(&registry_train("0.23.0"))
            .expect_err("a train older than the floor must be rejected");
        let message = error.to_string();
        assert!(message.contains("0.23.0"), "{message}");
        assert!(message.contains(CATALOG_FLOOR_TRAIN), "{message}");
    }

    #[test]
    fn ensure_train_supported_rejects_a_non_semver_train() {
        let error = ensure_train_supported(&registry_train("not-a-version"))
            .expect_err("a non-semver locked train must be rejected, not panic");
        assert!(error.to_string().contains("not-a-version"));
    }

    #[test]
    fn ensure_train_supported_exempts_a_local_path_dependency() {
        // Framework development against a local `phoxal` path dependency
        // carries whatever placeholder version the checkout happens to have
        // (often far below the floor) and is not a real deployed robot -
        // the floor exists to protect against a stale REAL release, not a
        // developer's local checkout.
        ensure_train_supported(&LockedTrain {
            version: "0.1.0".to_string(),
            source: TrainSource::Path,
        })
        .expect("a local path-dependency train is exempt from the floor");
    }

    #[test]
    fn registry_config_arg_carries_the_live_static_registry() {
        assert_eq!(
            registry_config_arg(),
            "registries.phoxal.index=\"sparse+https://phoxal.github.io/registry/\""
        );
    }
}
