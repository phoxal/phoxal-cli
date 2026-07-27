//! CLI-owned official runtime catalog.
//!
//! Official services, tools, and the infrastructure router are never
//! discovered from a network inventory: they are compiled into this CLI
//! release. `robot.yaml` declares USER services/tools and component
//! instances; the official set comes from here alone (organization#951 WS4).

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

    #[test]
    fn registry_config_arg_carries_the_live_static_registry() {
        assert_eq!(
            registry_config_arg(),
            "registries.phoxal.index=\"sparse+https://phoxal.github.io/registry/\""
        );
    }
}
