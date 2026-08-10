//! The CLI-owned official runtime catalog.
//!
//! Official participants are never discovered from a network inventory: they
//! are compiled into this CLI release. `robot.yaml` declares USER services and
//! component instances; the official set comes from here alone.
//!
//! The catalog is **internal data with no identity token**. It never crosses a
//! process boundary: `phoxal` and `phoxald` ship as one exact pair, participants
//! never read it, and an attaching client derives topology from the supervisor
//! snapshot rather than from a catalog. Compatibility at a process boundary is
//! the framework train version each binary was built from, compared for exact
//! equality, so a catalog identity would be a second claim about the same
//! thing. There is consequently no `CatalogId` and no catalog constant in any
//! wire contract: this remains an identity-free index of where official
//! participants live.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::todo,
        clippy::unimplemented
    )
)]

use std::collections::BTreeSet;

/// The registry name every `cargo install`/`cargo metadata` invocation against
/// official packages registers under. Robot projects carry no registry
/// configuration of their own - the CLI supplies it on every invocation via
/// `--config`.
pub const REGISTRY_NAME: &str = "phoxal";

/// The static margo registry official packages publish to.
pub const REGISTRY_INDEX: &str = "sparse+https://phoxal.github.io/registry/";

/// What an official catalog entry is, which decides its canonical staged binary
/// name and whether it belongs to the native or the simulation membership.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ArtifactKind {
    Service,
    ComponentDriver,
    Simulator,
}

impl ArtifactKind {
    /// The kind token used in package names and diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Service => "service",
            Self::ComponentDriver => "driver",
            Self::Simulator => "simulator",
        }
    }

    /// The canonical `bin/` file name a participant of this kind is staged
    /// under, given its short identity (`drive`, `ddsm115`).
    #[must_use]
    pub fn staged_binary_name(self, short: &str) -> String {
        match self {
            Self::ComponentDriver => format!("phoxal-component-{short}"),
            Self::Service => format!("phoxal-service-{short}"),
            Self::Simulator => format!("phoxal-simulator-{short}"),
        }
    }

    /// The provider-qualified package prefix entries of this kind carry.
    const fn package_prefix(self) -> &'static str {
        match self {
            Self::Service => "phoxal/service-",
            Self::ComponentDriver => "phoxal/component-",
            Self::Simulator => "phoxal/simulator-",
        }
    }
}

impl std::fmt::Display for ArtifactKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One official participant the catalog owns.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OfficialRuntime {
    /// The provider-qualified registry package identity (`phoxal/service-drive`).
    pub package: &'static str,
    pub kind: ArtifactKind,
}

impl OfficialRuntime {
    /// The participant identity this official declares in its own binary
    /// metadata, and the identity the launch graph uses: `phoxal/service-drive`
    /// is `drive`.
    #[must_use]
    pub fn short_name(&self) -> &'static str {
        self.package
            .strip_prefix(self.kind.package_prefix())
            .unwrap_or(self.package)
    }

    /// The canonical `bin/` file name this official is staged under.
    #[must_use]
    pub fn staged_binary_name(&self) -> String {
        self.kind.staged_binary_name(self.short_name())
    }

    /// The Cargo package name this catalog identity is published under.
    /// Catalog identities are not Cargo package names.
    #[must_use]
    pub fn cargo_package_name(&self) -> String {
        cargo_package_name(self.package)
    }
}

/// Project a provider-qualified catalog identity (`phoxal/service-drive`) to the
/// Cargo package name it is published under (`phoxal-service-drive`).
#[must_use]
pub fn cargo_package_name(package: &str) -> String {
    package.replace('/', "-")
}

/// The `--config` argument that injects the registry index into a `cargo`
/// invocation that never reads a project's own `.cargo/config.toml` for it.
#[must_use]
pub fn registry_config_arg() -> String {
    format!("registries.{REGISTRY_NAME}.index=\"{REGISTRY_INDEX}\"")
}

const NATIVE: &[OfficialRuntime] = &[
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

const SIMULATION: &[OfficialRuntime] = &[OfficialRuntime {
    package: "phoxal/simulator-webots-controller",
    kind: ArtifactKind::Simulator,
}];

/// The official participant set one `phoxal`/`phoxald` pair interprets.
///
/// A single current snapshot, deliberately not a table keyed by framework
/// train: pre-v1 every binary in one execution is built from one train, and the
/// embedded compatibility record of each built binary - not a catalog
/// version - is what proves it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Catalog;

impl Catalog {
    /// The official set this CLI release ships.
    #[must_use]
    pub const fn official() -> Self {
        Self
    }

    /// The officials that run on real hardware. Every one of them is
    /// mandatory: an official participant is always started, always startup
    /// required, and takes no authored configuration.
    pub fn native(&self) -> impl Iterator<Item = &'static OfficialRuntime> {
        NATIVE.iter()
    }

    /// The officials that only exist when the robot runs on simulated time.
    /// The world-clock owner is the first of them.
    pub fn simulation(&self) -> impl Iterator<Item = &'static OfficialRuntime> {
        SIMULATION.iter()
    }

    /// The participant that owns the simulated world clock. A `clock:
    /// simulated` bundle without it can never leave `Starting`.
    #[must_use]
    pub fn world_clock_owner(&self) -> &'static OfficialRuntime {
        &SIMULATION[0]
    }

    /// Whether `identity` is an official service short name, so an authored
    /// `services:` entry claiming it can be rejected with a precise diagnostic.
    #[must_use]
    pub fn is_official_service(&self, identity: &str) -> bool {
        self.native().any(|official| {
            official.kind == ArtifactKind::Service && official.short_name() == identity
        })
    }

    /// Every official service short name.
    #[must_use]
    pub fn service_identities(&self) -> BTreeSet<&'static str> {
        self.native()
            .filter(|official| official.kind == ArtifactKind::Service)
            .map(OfficialRuntime::short_name)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_identities_project_to_cargo_package_names() {
        assert_eq!(
            cargo_package_name("phoxal/service-drive"),
            "phoxal-service-drive"
        );
        assert_eq!(
            cargo_package_name("phoxal/component-ddsm115"),
            "phoxal-component-ddsm115"
        );
    }

    #[test]
    fn an_official_names_its_identity_and_its_staged_binary() {
        let drive = Catalog::official()
            .native()
            .find(|official| official.short_name() == "drive")
            .expect("drive is an official service");
        assert_eq!(drive.staged_binary_name(), "phoxal-service-drive");
        assert_eq!(drive.cargo_package_name(), "phoxal-service-drive");
        assert_eq!(
            ArtifactKind::ComponentDriver.staged_binary_name("ddsm115"),
            "phoxal-component-ddsm115"
        );
    }

    #[test]
    fn native_membership_excludes_simulators_and_simulation_owns_the_world_clock() {
        let catalog = Catalog::official();
        assert!(
            catalog
                .native()
                .all(|official| official.kind == ArtifactKind::Service),
            "the native set is services only"
        );
        assert_eq!(
            catalog.world_clock_owner().short_name(),
            "webots-controller"
        );
        assert!(catalog.is_official_service("drive"));
        assert!(!catalog.is_official_service("mission"));
    }

    #[test]
    fn registry_config_arg_carries_the_live_static_registry() {
        assert_eq!(
            registry_config_arg(),
            "registries.phoxal.index=\"sparse+https://phoxal.github.io/registry/\""
        );
    }
}
