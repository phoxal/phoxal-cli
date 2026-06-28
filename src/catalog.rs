#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformRuntimeEntry {
    pub name: &'static str,
    /// API versions for which this runtime's official image set is published.
    ///
    /// Add another API version here only after the framework side has rebuilt
    /// and published the runtime's official images for that API; API
    /// inheritance alone does not make an image available.
    pub api_versions: &'static [&'static str],
    pub uses_supervisor_api: bool,
    pub wires_to_router: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformRuntimeCatalog {
    pub entries: &'static [PlatformRuntimeEntry],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolVersion {
    pub name: &'static str,
    pub default_version: &'static str,
    pub repo: &'static str,
    pub artifact_template: &'static str,
    pub binary_template: &'static str,
}

pub const DEFAULT_TOOL_VERSIONS: &[ToolVersion] = &[
    ToolVersion {
        name: "simulator_webots_controller",
        default_version: "0.14.0",
        repo: "phoxal/framework",
        artifact_template: "phoxal-simulator-{version}-{target}.tar.gz",
        binary_template: "phoxal-simulator-webots-controller-{target}",
    },
    ToolVersion {
        name: "simulator_webots_supervisor",
        default_version: "0.14.0",
        repo: "phoxal/framework",
        artifact_template: "phoxal-simulator-{version}-{target}.tar.gz",
        binary_template: "phoxal-simulator-webots-supervisor-{target}",
    },
    ToolVersion {
        name: "rerun_proxy",
        default_version: "0.1.0",
        repo: "phoxal/operator",
        artifact_template: "phoxal-rerun-proxy-{version}-{target}.tar.gz",
        binary_template: "phoxal-rerun-proxy-{target}",
    },
    ToolVersion {
        name: "joypad",
        default_version: "0.14.0",
        repo: "phoxal/framework",
        artifact_template: "phoxal-joypad-{version}-{target}.tar.gz",
        binary_template: "phoxal-joypad-{target}",
    },
];

pub fn lookup_tool_version(name: &str) -> Option<&'static ToolVersion> {
    DEFAULT_TOOL_VERSIONS.iter().find(|tool| tool.name == name)
}

pub const CATALOG: PlatformRuntimeCatalog = PlatformRuntimeCatalog {
    // The default image repo for each runtime is derived from its name
    // (`ghcr.io/phoxal/runtime-<name>`, see `PlatformRuntimeEntry::image_repo`),
    // so the catalog carries only the name, API availability, and topology flags.
    // A robot may still override the full image ref per runtime via
    // `phoxal_runtimes.images.<name>`.
    //
    // The runtime NAMES here must mirror the `runtime/<name>` crate directories
    // in `phoxal/framework`; that cross-repo invariant is enforced in framework
    // release CI, not here (the framework tree isn't present at CLI build time).
    entries: &[
        entry("asset", &["y2026_1"], false, true),
        entry("drive", &["y2026_1"], false, true),
        entry("driver-ddsm115", &["y2026_1"], false, true),
        entry("localize", &["y2026_1"], false, true),
        entry("map", &["y2026_1"], false, true),
        entry("mission", &["y2026_1"], false, true),
        entry("odometry", &["y2026_1"], false, true),
        entry("battery", &["y2026_2"], false, true),
        entry("safety", &["y2026_2"], false, true),
    ],
};

const fn entry(
    name: &'static str,
    api_versions: &'static [&'static str],
    uses_supervisor_api: bool,
    wires_to_router: bool,
) -> PlatformRuntimeEntry {
    PlatformRuntimeEntry {
        name,
        api_versions,
        uses_supervisor_api,
        wires_to_router,
    }
}

impl PlatformRuntimeEntry {
    /// Default GHCR repo for this runtime, derived from its name. A per-robot
    /// `images.<name>` full image ref takes precedence over this in the resolver.
    pub fn image_repo(&self) -> String {
        format!("ghcr.io/phoxal/runtime-{}", self.name)
    }
}

impl PlatformRuntimeCatalog {
    pub fn entries_for_api<'a>(
        &'a self,
        api_version: &'a str,
    ) -> impl Iterator<Item = &'a PlatformRuntimeEntry> + 'a {
        self.entries
            .iter()
            .filter(move |entry| entry.api_versions.contains(&api_version))
    }

    pub fn names(&self) -> impl Iterator<Item = &'static str> {
        self.entries.iter().map(|entry| entry.name)
    }

    pub fn names_for_api(&self, api_version: &str) -> Vec<&'static str> {
        self.entries_for_api(api_version)
            .map(|entry| entry.name)
            .collect()
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
    use super::CATALOG;
    use std::collections::BTreeSet;

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
                    && entry.name.chars().all(|c| c.is_ascii_lowercase()
                        || c.is_ascii_digit()
                        || c == '_'
                        || c == '-'),
                "runtime name must be a lowercase token: {}",
                entry.name
            );
            assert!(
                !entry.api_versions.is_empty(),
                "runtime {} must declare at least one api_version",
                entry.name
            );
            for api_version in entry.api_versions {
                assert!(
                    !api_version.is_empty(),
                    "runtime {} must not declare an empty api_version",
                    entry.name
                );
            }
        }
    }

    #[test]
    fn entries_for_api_returns_the_official_set_for_that_api() {
        assert_eq!(
            CATALOG.names_for_api("y2026_1"),
            vec![
                "asset",
                "drive",
                "driver-ddsm115",
                "localize",
                "map",
                "mission",
                "odometry"
            ]
        );
        assert_eq!(CATALOG.names_for_api("y2026_2"), vec!["battery", "safety"]);
        assert!(CATALOG.names_for_api("y2099_1").is_empty());
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
