//! The CLI-owned official runtime catalog.
//!
//! Official participants are never discovered from a network inventory: they
//! are compiled into this CLI release. `robot.yaml` declares USER services and
//! component instances; the official set comes from here alone.
//!
//! The catalog is **internal data with no identity token**. It never crosses a
//! process boundary: it is compiled into this binary and never serialized,
//! participants never read it, and an attaching client derives topology from
//! the supervisor snapshot rather than from a catalog. Compatibility at a
//! process boundary is
//! the framework compatibility line each binary was built on, so a catalog
//! identity would be a second claim about the same
//! thing. There is consequently no `CatalogId` and no catalog constant in any
//! wire contract: this remains an identity-free index of where official
//! participants live.
//!
//! The Webots adapter binaries are deliberately *not* entries here. They are
//! not robot participants, are never staged into a bundle, and publish on the
//! exact framework train, so they are modelled below only as host tools the CLI
//! materializes side by side in its own cache.

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

/// Exact-train host tools for the version 0 Webots adapter.
///
/// These binaries are materialized beside each other for the local host. They
/// never enter a robot bundle and this crate never links their Webots APIs.
pub const WEBOTS_HOST_PACKAGE: &str = "phoxal-simulator-webots-host";
pub const WEBOTS_WORLD_CONTROLLER_PACKAGE: &str = "phoxal-simulator-webots-world-controller";
pub const WEBOTS_ROBOT_CONTROLLER_PACKAGE: &str = "phoxal-simulator-webots-robot-controller";

pub const WEBOTS_ADAPTER_PACKAGES: [&str; 3] = [
    WEBOTS_HOST_PACKAGE,
    WEBOTS_WORLD_CONTROLLER_PACKAGE,
    WEBOTS_ROBOT_CONTROLLER_PACKAGE,
];

/// What an official catalog entry is, which decides its canonical staged binary
/// name.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ArtifactKind {
    Service,
    ComponentDriver,
}

impl ArtifactKind {
    /// The kind token used in package names and diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Service => "service",
            Self::ComponentDriver => "driver",
        }
    }

    /// The Cargo package and executable name an official of this kind is
    /// published and installed under, given its short identity (`drive`,
    /// `ddsm115`).
    ///
    /// This is the name `cargo install` writes into `<root>/bin`, and it is
    /// **not** the name the binary is staged under inside a bundle - see
    /// [`bundle_binary_name`].
    #[must_use]
    pub fn cargo_binary_name(self, short: &str) -> String {
        match self {
            Self::ComponentDriver => format!("phoxal-component-{short}"),
            Self::Service => format!("phoxal-service-{short}"),
        }
    }

    /// The provider-qualified package prefix entries of this kind carry.
    const fn package_prefix(self) -> &'static str {
        match self {
            Self::Service => "phoxal/service-",
            Self::ComponentDriver => "phoxal/component-",
        }
    }
}

impl std::fmt::Display for ArtifactKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The name a runtime's executable is staged under inside `<bundle>/bin`.
///
/// There is no path field anywhere in the manifest: a launcher derives the
/// executable from the id it is launching. A service runs from its service id,
/// a component driver from its component *type*, and the brain from `brain`.
#[must_use]
pub fn bundle_binary_name(short: &str) -> String {
    short.to_string()
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

    /// The executable name `cargo install` produces for this official.
    #[must_use]
    pub fn cargo_binary_name(&self) -> String {
        self.kind.cargo_binary_name(self.short_name())
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
        package: "phoxal/service-safety",
        kind: ArtifactKind::Service,
    },
    OfficialRuntime {
        package: "phoxal/service-video",
        kind: ArtifactKind::Service,
    },
];

/// The official participant set one CLI/framework-supervisor pair interprets.
///
/// A single current snapshot, deliberately not a table keyed by framework
/// train: every binary in one execution is built on one compatibility line, and
/// the embedded compatibility record of each built binary - not a catalog
/// version - is what proves it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Catalog;

impl Catalog {
    /// The official set this CLI release ships.
    #[must_use]
    pub const fn official() -> Self {
        Self
    }

    /// The officials that run on the robot. Every one of them is mandatory: an
    /// official participant is always started, whether or not a document
    /// mentions it. A `services:` entry naming one is a configuration entry,
    /// never a declaration that it runs.
    pub fn native(&self) -> impl Iterator<Item = &'static OfficialRuntime> {
        NATIVE.iter()
    }

    /// Whether `identity` is an official service short name, so a `services:`
    /// entry claiming it is read as configuration for a catalog-owned service
    /// rather than as a declaration owing a workspace crate.
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

    /// The two names an official carries are deliberately different: the one
    /// Cargo publishes and installs it under, and the short id it is launched
    /// by inside a bundle.
    #[test]
    fn an_official_names_its_cargo_binary_and_its_bundle_entry() {
        let drive = Catalog::official()
            .native()
            .find(|official| official.short_name() == "drive")
            .expect("drive is an official service");
        assert_eq!(drive.cargo_binary_name(), "phoxal-service-drive");
        assert_eq!(drive.cargo_package_name(), "phoxal-service-drive");
        assert_eq!(bundle_binary_name(drive.short_name()), "drive");
        assert_eq!(
            ArtifactKind::ComponentDriver.cargo_binary_name("ddsm115"),
            "phoxal-component-ddsm115"
        );
        assert_eq!(bundle_binary_name("ddsm115"), "ddsm115");
    }

    /// The official robot set is services only. Adapter executables are local
    /// host tools on the framework train, never participant catalog entries.
    #[test]
    fn the_official_set_is_services_and_the_controller_is_a_host_tool() {
        let catalog = Catalog::official();
        assert!(
            catalog
                .native()
                .all(|official| official.kind == ArtifactKind::Service),
            "the official set is services only"
        );
        assert!(catalog.is_official_service("drive"));
        assert!(!catalog.is_official_service("mission"));
        assert_eq!(
            WEBOTS_ADAPTER_PACKAGES,
            [
                "phoxal-simulator-webots-host",
                "phoxal-simulator-webots-world-controller",
                "phoxal-simulator-webots-robot-controller",
            ]
        );
        for package in WEBOTS_ADAPTER_PACKAGES {
            assert!(
                catalog
                    .native()
                    .all(|official| official.cargo_package_name() != package),
                "adapter host tools are never catalog participants"
            );
        }
    }

    #[test]
    fn registry_config_arg_carries_the_live_static_registry() {
        assert_eq!(
            registry_config_arg(),
            "registries.phoxal.index=\"sparse+https://phoxal.github.io/registry/\""
        );
    }
}
