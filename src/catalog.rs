#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformRuntimeEntry {
    pub name: &'static str,
    pub image_repo: &'static str,
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
    // The Webots controller + supervisor now ship from phoxal/framework in the
    // same release as the platform runtimes, so they ride the runtime version
    // train rather than a hand-bumped pin (was phoxal/simulator @ 0.2.0).
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
        version: ToolVersionSource::Pinned("0.1.0"),
        repo: "phoxal/joypad",
        artifact_template: "phoxal-joypad-{version}-{target}.tar.gz",
        binary_template: "phoxal-joypad-{target}",
    },
];

pub fn lookup_tool_version(name: &str) -> Option<&'static ToolVersion> {
    DEFAULT_TOOL_VERSIONS.iter().find(|tool| tool.name == name)
}

pub const CATALOG: PlatformRuntimeCatalog = PlatformRuntimeCatalog {
    supported_runtimes_version_req: SUPPORTED_RUNTIME_TRAIN,
    entries: &[
        entry("asset", "ghcr.io/phoxal/runtime-asset", false, true),
        entry("presence", "ghcr.io/phoxal/runtime-presence", false, true),
        entry("frame", "ghcr.io/phoxal/runtime-frame", false, true),
        entry("safety", "ghcr.io/phoxal/runtime-safety", false, true),
        entry("drive", "ghcr.io/phoxal/runtime-drive", false, true),
        entry("localize", "ghcr.io/phoxal/runtime-localize", false, true),
        entry("map", "ghcr.io/phoxal/runtime-map", false, true),
        entry("mission", "ghcr.io/phoxal/runtime-mission", false, true),
        entry("plan", "ghcr.io/phoxal/runtime-plan", false, true),
        entry(
            "perception",
            "ghcr.io/phoxal/runtime-perception",
            false,
            true,
        ),
        entry("motion", "ghcr.io/phoxal/runtime-motion", false, true),
        entry("odometry", "ghcr.io/phoxal/runtime-odometry", false, true),
        entry("joint", "ghcr.io/phoxal/runtime-joint", false, true),
        entry("video", "ghcr.io/phoxal/runtime-video", false, true),
        entry("power", "ghcr.io/phoxal/runtime-power", true, true),
        entry("follow", "ghcr.io/phoxal/runtime-follow", false, true),
        entry("explore", "ghcr.io/phoxal/runtime-explore", false, true),
    ],
};

const fn entry(
    name: &'static str,
    image_repo: &'static str,
    uses_supervisor_api: bool,
    wires_to_router: bool,
) -> PlatformRuntimeEntry {
    PlatformRuntimeEntry {
        name,
        image_repo,
        uses_supervisor_api,
        wires_to_router,
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
    use semver::{Version, VersionReq};

    use super::SUPPORTED_RUNTIME_TRAIN;

    #[test]
    fn supported_runtime_train_is_valid_and_covers_current_zero_x_train() {
        let train = SUPPORTED_RUNTIME_TRAIN
            .parse::<VersionReq>()
            .expect("supported runtime train should parse as a semver requirement");

        assert!(train.matches(&Version::parse("0.9.0").unwrap()));
        assert!(!train.matches(&Version::parse("1.0.0").unwrap()));
    }
}
