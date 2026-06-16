#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformRuntimeEntry {
    pub name: &'static str,
    pub uses_supervisor_api: bool,
    pub wires_to_router: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformRuntimeCatalog {
    pub supported_runtimes_version_req: &'static str,
    pub entries: &'static [PlatformRuntimeEntry],
}

/// Where a tool's version comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolVersionSource {
    /// Pinned to a fixed version, bumped manually as that tool releases.
    Pinned(&'static str),
    /// Tracks the resolved platform-runtime version (the framework release
    /// train). Used for tools that ship from `phoxal/framework` in the same
    /// release as the runtimes, so they are always version-matched.
    RuntimeTrain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolVersion {
    pub name: &'static str,
    pub version: ToolVersionSource,
    pub repo: &'static str,
    pub artifact_template: &'static str,
    pub binary_template: &'static str,
}

// The CLI can orchestrate any pre-1.0 runtime train at or above this floor:
// Phoxal's 0.x wire contracts are append-only, so framework minors remain
// compatible. Bump the floor only when the CLI drops an old train, not for
// every framework minor.
pub const SUPPORTED_RUNTIME_TRAIN: &str = ">=0.8.0, <1.0.0";

pub const DEFAULT_TOOL_VERSIONS: &[ToolVersion] = &[
    // The Webots simulator binaries and joypad ship from phoxal/framework on
    // the same release as the platform runtimes, so they ride the runtime
    // version train rather than hand-bumped pins.
    ToolVersion {
        name: "simulator_webots_controller",
        version: ToolVersionSource::RuntimeTrain,
        repo: "phoxal/framework",
        artifact_template: "phoxal-simulator-{version}-{target}.tar.gz",
        binary_template: "phoxal-simulator-webots-controller-{target}",
    },
    ToolVersion {
        name: "simulator_webots_supervisor",
        version: ToolVersionSource::RuntimeTrain,
        repo: "phoxal/framework",
        artifact_template: "phoxal-simulator-{version}-{target}.tar.gz",
        binary_template: "phoxal-simulator-webots-supervisor-{target}",
    },
    ToolVersion {
        name: "rerun_proxy",
        version: ToolVersionSource::Pinned("0.1.0"),
        repo: "phoxal/operator",
        artifact_template: "phoxal-rerun-proxy-{version}-{target}.tar.gz",
        binary_template: "phoxal-rerun-proxy-{target}",
    },
    ToolVersion {
        name: "joypad",
        version: ToolVersionSource::RuntimeTrain,
        repo: "phoxal/framework",
        artifact_template: "phoxal-joypad-{version}-{target}.tar.gz",
        binary_template: "phoxal-joypad-{target}",
    },
];

pub fn lookup_tool_version(name: &str) -> Option<&'static ToolVersion> {
    DEFAULT_TOOL_VERSIONS.iter().find(|tool| tool.name == name)
}

pub const CATALOG: PlatformRuntimeCatalog = PlatformRuntimeCatalog {
    supported_runtimes_version_req: SUPPORTED_RUNTIME_TRAIN,
    // The default image repo for each runtime is derived from its name
    // (`ghcr.io/phoxal/runtime-<name>`, see `PlatformRuntimeEntry::image_repo`),
    // so the catalog carries only the name + topology flags — no parallel list
    // of repo strings to keep in sync. A robot may still override the repo per
    // runtime via `phoxal_runtimes.overrides.<name>.image`.
    //
    // The runtime NAMES here must mirror the `runtime/<name>` crate directories
    // in `phoxal/framework`; that cross-repo invariant is enforced in framework
    // release CI, not here (the framework tree isn't present at CLI build time).
    entries: &[
        entry("asset", false, true),
        entry("presence", false, true),
        entry("frame", false, true),
        entry("safety", false, true),
        entry("drive", false, true),
        entry("localize", false, true),
        entry("map", false, true),
        entry("mission", false, true),
        entry("plan", false, true),
        entry("perception", false, true),
        entry("motion", false, true),
        entry("odometry", false, true),
        entry("joint", false, true),
        entry("video", false, true),
        entry("power", true, true),
        entry("follow", false, true),
        entry("explore", false, true),
    ],
};

const fn entry(
    name: &'static str,
    uses_supervisor_api: bool,
    wires_to_router: bool,
) -> PlatformRuntimeEntry {
    PlatformRuntimeEntry {
        name,
        uses_supervisor_api,
        wires_to_router,
    }
}

impl PlatformRuntimeEntry {
    /// Default GHCR repo for this runtime, derived from its name. A per-robot
    /// `overrides.<name>.image` takes precedence over this in the resolver.
    pub fn image_repo(&self) -> String {
        format!("ghcr.io/phoxal/runtime-{}", self.name)
    }
}

impl PlatformRuntimeCatalog {
    pub fn names(&self) -> impl Iterator<Item = &'static str> {
        self.entries.iter().map(|entry| entry.name)
    }

    pub fn lookup(&self, name: &str) -> Option<&PlatformRuntimeEntry> {
        self.entries.iter().find(|entry| entry.name == name)
    }

    pub fn names_vec(&self) -> Vec<&'static str> {
        self.names().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{CATALOG, SUPPORTED_RUNTIME_TRAIN};
    use semver::{Version, VersionReq};
    use std::collections::BTreeSet;

    #[test]
    fn supported_runtime_train_is_valid_and_covers_current_zero_x_train() {
        let train = SUPPORTED_RUNTIME_TRAIN
            .parse::<VersionReq>()
            .expect("supported runtime train should parse as a semver requirement");

        assert!(train.matches(&Version::parse("0.9.0").unwrap()));
        assert!(!train.matches(&Version::parse("1.0.0").unwrap()));
    }

    #[test]
    fn catalog_runtime_names_are_unique_lowercase_tokens() {
        let mut seen = BTreeSet::new();
        for entry in CATALOG.entries {
            assert!(
                seen.insert(entry.name),
                "duplicate runtime name in catalog: {}",
                entry.name
            );
            assert!(
                !entry.name.is_empty()
                    && entry
                        .name
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c == '_'),
                "runtime name must be a lowercase token: {}",
                entry.name
            );
        }
    }

    #[test]
    fn image_repo_is_derived_from_name() {
        for entry in CATALOG.entries {
            assert_eq!(
                entry.image_repo(),
                format!("ghcr.io/phoxal/runtime-{}", entry.name)
            );
        }
    }
}
