use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformRuntimeEntry {
    pub name: &'static str,
    pub image_repo: &'static str,
    pub default_env: &'static [(&'static str, &'static str)],
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
}

pub const DEFAULT_TOOL_VERSIONS: &[ToolVersion] = &[
    ToolVersion {
        name: "simulator_webots",
        version: "0.0.0-dev",
    },
    ToolVersion {
        name: "rerun_proxy",
        version: "0.0.0-dev",
    },
    ToolVersion {
        name: "joypad",
        version: "0.0.0-dev",
    },
];

pub const CATALOG: PlatformRuntimeCatalog = PlatformRuntimeCatalog {
    supported_runtimes_version_req: "*",
    entries: &[
        entry("router", "ghcr.io/phoxal/runtime-router", false, false),
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
        default_env: &[("PHOXAL_ROBOT_DIR", "/robot")],
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

    pub fn env_by_runtime(
        &self,
    ) -> BTreeMap<&'static str, &'static [(&'static str, &'static str)]> {
        self.entries
            .iter()
            .map(|entry| (entry.name, entry.default_env))
            .collect()
    }
}
