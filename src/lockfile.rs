use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::releases::ReleasesSnapshot;
use crate::resolver::{ImagePin, ResolvedComponentSource, ResolvedRobot, ResolvedUserRuntimeBuild};

pub const LOCKFILE_NAME: &str = "phoxal.lock";
pub const LOCKFILE_SCHEMA_VERSION: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lockfile {
    pub schema_version: u32,
    pub phoxal_runtimes: LockedPhoxalRuntimes,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub user_runtimes: BTreeMap<String, LockedUserRuntime>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub components: BTreeMap<String, LockedComponent>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tools: BTreeMap<String, LockedTool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedPhoxalRuntimes {
    pub requested: String,
    pub resolved: String,
    pub releases_fetched_at: Option<DateTime<Utc>>,
    /// Per-runtime image deploy ref. Either a content pin
    /// (`repo@sha256:…`, reproducible) or a tag ref (`repo:version`,
    /// recovery / not yet pinned). The lockfile never stores a fabricated
    /// digest.
    pub images: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedUserRuntime {
    pub path: PathBuf,
    pub framework: String,
    pub source_hash: String,
    pub image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<ResolvedUserRuntimeBuild>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedComponent {
    pub source: LockedComponentSource,
    pub commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LockedComponentSource {
    Git {
        git: String,
        tag: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        directory: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedTool {
    pub requested: String,
    pub resolved: String,
    #[serde(default)]
    pub repo: String,
    pub asset: String,
    #[serde(default)]
    pub binary_name: String,
    pub sha256: String,
}

impl Lockfile {
    #[must_use]
    pub fn from_resolved(resolved: &ResolvedRobot) -> Self {
        // Store the deploy ref: a real `repo@sha256:…` pin when resolved, or a
        // `repo:version` tag ref during recovery. Never a fabricated digest, so
        // phoxal.lock never presents a fake runtime pin as reproducible.
        let images = resolved
            .platform_runtimes
            .iter()
            .map(|runtime| (runtime.name.clone(), runtime.deploy_ref()))
            .collect();
        let user_runtimes = resolved
            .user_runtimes
            .iter()
            .map(|runtime| {
                (
                    runtime.name.clone(),
                    LockedUserRuntime {
                        path: runtime.path.clone(),
                        framework: runtime.framework.clone(),
                        source_hash: runtime.source_hash.clone(),
                        image: runtime.image.clone(),
                        build: runtime.build.clone(),
                    },
                )
            })
            .collect();
        let components = resolved
            .components
            .iter()
            .filter_map(|component| match &component.source {
                ResolvedComponentSource::Git {
                    git,
                    tag,
                    commit,
                    directory,
                } => Some((
                    component.source_name.clone(),
                    LockedComponent {
                        source: LockedComponentSource::Git {
                            git: git.clone(),
                            tag: tag.clone(),
                            directory: directory.clone(),
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
                        repo: tool.repo.clone(),
                        asset: tool.asset.clone(),
                        binary_name: tool.binary_name.clone(),
                        sha256: tool.sha256.clone(),
                    },
                )
            })
            .collect();

        Self {
            schema_version: LOCKFILE_SCHEMA_VERSION,
            phoxal_runtimes: LockedPhoxalRuntimes {
                requested: resolved.requested_runtime_set.clone(),
                resolved: resolved.runtime_set_version.to_string(),
                releases_fetched_at: resolved.releases_fetched_at.map(DateTime::<Utc>::from),
                images,
            },
            user_runtimes,
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

    #[must_use]
    pub fn is_drift_free(&self, resolved: &ResolvedRobot) -> bool {
        self == &Self::from_resolved(resolved)
    }

    #[must_use]
    pub fn has_current_schema(&self) -> bool {
        self.schema_version == LOCKFILE_SCHEMA_VERSION
    }
}

pub(crate) fn releases_snapshot_from_lockfile(lockfile: &Lockfile) -> Result<ReleasesSnapshot> {
    let resolved_version = lockfile
        .phoxal_runtimes
        .resolved
        .parse::<semver::Version>()?;
    Ok(ReleasesSnapshot {
        fetched_at: lockfile
            .phoxal_runtimes
            .releases_fetched_at
            .map(SystemTime::from)
            .unwrap_or(SystemTime::UNIX_EPOCH),
        versions: vec![resolved_version.to_string()],
    })
}

pub(crate) fn apply_lockfile(lockfile: &Lockfile, resolved: &mut ResolvedRobot) -> Result<()> {
    if !lockfile.has_current_schema() {
        bail!(
            "lockfile schema_version {} is stale; regenerate {} with schema_version {}",
            lockfile.schema_version,
            LOCKFILE_NAME,
            LOCKFILE_SCHEMA_VERSION
        );
    }
    if lockfile.phoxal_runtimes.requested != resolved.requested_runtime_set {
        bail!(
            "lockfile requested runtime-set selector '{}' does not match robot.yaml selector '{}'",
            lockfile.phoxal_runtimes.requested,
            resolved.requested_runtime_set
        );
    }
    resolved.runtime_set_version =
        lockfile.phoxal_runtimes.resolved.parse().with_context(|| {
            format!(
                "lockfile runtime-set version '{}' is not semver",
                lockfile.phoxal_runtimes.resolved
            )
        })?;

    let mut runtime_names = BTreeSet::new();
    for runtime in &mut resolved.platform_runtimes {
        runtime_names.insert(runtime.name.clone());
        let image = lockfile
            .phoxal_runtimes
            .images
            .get(&runtime.name)
            .with_context(|| format!("lockfile is missing image for runtime {}", runtime.name))?;
        if let Some((image_repo, image_digest)) = image.split_once('@') {
            // Reproducible content pin: repo@sha256:…
            runtime.image_repo = image_repo.to_string();
            runtime.pin = ImagePin::Digest(image_digest.to_string());
        } else {
            // Recovery tag ref: repo:version (the last colon separates the tag,
            // leaving an optional registry-port colon in the repo intact).
            let (image_repo, version) = image.rsplit_once(':').with_context(|| {
                format!(
                    "lockfile image '{image}' for runtime {} is neither a digest pin \
                     (repo@sha256:…) nor a tag ref (repo:version)",
                    runtime.name
                )
            })?;
            runtime.image_repo = image_repo.to_string();
            runtime.version = version.parse().with_context(|| {
                format!(
                    "lockfile image tag '{version}' for runtime {} is not a semver version",
                    runtime.name
                )
            })?;
            runtime.pin = ImagePin::Unpinned;
        }
    }
    for name in lockfile.phoxal_runtimes.images.keys() {
        if !runtime_names.contains(name) {
            bail!("lockfile has image for unknown runtime {name}");
        }
    }

    let mut user_runtime_names = BTreeSet::new();
    for runtime in &mut resolved.user_runtimes {
        user_runtime_names.insert(runtime.name.clone());
        let locked = lockfile
            .user_runtimes
            .get(&runtime.name)
            .with_context(|| format!("lockfile is missing user runtime {}", runtime.name))?;
        if locked.path != runtime.path
            || locked.framework != runtime.framework
            || locked.build != runtime.build
        {
            bail!(
                "lockfile user runtime {} does not match robot.yaml; re-run `phoxal update` to regenerate phoxal.lock",
                runtime.name
            );
        }
        if locked.source_hash != runtime.source_hash {
            bail!(
                "user runtime {} source tree drifted from lockfile: current hash {} does not match locked hash {}",
                runtime.name,
                runtime.source_hash,
                locked.source_hash
            );
        }
        if locked.image != runtime.image {
            bail!(
                "lockfile image '{}' for user runtime {} does not match resolved image '{}'",
                locked.image,
                runtime.name,
                runtime.image
            );
        }
        runtime.image = locked.image.clone();
        runtime.source_hash = locked.source_hash.clone();
    }
    for name in lockfile.user_runtimes.keys() {
        if !user_runtime_names.contains(name) {
            bail!("lockfile has unknown user runtime {name}");
        }
    }

    let mut component_sources = BTreeSet::new();
    for component in &mut resolved.components {
        let ResolvedComponentSource::Git {
            git,
            tag,
            commit,
            directory,
        } = &mut component.source
        else {
            continue;
        };
        component_sources.insert(component.source_name.clone());
        let locked = lockfile
            .components
            .get(&component.source_name)
            .with_context(|| {
                format!(
                    "lockfile is missing component source {}",
                    component.source_name
                )
            })?;
        let crate::lockfile::LockedComponentSource::Git {
            git: locked_git,
            tag: locked_tag,
            directory: locked_directory,
        } = &locked.source;
        if locked_git != git || locked_tag != tag || locked_directory != directory {
            bail!(
                "lockfile component source {} does not match robot.yaml",
                component.source_name
            );
        }
        *commit = locked.commit.clone();
    }
    for name in lockfile.components.keys() {
        if !component_sources.contains(name) {
            bail!("lockfile has component source {name} that robot.yaml no longer uses");
        }
    }

    let mut tool_names = BTreeSet::new();
    for tool in &mut resolved.tools {
        tool_names.insert(tool.name.clone());
        let locked = lockfile
            .tools
            .get(&tool.name)
            .with_context(|| format!("lockfile is missing tool {}", tool.name))?;
        if locked.requested != tool.requested {
            bail!(
                "lockfile tool {} does not match robot.yaml; re-run `phoxal update` to regenerate phoxal.lock",
                tool.name
            );
        }
        tool.resolved = locked.resolved.clone();
        if !locked.repo.is_empty() {
            tool.repo = locked.repo.clone();
        }
        tool.asset = locked.asset.clone();
        if !locked.binary_name.is_empty() {
            tool.binary_name = locked.binary_name.clone();
        }
        tool.sha256 = locked.sha256.clone();
    }
    for name in lockfile.tools.keys() {
        if !tool_names.contains(name) {
            bail!("lockfile has unknown tool {name}");
        }
    }

    Ok(())
}

pub(crate) fn reconcile_lockfile(
    project_root: &Path,
    resolved: &ResolvedRobot,
    locked: bool,
) -> Result<Option<PathBuf>> {
    let lock_path = project_root.join(LOCKFILE_NAME);
    let expected = Lockfile::from_resolved(resolved);
    if lock_path.is_file() {
        let actual = Lockfile::read(&lock_path)?;
        if actual.is_drift_free(resolved) {
            return Ok(None);
        }
        if locked {
            bail!("{} differs from recomputed resolution", lock_path.display());
        }
    } else if locked {
        bail!("{} is required by --locked", lock_path.display());
    }
    expected.write(&lock_path)?;
    Ok(Some(lock_path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::{ResolvedRobot, ResolvedUserRuntime};
    use phoxal::model::robot::RobotV1 as Robot;
    use semver::Version;
    use std::time::SystemTime;

    #[test]
    fn schema_version_is_bumped_for_user_runtime_locks() {
        assert_eq!(LOCKFILE_SCHEMA_VERSION, 3);
    }

    #[test]
    fn locked_user_runtime_round_trips() {
        let mut user_runtimes = BTreeMap::new();
        user_runtimes.insert(
            "autonomy".to_string(),
            LockedUserRuntime {
                path: PathBuf::from("runtimes/autonomy"),
                framework: "0.9.0".to_string(),
                source_hash: "abc123".to_string(),
                image: "phoxal-local/testbot/user-runtime/autonomy:abc123".to_string(),
                build: Some(ResolvedUserRuntimeBuild {
                    context: PathBuf::from("container"),
                    dockerfile: Some(PathBuf::from("Dockerfile.runtime")),
                    target: Some("runtime".to_string()),
                }),
            },
        );
        let lockfile = Lockfile {
            schema_version: LOCKFILE_SCHEMA_VERSION,
            phoxal_runtimes: LockedPhoxalRuntimes {
                requested: "latest".to_string(),
                resolved: "0.9.0".to_string(),
                releases_fetched_at: None,
                images: BTreeMap::new(),
            },
            user_runtimes,
            components: BTreeMap::new(),
            tools: BTreeMap::new(),
        };

        let yaml = serde_yaml::to_string(&lockfile).expect("serialize");
        assert!(yaml.contains("user_runtimes:"), "got: {yaml}");
        assert!(yaml.contains("source_hash: abc123"), "got: {yaml}");
        let reparsed: Lockfile = serde_yaml::from_str(&yaml).expect("reparse");
        assert_eq!(reparsed, lockfile);
    }

    #[test]
    fn user_runtime_source_hash_changes_are_drift() -> anyhow::Result<()> {
        let resolved = resolved_with_user_runtime("abc123")?;
        let lockfile = Lockfile::from_resolved(&resolved);
        assert!(lockfile.is_drift_free(&resolved));

        let mut changed = resolved.clone();
        changed.user_runtimes[0].source_hash = "def456".to_string();
        changed.user_runtimes[0].image =
            "phoxal-local/testbot/user-runtime/autonomy:def456".to_string();

        assert!(!lockfile.is_drift_free(&changed));
        Ok(())
    }

    #[test]
    fn locked_component_directory_round_trips() {
        let locked = LockedComponent {
            source: LockedComponentSource::Git {
                git: "https://github.com/phoxal/components".to_string(),
                tag: "v0.3.0".to_string(),
                directory: Some(PathBuf::from("bno085")),
            },
            commit: "abc123".to_string(),
        };
        let yaml = serde_yaml::to_string(&locked).expect("serialize");
        assert!(yaml.contains("directory: bno085"), "got: {yaml}");
        let reparsed: LockedComponent = serde_yaml::from_str(&yaml).expect("reparse");
        assert_eq!(reparsed, locked);
    }

    #[test]
    fn locked_component_omits_absent_directory() {
        let locked = LockedComponent {
            source: LockedComponentSource::Git {
                git: "https://github.com/phoxal/component-bno085".to_string(),
                tag: "main".to_string(),
                directory: None,
            },
            commit: "abc123".to_string(),
        };
        let yaml = serde_yaml::to_string(&locked).expect("serialize");
        assert!(!yaml.contains("directory"), "got: {yaml}");
        // A pre-`directory` lockfile (no field) still parses.
        let legacy = "source:\n  git: https://github.com/phoxal/component-bno085\n  tag: main\ncommit: abc123\n";
        let parsed: LockedComponent = serde_yaml::from_str(legacy).expect("legacy parse");
        assert_eq!(parsed, locked);
    }

    fn resolved_with_user_runtime(source_hash: &str) -> anyhow::Result<ResolvedRobot> {
        Ok(ResolvedRobot {
            robot: Robot::parse_from_string(MINIMAL_ROBOT)?,
            runtime_set_version: Version::parse("0.9.0")?,
            requested_runtime_set: "latest".to_string(),
            releases_fetched_at: Some(SystemTime::UNIX_EPOCH),
            platform_runtimes: Vec::new(),
            user_runtimes: vec![ResolvedUserRuntime {
                name: "autonomy".to_string(),
                path: PathBuf::from("runtimes/autonomy"),
                framework: "0.9.0".to_string(),
                build: Some(ResolvedUserRuntimeBuild {
                    context: PathBuf::from("container"),
                    dockerfile: Some(PathBuf::from("Dockerfile.runtime")),
                    target: Some("runtime".to_string()),
                }),
                source_hash: source_hash.to_string(),
                image: format!("phoxal-local/testbot/user-runtime/autonomy:{source_hash}"),
            }],
            components: Vec::new(),
            tools: Vec::new(),
        })
    }

    const MINIMAL_ROBOT: &str = r#"schema: v0
api_version: y2026_1

identity:
  id: testbot
  namespace: test

structure: structure.urdf

phoxal_runtimes:
  version: "latest"

motion:
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5

components:
  sources: {}
  instances: {}
"#;
}
