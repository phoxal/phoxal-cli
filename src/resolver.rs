use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use phoxal::model::robot::{RobotV1 as Robot, v1::ComponentSource};
use semver::{Version, VersionReq};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::catalog::{
    DEFAULT_TOOL_VERSIONS, PlatformRuntimeCatalog, ToolVersionSource, lookup_tool_version,
};
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
}

#[derive(Debug, Clone, Copy)]
pub enum ReleaseSource<'a> {
    Snapshot(&'a ReleasesSnapshot),
    CacheDir(&'a Path),
    RefreshCache(&'a Path),
}

/// How a platform runtime image is referenced for deployment.
///
/// A [`ImagePin::Digest`] is a reproducible OCI content pin obtained from the
/// registry (via `docker buildx imagetools inspect`). [`ImagePin::Unpinned`]
/// means no digest was resolved — the image is deployed by its `repo:version`
/// tag. We never fabricate a digest: an unpinned image is deployed by tag,
/// which is the honest pre-publish-recovery behavior and is still a real,
/// pullable reference (it just isn't content-addressed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImagePin {
    /// A real registry-reported content digest, e.g. `sha256:…`.
    Digest(String),
    /// No digest resolved; deploy by `repo:version` tag.
    Unpinned,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPlatformRuntime {
    pub name: String,
    pub image_repo: String,
    pub version: Version,
    pub pin: ImagePin,
}

impl ResolvedPlatformRuntime {
    /// The floating tag reference, e.g. `ghcr.io/phoxal/runtime-foo:0.2.0`.
    #[must_use]
    pub fn tag_ref(&self) -> String {
        format!("{}:{}", self.image_repo, self.version)
    }

    /// The reference Docker should pull and compose should embed: a real
    /// digest pin when one was resolved, otherwise the `repo:version` tag.
    /// Never a fabricated digest — so live `simulate` can never attempt to
    /// pull a fake `repo@sha256:…`.
    #[must_use]
    pub fn deploy_ref(&self) -> String {
        match &self.pin {
            ImagePin::Digest(digest) => format!("{}@{}", self.image_repo, digest),
            ImagePin::Unpinned => self.tag_ref(),
        }
    }

    /// The real content digest, if one was resolved. `None` during
    /// pre-publish recovery (deploy-by-tag).
    #[must_use]
    pub fn digest_pin(&self) -> Option<&str> {
        match &self.pin {
            ImagePin::Digest(digest) => Some(digest.as_str()),
            ImagePin::Unpinned => None,
        }
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
        /// Subdirectory within the repository holding the component
        /// definition. `None` means the repository root.
        directory: Option<PathBuf>,
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
    pub repo: String,
    pub asset: String,
    pub binary_name: String,
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
            .unwrap_or_else(|| entry.image_repo());
        let version = match runtime_override
            .and_then(|runtime_override| runtime_override.version.as_deref())
        {
            Some(version) => {
                select_override_version(version, &runtime_set_version, &release_versions)?
            }
            None => runtime_set_version.clone(),
        };
        let image_ref = format!("{image_repo}:{version}");
        // Only attempt registry digest resolution when explicitly asked
        // (`--pin-digests`). Otherwise stay unpinned and deploy by tag — we
        // never synthesize a placeholder `sha256:` that would later be
        // mistaken for a real OCI pin.
        let pin = if options.resolve_external_artifacts {
            ImagePin::Digest(resolve_image_digest(&image_ref)?)
        } else {
            ImagePin::Unpinned
        };
        platform_runtimes.push(ResolvedPlatformRuntime {
            name: entry.name.to_string(),
            image_repo,
            version,
            pin,
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
    let tools = resolve_tools(
        robot,
        &runtime_set_version,
        options.resolve_external_artifacts,
    )?;

    Ok(ResolvedRobot {
        robot: robot.clone(),
        runtime_set_version,
        requested_runtime_set: robot.phoxal_runtimes.version.clone(),
        releases_fetched_at: Some(releases.fetched_at),
        platform_runtimes,
        user_runtimes,
        components,
        tools,
    })
}

/// Resolve a real OCI content digest for `image_ref` from the registry.
///
/// This requires `docker buildx imagetools inspect`, which reports the actual
/// registry index digest. We deliberately do **not** fall back to hashing a
/// `docker manifest inspect` body — fabricating a `sha256:` from manifest JSON
/// produces a string that looks like an OCI pin but can never be pulled. If
/// buildx cannot reach the image, fail loudly with guidance instead.
pub fn resolve_image_digest(image_ref: &str) -> Result<String> {
    let output = shell::run_stdout(
        "docker",
        [
            "buildx",
            "imagetools",
            "inspect",
            image_ref,
            "--format",
            "{{json .Manifest}}",
        ],
        None,
    )
    .with_context(|| {
        format!(
            "could not resolve a real image digest for {image_ref}. \
             `docker buildx imagetools inspect` failed — install Docker with buildx and ensure \
             the daemon can reach the registry. If the phoxal/framework GHCR runtime images are \
             not published yet, omit --pin-digests to deploy by tag during pre-publish recovery."
        )
    })?;
    buildx_imagetools_manifest_digest(&output).with_context(|| {
        format!("docker buildx imagetools inspect did not report an index digest for {image_ref}")
    })
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
                    directory: source.directory.clone(),
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

fn resolve_tools(
    robot: &Robot,
    runtime_set_version: &Version,
    resolve_external_artifacts: bool,
) -> Result<Vec<ResolvedTool>> {
    // Reject robot.yaml tool selectors for unknown tools up front (previously a
    // robot.tools entry seeded the request map; now the catalog is the source of
    // truth and overrides only adjust known, pinned tools).
    for name in robot.tools.keys() {
        if lookup_tool_version(name).is_none() {
            bail!("unknown native tool '{name}'");
        }
    }
    let target = host_target_triple();
    DEFAULT_TOOL_VERSIONS
        .iter()
        .map(|catalog_tool| {
            let name = catalog_tool.name;
            let override_version = robot.tools.get(name).map(|tool| tool.version.as_str());
            let version = match catalog_tool.version {
                ToolVersionSource::RuntimeTrain => {
                    // Train-tracked tools are version-matched to the runtime set
                    // by construction; a per-robot pin would silently desync the
                    // simulator binaries from the runtimes/crate. (The "no Webots
                    // build for this host" check lives in the live `simulate`
                    // path, where the binaries are actually required — see
                    // commands/simulate.rs — so validate/dry-run stay usable on
                    // hosts without a Webots build.)
                    if let Some(requested) = override_version {
                        bail!(
                            "tool '{name}' tracks the runtime version train and cannot be pinned \
                             via robot.yaml tools.{name}.version (got '{requested}'); it always \
                             matches phoxal_runtimes.version (resolved {runtime_set_version})"
                        );
                    }
                    runtime_set_version.to_string()
                }
                ToolVersionSource::Pinned(pinned) => override_version.unwrap_or(pinned).to_string(),
            };
            let asset = render_tool_template(catalog_tool.artifact_template, &version, &target);
            let binary_name = render_tool_template(catalog_tool.binary_template, &version, &target);
            let sha256 = if resolve_external_artifacts {
                resolve_release_asset_sha256(catalog_tool.repo, &version, &asset)?
                    .unwrap_or_default()
            } else {
                fake_sha(&format!("{name}:{version}:{asset}"))
            };
            Ok(ResolvedTool {
                name: name.to_string(),
                requested: version.clone(),
                resolved: version,
                repo: catalog_tool.repo.to_string(),
                asset,
                binary_name,
                sha256,
            })
        })
        .collect()
}

fn render_tool_template(template: &str, version: &str, target: &str) -> String {
    template
        .replace("{version}", version)
        .replace("{target}", target)
}

fn resolve_release_asset_sha256(repo: &str, version: &str, asset: &str) -> Result<Option<String>> {
    let url = format!("https://api.github.com/repos/{repo}/releases/tags/v{version}");
    let client = reqwest::blocking::Client::builder()
        .user_agent("phoxal-cli")
        .timeout(Duration::from_secs(15))
        .build()?;
    let mut req = client.get(&url);
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        req = req.bearer_auth(token);
    }
    let response = req
        .send()
        .with_context(|| format!("failed to fetch GitHub release {repo} v{version}"))?;
    if !response.status().is_success() {
        let status = response.status();
        if is_unpublished_placeholder(version) {
            return Ok(None);
        }
        if status.as_u16() == 403 {
            bail!(
                "GitHub releases API returned 403 while resolving {repo} v{version} \
                 (likely anonymous rate limit of 60/hour from this network - wait an hour, \
                 or set GITHUB_TOKEN env var for 5000/hour)"
            );
        }
        bail!("GitHub release lookup for {repo} v{version} returned {status}");
    }

    #[derive(Deserialize)]
    struct GhAsset {
        name: String,
        digest: Option<String>,
    }

    #[derive(Deserialize)]
    struct GhRelease {
        assets: Vec<GhAsset>,
    }

    let release: GhRelease = response
        .json()
        .with_context(|| format!("failed to parse GitHub release {repo} v{version}"))?;
    let Some(release_asset) = release
        .assets
        .iter()
        .find(|candidate| candidate.name == asset)
    else {
        if is_unpublished_placeholder(version) {
            return Ok(None);
        }
        bail!("GitHub release {repo} v{version} does not contain asset {asset}");
    };
    let Some(digest) = release_asset.digest.as_deref() else {
        if is_unpublished_placeholder(version) {
            return Ok(None);
        }
        bail!("GitHub release asset {repo} v{version} {asset} does not expose a digest");
    };
    let sha256 = digest.strip_prefix("sha256:").unwrap_or(digest);
    if sha256.len() == 64 && sha256.chars().all(|value| value.is_ascii_hexdigit()) {
        Ok(Some(sha256.to_ascii_lowercase()))
    } else if is_unpublished_placeholder(version) {
        Ok(None)
    } else {
        bail!("GitHub release asset {repo} v{version} {asset} has invalid sha256 digest {digest}")
    }
}

fn is_unpublished_placeholder(version: &str) -> bool {
    version == "0.0.0-dev"
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
        // A platform-runtime override must still resolve to a real published
        // runtime release — never a fabricated version. (The runtime-set path
        // enforces the same membership check; this branch previously returned
        // the parsed version without verifying it exists.)
        if !releases.contains(&version) {
            bail!("platform runtime override version {requested} is not a known runtime release");
        }
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

fn buildx_imagetools_manifest_digest(output: &str) -> Result<String> {
    let value: Value =
        serde_json::from_str(output).context("docker buildx imagetools output was not JSON")?;
    value
        .as_object()
        .and_then(|object| object.get("digest").or_else(|| object.get("Digest")))
        .and_then(Value::as_str)
        .filter(|digest| digest.starts_with("sha256:"))
        .map(ToString::to_string)
        .ok_or_else(|| anyhow!("manifest index digest not found"))
}

/// Deterministic non-cryptographic stand-in hash for **tool asset** entries
/// during offline resolution. Never used for OCI image digests — image pins
/// are either a real registry digest or an honest tag ref (see [`ImagePin`]).
fn fake_sha(seed: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(seed.as_bytes());
    hex::encode(hasher.finalize())
}

fn is_floating_selector(selector: &str) -> bool {
    selector == "latest" || Version::parse(selector).is_err()
}

fn join_errors(errors: Vec<phoxal::model::robot::ValidationError>) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buildx_imagetools_digest_uses_index_digest_not_platform_leaf() -> anyhow::Result<()> {
        let output = r#"{
  "mediaType": "application/vnd.oci.image.index.v1+json",
  "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "manifests": [
    {
      "digest": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
      "platform": {
        "architecture": "amd64",
        "os": "linux"
      }
    },
    {
      "digest": "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
      "platform": {
        "architecture": "arm64",
        "os": "linux"
      }
    }
  ]
}"#;

        assert_eq!(
            buildx_imagetools_manifest_digest(output)?,
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );

        Ok(())
    }

    #[test]
    fn buildx_imagetools_digest_rejects_non_sha256_digest() {
        // A registry that reports a non-sha256 digest must not be accepted as a
        // pin — we never coerce a bogus value into a `sha256:` string.
        let output = r#"{"digest": "md5:deadbeef"}"#;
        assert!(buildx_imagetools_manifest_digest(output).is_err());
    }

    #[test]
    fn override_exact_version_must_be_a_known_release() {
        let default = Version::parse("0.9.0").unwrap();
        let releases = vec![Version::parse("0.8.0").unwrap(), default.clone()];

        // A real published version is accepted as-is.
        assert_eq!(
            select_override_version("0.8.0", &default, &releases).unwrap(),
            Version::parse("0.8.0").unwrap()
        );

        // A fabricated version that is not in the release list is rejected,
        // rather than being returned unchecked.
        let err = select_override_version("0.7.99", &default, &releases).unwrap_err();
        assert!(
            err.to_string().contains("not a known runtime release"),
            "unexpected error: {err}"
        );
    }

    fn runtime(pin: ImagePin) -> ResolvedPlatformRuntime {
        ResolvedPlatformRuntime {
            name: "asset".to_string(),
            image_repo: "ghcr.io/phoxal/runtime-asset".to_string(),
            version: Version::parse("0.2.0").expect("valid version"),
            pin,
        }
    }

    #[test]
    fn unpinned_runtime_deploys_by_tag_not_fabricated_digest() {
        let runtime = runtime(ImagePin::Unpinned);
        assert_eq!(runtime.tag_ref(), "ghcr.io/phoxal/runtime-asset:0.2.0");
        // The deploy ref is the tag — no `@sha256:` is ever invented.
        assert_eq!(runtime.deploy_ref(), "ghcr.io/phoxal/runtime-asset:0.2.0");
        assert!(!runtime.deploy_ref().contains('@'));
        assert_eq!(runtime.digest_pin(), None);
    }

    #[test]
    fn pinned_runtime_deploys_by_digest() {
        let digest = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let runtime = runtime(ImagePin::Digest(digest.to_string()));
        assert_eq!(
            runtime.deploy_ref(),
            format!("ghcr.io/phoxal/runtime-asset@{digest}")
        );
        assert_eq!(runtime.digest_pin(), Some(digest));
    }
}
