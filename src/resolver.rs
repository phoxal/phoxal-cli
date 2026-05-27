use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use phoxal_cli_core::shell;
use phoxal_utils_robot::{ComponentSource, Robot};
use semver::{Version, VersionReq};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::catalog::{DEFAULT_HOST_TOOLS, KNOWN_RUNTIME_SET_RELEASES, PlatformRuntimeCatalog};

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
    pub platform_runtimes: Vec<ResolvedPlatformRuntime>,
    pub user_runtimes: Vec<ResolvedUserRuntime>,
    pub components: Vec<ResolvedComponent>,
    pub tools: Vec<ResolvedTool>,
    pub sim_world: PathBuf,
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

pub fn resolve(
    robot: &Robot,
    catalog: &PlatformRuntimeCatalog,
    options: ResolveOptions,
) -> Result<ResolvedRobot> {
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
            Some(version) => select_override_version(version, &runtime_set_version)?,
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

    let components = resolve_components(robot, options.resolve_external_artifacts)?;
    let tools = resolve_tools(robot)?;

    Ok(ResolvedRobot {
        robot: robot.clone(),
        runtime_set_version,
        requested_runtime_set: robot.phoxal_runtimes.version.clone(),
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

pub fn resolve_git_tag(url: &str, tag: &str) -> Result<String> {
    let peeled = format!("refs/tags/{tag}^{{}}");
    let direct = format!("refs/tags/{tag}");
    let output = shell::run_stdout("git", ["ls-remote", url, peeled.as_str()], None)
        .or_else(|_| shell::run_stdout("git", ["ls-remote", url, direct.as_str()], None))
        .with_context(|| format!("failed to resolve git tag {tag} from {url}"))?;
    output
        .split_whitespace()
        .next()
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("git tag {tag} does not exist in {url}"))
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

fn resolve_components(
    robot: &Robot,
    resolve_external_artifacts: bool,
) -> Result<Vec<ResolvedComponent>> {
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
                let commit = if resolve_external_artifacts {
                    resolve_git_tag(&source.git, &source.tag)?
                } else {
                    fake_sha(&format!("{}:{}", source.git, source.tag))
                };
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
    let mut requested = DEFAULT_HOST_TOOLS
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

fn select_runtime_set_version(requested: &str, cli_req: &str) -> Result<Version> {
    let supported = VersionReq::parse(cli_req)
        .with_context(|| format!("invalid CLI supported runtime requirement {cli_req}"))?;
    let releases = known_releases()?;
    if requested == "latest" {
        return newest_matching(&releases, |version| supported.matches(version))
            .with_context(|| format!("no known runtime releases match CLI support {cli_req}"));
    }

    if let Ok(exact) = Version::parse(requested) {
        if supported.matches(&exact) && releases.contains(&exact) {
            return Ok(exact);
        }
        bail!(
            "requested runtime set version {requested} is outside CLI supported range {cli_req}; upgrade phoxal-cli or choose a supported runtime set"
        );
    }

    let user_req = VersionReq::parse(requested)
        .with_context(|| format!("invalid phoxal_runtimes.version selector '{requested}'"))?;
    newest_matching(&releases, |version| {
        supported.matches(version) && user_req.matches(version)
    })
    .with_context(|| {
        format!(
            "no known runtime releases match phoxal_runtimes.version {requested} within CLI support {cli_req}"
        )
    })
}

fn select_override_version(requested: &str, default_version: &Version) -> Result<Version> {
    if requested == "latest" {
        return known_releases()?
            .into_iter()
            .max()
            .ok_or_else(|| anyhow!("no known runtime releases are embedded in phoxal-cli"));
    }
    if let Ok(version) = Version::parse(requested) {
        return Ok(version);
    }
    let req = VersionReq::parse(requested)
        .with_context(|| format!("invalid platform runtime override version '{requested}'"))?;
    newest_matching(&known_releases()?, |version| req.matches(version))
        .or_else(|| {
            if req.matches(default_version) {
                Some(default_version.clone())
            } else {
                None
            }
        })
        .with_context(|| format!("no known runtime releases match override version {requested}"))
}

fn known_releases() -> Result<Vec<Version>> {
    KNOWN_RUNTIME_SET_RELEASES
        .iter()
        .map(|release| {
            Version::parse(release).with_context(|| format!("invalid embedded release {release}"))
        })
        .collect()
}

fn newest_matching(releases: &[Version], predicate: impl Fn(&Version) -> bool) -> Option<Version> {
    releases
        .iter()
        .filter(|version| predicate(version))
        .max()
        .cloned()
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

fn join_errors(errors: Vec<phoxal_utils_robot::ValidationError>) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}
