use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use phoxal::model::robot::{
    RobotV1 as Robot,
    v1::{Channel, ComponentSource, UserRuntime},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::catalog::{DEFAULT_TOOL_VERSIONS, PlatformRuntimeCatalog, lookup_tool_version};
use crate::shell;
use crate::utils::{hash_tree, resolve_project_path};

const ROBOT_FILE: &str = "robot.yaml";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolveOptions {
    pub locked: bool,
    pub resolve_external_artifacts: bool,
}

impl Default for ResolveOptions {
    fn default() -> Self {
        Self {
            locked: false,
            resolve_external_artifacts: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedRobot {
    pub robot: Robot,
    pub api_version: String,
    pub channel: Channel,
    pub platform_runtimes: Vec<ResolvedPlatformRuntime>,
    pub user_runtimes: Vec<ResolvedUserRuntime>,
    pub components: Vec<ResolvedComponent>,
    pub tools: Vec<ResolvedTool>,
}

/// How a platform runtime image is referenced for deployment.
///
/// A [`ImagePin::Digest`] is a reproducible OCI content pin obtained from the
/// registry (via `docker buildx imagetools inspect`). [`ImagePin::Unpinned`]
/// means no digest was resolved — the image is deployed by its selected tag
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
    pub image_ref: String,
    pub pin: ImagePin,
}

impl ResolvedPlatformRuntime {
    /// The selected image reference, e.g. `ghcr.io/phoxal/runtime-drive:y2026_1-stable`.
    #[must_use]
    pub fn tag_ref(&self) -> String {
        self.image_ref.clone()
    }

    /// The reference Docker should pull and compose should embed: a real
    /// digest pin when one was resolved, otherwise the selected image ref.
    /// Never a fabricated digest — so live `simulate` can never attempt to
    /// pull a fake `ref@sha256:…`.
    #[must_use]
    pub fn deploy_ref(&self) -> String {
        if image_ref_digest(&self.image_ref).is_some() {
            return self.image_ref.clone();
        }
        match &self.pin {
            ImagePin::Digest(digest) => format!("{}@{}", self.image_ref, digest),
            ImagePin::Unpinned => self.image_ref.clone(),
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
    pub framework: String,
    pub build: Option<ResolvedUserRuntimeBuild>,
    pub source_hash: String,
    pub image: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedUserRuntimeBuild {
    pub context: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dockerfile: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
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

pub fn resolve(
    robot: &Robot,
    project_root: &Path,
    catalog: &PlatformRuntimeCatalog,
    options: ResolveOptions,
) -> Result<ResolvedRobot> {
    let api_version = robot.api_version.clone();
    let channel = robot.phoxal_runtimes.channel;
    let platform_names = catalog.names_for_api(&api_version);
    if platform_names.is_empty() {
        bail!(
            "API version {api_version} is not available in the compiled-in platform runtime catalog"
        );
    }
    robot
        .validate_with(&platform_names)
        .map_err(|errors| anyhow!("Robot errors:\n{}", join_errors(errors)))?;

    let mut platform_runtimes = Vec::new();
    for entry in catalog.entries_for_api(&api_version) {
        let image_ref = robot
            .phoxal_runtimes
            .images
            .get(entry.name)
            .cloned()
            .unwrap_or_else(|| {
                format!(
                    "{}:{}-{}",
                    entry.image_repo(),
                    api_version,
                    channel.as_str()
                )
            });
        // Only attempt registry digest resolution when explicitly asked
        // (`--pin-digests`). Otherwise stay unpinned and deploy by tag — we
        // never synthesize a placeholder `sha256:` that would later be
        // mistaken for a real OCI pin.
        let pin = if let Some(digest) = image_ref_digest(&image_ref) {
            ImagePin::Digest(digest.to_string())
        } else if options.resolve_external_artifacts {
            ImagePin::Digest(resolve_image_digest(&image_ref)?)
        } else {
            ImagePin::Unpinned
        };
        platform_runtimes.push(ResolvedPlatformRuntime {
            name: entry.name.to_string(),
            image_ref,
            pin,
        });
    }

    let user_runtimes = robot
        .user_runtimes
        .iter()
        .map(|(name, runtime)| {
            resolve_user_runtime(
                project_root,
                &robot.identity.id,
                &api_version,
                name,
                runtime,
            )
        })
        .collect::<Result<Vec<_>>>()?;

    // Git ref → commit SHA resolution is cheap (no Docker, just network) and
    // is required for `simulate --dry-run` to assemble a usable .phoxal/run/
    // tree. We always attempt it; the only thing the offline path skips is
    // image digest pinning.
    let components = resolve_components(robot)?;
    let tools = resolve_tools(robot, options.resolve_external_artifacts)?;

    Ok(ResolvedRobot {
        robot: robot.clone(),
        api_version,
        channel,
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

fn resolve_user_runtime(
    project_root: &Path,
    robot_id: &str,
    api_version: &str,
    name: &str,
    runtime: &UserRuntime,
) -> Result<ResolvedUserRuntime> {
    let runtime_dir = resolve_project_path(project_root, &runtime.path);
    if !runtime_dir.is_dir() {
        bail!(
            "user runtime '{name}' source dir {} does not exist; user runtimes must have an on-disk source directory to hash/build",
            runtime_dir.display()
        );
    }
    let framework = resolve_user_runtime_framework(name, &runtime.framework, api_version)?;
    let source_hash = hash_tree(&runtime_dir).with_context(|| {
        format!(
            "failed to hash user runtime '{name}' source tree at {}",
            runtime_dir.display()
        )
    })?;
    let image = format!("phoxal-local/{robot_id}/user-runtime/{name}:{source_hash}");
    let build = runtime
        .build
        .as_ref()
        .map(mirror_user_runtime_build)
        .transpose()?;

    Ok(ResolvedUserRuntime {
        name: name.to_string(),
        path: runtime.path.clone(),
        framework,
        build,
        source_hash,
        image,
    })
}

fn mirror_user_runtime_build(build: &impl Serialize) -> Result<ResolvedUserRuntimeBuild> {
    let value =
        serde_yaml::to_value(build).context("failed to serialize user runtime build recipe")?;
    serde_yaml::from_value(value).context("failed to mirror user runtime build recipe")
}

pub(crate) fn resolve_user_runtime_framework(
    runtime_name: &str,
    selector: &str,
    api_version: &str,
) -> Result<String> {
    validate_user_runtime_framework_selector(runtime_name, selector, api_version)?;
    if selector == "match-platform" {
        return Ok(api_version.to_string());
    }
    Ok(selector.to_string())
}

pub(crate) fn validate_user_runtime_framework_selector(
    runtime_name: &str,
    selector: &str,
    api_version: &str,
) -> Result<()> {
    if selector == "match-platform" || selector == api_version {
        return Ok(());
    }
    bail!(
        "user runtime '{runtime_name}': framework '{selector}' must be \"match-platform\" or the graph api_version '{api_version}'"
    )
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

fn resolve_tools(robot: &Robot, resolve_external_artifacts: bool) -> Result<Vec<ResolvedTool>> {
    // Reject robot.yaml tool selectors for unknown tools up front (previously a
    // robot.tools entry seeded the request map; now the catalog is the source of
    // truth and robot.yaml may only pin known tools to explicit versions).
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
            let version = override_version
                .unwrap_or(catalog_tool.default_version)
                .to_string();
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

pub(crate) fn resolve_release_asset_sha256(
    repo: &str,
    version: &str,
    asset: &str,
) -> Result<Option<String>> {
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

fn image_ref_digest(image_ref: &str) -> Option<&str> {
    image_ref
        .rsplit_once('@')
        .map(|(_, digest)| digest)
        .filter(|digest| digest.starts_with("sha256:"))
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

    fn runtime(pin: ImagePin) -> ResolvedPlatformRuntime {
        ResolvedPlatformRuntime {
            name: "asset".to_string(),
            image_ref: "ghcr.io/phoxal/runtime-asset:y2026_1-stable".to_string(),
            pin,
        }
    }

    #[test]
    fn unpinned_runtime_deploys_by_tag_not_fabricated_digest() {
        let runtime = runtime(ImagePin::Unpinned);
        assert_eq!(
            runtime.tag_ref(),
            "ghcr.io/phoxal/runtime-asset:y2026_1-stable"
        );
        // The deploy ref is the tag — no `@sha256:` is ever invented.
        assert_eq!(
            runtime.deploy_ref(),
            "ghcr.io/phoxal/runtime-asset:y2026_1-stable"
        );
        assert!(!runtime.deploy_ref().contains('@'));
        assert_eq!(runtime.digest_pin(), None);
    }

    #[test]
    fn pinned_runtime_deploys_by_digest() {
        let digest = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let runtime = runtime(ImagePin::Digest(digest.to_string()));
        assert_eq!(
            runtime.deploy_ref(),
            format!("ghcr.io/phoxal/runtime-asset:y2026_1-stable@{digest}")
        );
        assert_eq!(runtime.digest_pin(), Some(digest));
    }

    #[test]
    fn digest_override_deploys_as_the_override_ref_without_double_pin() {
        let digest = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let runtime = ResolvedPlatformRuntime {
            name: "asset".to_string(),
            image_ref: format!("ghcr.io/phoxal/runtime-asset@{digest}"),
            pin: ImagePin::Digest(digest.to_string()),
        };

        assert_eq!(runtime.deploy_ref(), runtime.image_ref);
        assert_eq!(runtime.digest_pin(), Some(digest));
    }
}
