#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformRuntimeService {
    pub name: &'static str,
    pub uses_supervisor_api: bool,
}

pub const PLATFORM_RUNTIME_SERVICES: &[PlatformRuntimeService] = &[
    PlatformRuntimeService {
        name: "router",
        uses_supervisor_api: false,
    },
    PlatformRuntimeService {
        name: "asset",
        uses_supervisor_api: false,
    },
    PlatformRuntimeService {
        name: "presence",
        uses_supervisor_api: false,
    },
    PlatformRuntimeService {
        name: "power",
        uses_supervisor_api: true,
    },
    PlatformRuntimeService {
        name: "video",
        uses_supervisor_api: false,
    },
    PlatformRuntimeService {
        name: "joint",
        uses_supervisor_api: false,
    },
    PlatformRuntimeService {
        name: "frame",
        uses_supervisor_api: false,
    },
    PlatformRuntimeService {
        name: "odometry",
        uses_supervisor_api: false,
    },
    PlatformRuntimeService {
        name: "localize",
        uses_supervisor_api: false,
    },
    PlatformRuntimeService {
        name: "map",
        uses_supervisor_api: false,
    },
    PlatformRuntimeService {
        name: "safety",
        uses_supervisor_api: false,
    },
    PlatformRuntimeService {
        name: "mission",
        uses_supervisor_api: false,
    },
    PlatformRuntimeService {
        name: "explore",
        uses_supervisor_api: false,
    },
    PlatformRuntimeService {
        name: "plan",
        uses_supervisor_api: false,
    },
    PlatformRuntimeService {
        name: "follow",
        uses_supervisor_api: false,
    },
    PlatformRuntimeService {
        name: "motion",
        uses_supervisor_api: false,
    },
    PlatformRuntimeService {
        name: "drive",
        uses_supervisor_api: false,
    },
];

pub const PLATFORM_RUNTIME_NAMES: &[&str] = &[
    "router", "asset", "presence", "power", "video", "joint", "frame", "odometry", "localize",
    "map", "safety", "mission", "explore", "plan", "follow", "motion", "drive",
];

pub fn is_platform_runtime(name: &str) -> bool {
    PLATFORM_RUNTIME_NAMES.contains(&name)
}
