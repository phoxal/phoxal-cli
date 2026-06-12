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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolVersion {
    pub name: &'static str,
    pub version: &'static str,
    pub repo: &'static str,
    pub artifact_template: &'static str,
    pub binary_template: &'static str,
}

// Must track the `phoxal` dependency line in Cargo.toml.
pub const SUPPORTED_RUNTIME_TRAIN: &str = "^0.7";

pub const DEFAULT_TOOL_VERSIONS: &[ToolVersion] = &[
    ToolVersion {
        name: "simulator_webots_controller",
        version: "0.2.0",
        repo: "phoxal/simulator",
        artifact_template: "phoxal-simulator-{version}-{target}.tar.gz",
        binary_template: "phoxal-simulator-webots-controller-{target}",
    },
    ToolVersion {
        name: "simulator_webots_supervisor",
        version: "0.2.0",
        repo: "phoxal/simulator",
        artifact_template: "phoxal-simulator-{version}-{target}.tar.gz",
        binary_template: "phoxal-simulator-webots-supervisor-{target}",
    },
    ToolVersion {
        name: "rerun_proxy",
        version: "0.1.0",
        repo: "phoxal/operator",
        artifact_template: "phoxal-rerun-proxy-{version}-{target}.tar.gz",
        binary_template: "phoxal-rerun-proxy-{target}",
    },
    ToolVersion {
        name: "joypad",
        version: "0.1.0",
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
