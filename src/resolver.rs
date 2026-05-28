use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow, bail};
use phoxal_robot::{RobotV1 as Robot, v1::ComponentSource};
use semver::{Version, VersionReq};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::catalog::{DEFAULT_TOOL_VERSIONS, PlatformRuntimeCatalog};
use crate::releases::{self, ReleasesSnapshot};
use crate::shell;

const ROBOT_FILE: &str = "robot.yaml";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolveOptions {
    pub locked: bool,
    pub allow_floating: bool,
    pub resolve_external_artifacts: bool,
}

impl Default for ResolveOptions {
    fn default() -> Self {
        Self {
            locked: false,
            allow_floating: true,
            resolve_external_artifacts: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedRobot {
    pub robot: Robot,
    pub runtime_set_version: Version,
    pub requested_runtime_set: String,
    pub releases_fetched_at: Option<SystemTime>,
    pub platform_runtimes: Vec<ResolvedPlatformRuntime>,
    pub user_runtimes: Vec<ResolvedUserRuntime>,
    pub components: Vec<ResolvedComponent>,
    pub tools: Vec<ResolvedTool>,
    pub sim_world: PathBuf,
}

#[derive(Debug, Clone, Copy)]
pub enum ReleaseSource<'a> {
    Snapshot(&'a ReleasesSnapshot),
    CacheDir(&'a Path),
    RefreshCache(&'a Path),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPlatformRuntime {
    pub name: String,
    pub image_repo: String,
    pub version: Version,
    pub image_digest: String,
}

impl ResolvedPlatformRuntime {
    #[must_use]
    pub fn pinned_image(&self) -> String {
        format!("{}@{}", self.image_repo, self.image_digest)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedUserRuntime {
    pub name: String,
    pub path: PathBuf,
    pub image_tag: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedComponent {
    pub instance: String,
    pub source_name: String,
    pub source: ResolvedComponentSource,
    pub has_driver: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedComponentSource {
    Git {
        git: String,
        tag: String,
        commit: String,
    },
    Path {
        path: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTool {
    pub name: String,
    pub requested: String,
    pub resolved: String,
    pub asset: String,
    pub sha256: String,
}

pub fn discover_robot_yaml(start: &Path) -> Result<PathBuf> {
    let mut cursor = if start.is_file() {
        start
            .parent()
            .ok_or_else(|| anyhow!("cannot discover robot.yaml above {}", start.display()))?
            .to_path_buf()
    } else {
        start.to_path_buf()
    };

    loop {
        let candidate = cursor.join(ROBOT_FILE);
        if candidate.is_file() {
            return Ok(candidate);
        }
        if !cursor.pop() {
            bail!("failed to discover robot.yaml above {}", start.display());
        }
    }
}

pub fn load_robot(path: &Path) -> Result<Robot> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read robot file {}", path.display()))?;
    Robot::read_from_string(&contents)
}

pub fn resolve_with_release_source(
    robot: &Robot,
    catalog: &PlatformRuntimeCatalog,
    options: ResolveOptions,
    release_source: ReleaseSource<'_>,
) -> Result<ResolvedRobot> {
    let snapshot = match release_source {
        ReleaseSource::Snapshot(snapshot) => snapshot.clone(),
        ReleaseSource::CacheDir(cache_dir) => releases::known_releases_snapshot(cache_dir)?,
        ReleaseSource::RefreshCache(cache_dir) => releases::refresh(cache_dir)?,
    };
    resolve_with_releases(robot, catalog, options, &snapshot)
}

pub fn resolve_with_releases(
    robot: &Robot,
    catalog: &PlatformRuntimeCatalog,
    options: ResolveOptions,
    releases: &ReleasesSnapshot,
) -> Result<ResolvedRobot> {
    let release_versions = releases.versions_semver()?;
    if !options.allow_floating && is_floating_selector(&robot.phoxal_runtimes.version) {
        bail!(
            "phoxal_runtimes.version '{}' is floating but floating resolution is disabled",
            robot.phoxal_runtimes.version
        );
    }

    let platform_names = catalog.names_vec();
    robot
        .validate_with(&platform_names)
        .map_err(|errors| anyhow!("Robot errors:\n{}", join_errors(errors)))?;

    let runtime_set_version = select_runtime_set_version(
        &robot.phoxal_runtimes.version,
        catalog.supported_runtimes_version_req,
        &release_versions,
    )?;

    let mut override_names = BTreeSet::new();
    let mut platform_runtimes = Vec::new();
    for entry in catalog.entries {
        let runtime_override = robot.phoxal_runtimes.overrides.get(entry.name);
        if runtime_override.is_some() {
            override_names.insert(entry.name.to_string());
        }
        let image_repo = runtime_override
            .and_then(|runtime_override| runtime_override.image.clone())
            .unwrap_or_else(|| entry.image_repo.to_string());
        let version = match runtime_override
            .and_then(|runtime_override| runtime_override.version.as_deref())
        {
            Some(version) => {
                select_override_version(version, &runtime_set_version, &release_versions)?
            }
            None => runtime_set_version.clone(),
        };
        let image_ref = format!("{image_repo}:{version}");
        let image_digest = if options.resolve_external_artifacts {
            resolve_image_digest(&image_ref)?
        } else {
            fake_digest(&image_ref)
        };
        platform_runtimes.push(ResolvedPlatformRuntime {
            name: entry.name.to_string(),
            image_repo,
            version,
            image_digest,
        });
    }

    for runtime_name in robot.phoxal_runtimes.overrides.keys() {
        if !override_names.contains(runtime_name) {
            bail!("unknown platform runtime override '{runtime_name}'");
        }
    }

    let user_runtimes = robot
        .user_runtimes
        .iter()
        .map(|(name, runtime)| ResolvedUserRuntime {
            name: name.clone(),
            path: runtime.path.clone(),
            image_tag: format!(
                "phoxal-local/{}/user-runtime/{}:unbuilt",
                robot.identity.id, name
            ),
        })
        .collect();

    // Git ref → commit SHA resolution is cheap (no Docker, just network) and
    // is required for `simulate --dry-run` to assemble a usable .phoxal/run/
    // tree. We always attempt it; the only thing the offline path skips is
    // image digest pinning.
    let components = resolve_components(robot)?;
    let tools = resolve_tools(robot)?;

    Ok(ResolvedRobot {
        robot: robot.clone(),
        runtime_set_version,
        requested_runtime_set: robot.phoxal_runtimes.version.clone(),
        releases_fetched_at: Some(releases.fetched_at),
        platform_runtimes,
        user_runtimes,
        components,
        tools,
        sim_world: robot.sim.world.clone(),
    })
}

pub fn resolve_image_digest(image_ref: &str) -> Result<String> {
    let output = shell::run_stdout(
        "docker",
        ["manifest", "inspect", "--verbose", image_ref],
        None,
    )
    .with_context(|| {
        format!(
            "failed to inspect Docker manifest for {image_ref}; install Docker and ensure the daemon is running"
        )
    })?;
    docker_manifest_digest(&output)
        .with_context(|| format!("docker manifest inspect did not report a digest for {image_ref}"))
}

pub fn resolve_git_ref(url: &str, git_ref: &str) -> Result<String> {
    let candidates = [
        format!("refs/tags/{git_ref}^{{}}"),
        format!("refs/tags/{git_ref}"),
        format!("refs/heads/{git_ref}"),
        git_ref.to_string(),
    ];
    for candidate in candidates {
        let output = shell::run_stdout("git", ["ls-remote", url, candidate.as_str()], None)
            .with_context(|| format!("failed to resolve git ref {git_ref} from {url}"))?;
        if let Some(commit) = output
            .split_whitespace()
            .next()
            .filter(|value| !value.is_empty())
        {
            return Ok(commit.to_string());
        }
    }
    Err(anyhow!("git ref {git_ref} does not exist in {url}"))
}

pub fn host_target_triple() -> String {
    std::env::var("PHOXAL_HOST_TARGET_TRIPLE").unwrap_or_else(|_| {
        let arch = std::env::consts::ARCH;
        let os = match std::env::consts::OS {
            "macos" => "apple-darwin",
            "linux" => "unknown-linux-gnu",
            "windows" => "pc-windows-msvc",
            other => other,
        };
        format!("{arch}-{os}")
    })
}

fn resolve_components(robot: &Robot) -> Result<Vec<ResolvedComponent>> {
    let mut components = Vec::new();
    for (instance_name, instance) in &robot.components.instances {
        let source = robot
            .components
            .sources
            .get(&instance.component)
            .ok_or_else(|| {
                anyhow!(
                    "components.instances.{}.component references missing source '{}'",
                    instance_name,
                    instance.component
                )
            })?;
        let source = match source {
            ComponentSource::Git(source) => {
                let commit = resolve_git_ref(&source.git, &source.tag)?;
                ResolvedComponentSource::Git {
                    git: source.git.clone(),
                    tag: source.tag.clone(),
                    commit,
                }
            }
            ComponentSource::Path(source) => ResolvedComponentSource::Path {
                path: source.path.clone(),
            },
        };
        components.push(ResolvedComponent {
            instance: instance_name.clone(),
            source_name: instance.component.clone(),
            source,
            has_driver: instance.driver.is_some(),
        });
    }
    Ok(components)
}

fn resolve_tools(robot: &Robot) -> Result<Vec<ResolvedTool>> {
    let mut requested = DEFAULT_TOOL_VERSIONS
        .iter()
        .map(|tool| (tool.name.to_string(), tool.version.to_string()))
        .collect::<BTreeMap<_, _>>();
    for (name, tool) in &robot.tools {
        requested.insert(name.clone(), tool.version.clone());
    }
    let target = host_target_triple();
    Ok(requested
        .into_iter()
        .map(|(name, version)| {
            let asset = format!("{}-{}-{}.tar.gz", name.replace('_', "-"), version, target);
            let sha256 = fake_sha(&format!("{name}:{version}:{asset}"));
            ResolvedTool {
                name,
                requested: version.clone(),
                resolved: version,
                asset,
                sha256,
            }
        })
        .collect())
}

fn select_runtime_set_version(
    requested: &str,
    cli_req: &str,
    releases: &[Version],
) -> Result<Version> {
    let supported = VersionReq::parse(cli_req)
        .with_context(|| format!("invalid CLI supported runtime requirement {cli_req}"))?;
    if requested == "latest" {
        return newest_matching(releases, |version| {
            supported_matches(cli_req, &supported, version)
        })
        .with_context(|| format!("no known runtime releases match CLI support {cli_req}"));
    }

    if let Ok(exact) = Version::parse(requested) {
        if supported_matches(cli_req, &supported, &exact) && releases.contains(&exact) {
            return Ok(exact);
        }
        bail!(
            "your CLI doesn't know how to compose runtime-set version {requested}; supported runtime-set requirement is {cli_req}"
        );
    }

    let user_req = VersionReq::parse(requested)
        .with_context(|| format!("invalid phoxal_runtimes.version selector '{requested}'"))?;
    newest_matching(releases, |version| {
        supported_matches(cli_req, &supported, version) && user_req.matches(version)
    })
    .with_context(|| {
        format!(
            "your CLI doesn't know how to compose runtime-set version selector {requested}; supported runtime-set requirement is {cli_req}"
        )
    })
}

fn select_override_version(
    requested: &str,
    default_version: &Version,
    releases: &[Version],
) -> Result<Version> {
    if requested == "latest" {
        return releases
            .iter()
            .max()
            .cloned()
            .ok_or_else(|| anyhow!("no known runtime releases are available"));
    }
    if let Ok(version) = Version::parse(requested) {
        return Ok(version);
    }
    let req = VersionReq::parse(requested)
        .with_context(|| format!("invalid platform runtime override version '{requested}'"))?;
    newest_matching(releases, |version| req.matches(version))
        .or_else(|| {
            if req.matches(default_version) {
                Some(default_version.clone())
            } else {
                None
            }
        })
        .with_context(|| format!("no known runtime releases match override version {requested}"))
}

fn newest_matching(releases: &[Version], predicate: impl Fn(&Version) -> bool) -> Option<Version> {
    releases
        .iter()
        .filter(|version| predicate(version))
        .max()
        .cloned()
}

fn supported_matches(cli_req: &str, supported: &VersionReq, version: &Version) -> bool {
    cli_req == "*" || supported.matches(version)
}

fn docker_manifest_digest(output: &str) -> Result<String> {
    let value: Value =
        serde_json::from_str(output).context("docker manifest output was not JSON")?;
    find_digest(&value).ok_or_else(|| anyhow!("manifest digest not found"))
}

fn find_digest(value: &Value) -> Option<String> {
    match value {
        Value::Object(object) => {
            for key in ["digest", "Digest"] {
                if let Some(digest) = object.get(key).and_then(Value::as_str)
                    && digest.starts_with("sha256:")
                {
                    return Some(digest.to_string());
                }
            }
            object.values().find_map(find_digest)
        }
        Value::Array(values) => values.iter().find_map(find_digest),
        _ => None,
    }
}

fn fake_digest(seed: &str) -> String {
    format!("sha256:{}", fake_sha(seed))
}

fn fake_sha(seed: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(seed.as_bytes());
    hex::encode(hasher.finalize())
}

fn is_floating_selector(selector: &str) -> bool {
    selector == "latest" || Version::parse(selector).is_err()
}

fn join_errors(errors: Vec<phoxal_robot::ValidationError>) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}
