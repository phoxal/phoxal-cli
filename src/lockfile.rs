use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::resolver::{ResolvedComponentSource, ResolvedRobot};

pub const LOCKFILE_NAME: &str = "phoxal.lock";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lockfile {
    pub schema_version: u32,
    pub phoxal_runtimes: LockedPhoxalRuntimes,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub components: BTreeMap<String, LockedComponent>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tools: BTreeMap<String, LockedTool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedPhoxalRuntimes {
    pub requested: String,
    pub resolved: String,
    pub images: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedComponent {
    pub source: LockedComponentSource,
    pub commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LockedComponentSource {
    Git { git: String, tag: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedTool {
    pub requested: String,
    pub resolved: String,
    pub asset: String,
    pub sha256: String,
}

impl Lockfile {
    #[must_use]
    pub fn from_resolved(resolved: &ResolvedRobot) -> Self {
        let images = resolved
            .platform_runtimes
            .iter()
            .map(|runtime| (runtime.name.clone(), runtime.pinned_image()))
            .collect();
        let components = resolved
            .components
            .iter()
            .filter_map(|component| match &component.source {
                ResolvedComponentSource::Git { git, tag, commit } => Some((
                    component.source_name.clone(),
                    LockedComponent {
                        source: LockedComponentSource::Git {
                            git: git.clone(),
                            tag: tag.clone(),
                        },
                        commit: commit.clone(),
                    },
                )),
                ResolvedComponentSource::Path { .. } => None,
            })
            .collect();
        let tools = resolved
            .tools
            .iter()
            .map(|tool| {
                (
                    tool.name.clone(),
                    LockedTool {
                        requested: tool.requested.clone(),
                        resolved: tool.resolved.clone(),
                        asset: tool.asset.clone(),
                        sha256: tool.sha256.clone(),
                    },
                )
            })
            .collect();

        Self {
            schema_version: 1,
            phoxal_runtimes: LockedPhoxalRuntimes {
                requested: resolved.requested_runtime_set.clone(),
                resolved: resolved.runtime_set_version.to_string(),
                images,
            },
            components,
            tools,
        }
    }

    pub fn read(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read lockfile {}", path.display()))?;
        serde_yaml::from_str(&contents)
            .with_context(|| format!("failed to parse lockfile {}", path.display()))
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let contents = serde_yaml::to_string(self).context("failed to serialize lockfile")?;
        fs::write(path, contents)
            .with_context(|| format!("failed to write lockfile {}", path.display()))
    }
}
