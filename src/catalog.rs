//! Consumption and selection of the framework's `phoxal.catalog/v0` index.
//!
//! The catalog is a location/integrity index only. The CLI owns artifact kind
//! and package expectations; participant metadata is always read from staged
//! binaries and is never synthesized from this document.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};

pub use phoxal::catalog::{Artifact, Blob, BuildProvenance, Catalog, Heads, OFFICIAL_SERVICES};

pub const DEFAULT_CATALOG_URL: &str =
    "https://github.com/phoxal/framework/releases/latest/download/catalog.json";
pub const CATALOG_SOURCE_ENV: &str = "PHOXAL_ARTIFACT_CATALOG";
pub const OFFLINE_ENV: &str = "PHOXAL_OFFLINE";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionChannel {
    Stable,
    Nightly,
}

impl SelectionChannel {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Nightly => "nightly",
        }
    }
}

impl std::fmt::Display for SelectionChannel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Map the dependency's temporary internal enum onto the locked CLI model.
/// Robot YAML accepts `nightly` only; `load_robot_with_extras` translates it
/// before framework deserialization while explicitly rejecting `preview`.
#[must_use]
pub const fn selection_channel(channel: phoxal::model::robot::v0::Channel) -> SelectionChannel {
    match channel {
        phoxal::model::robot::v0::Channel::Stable => SelectionChannel::Stable,
        phoxal::model::robot::v0::Channel::Preview => SelectionChannel::Nightly,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogLoadOptions {
    pub cli_source: Option<String>,
    pub robot_source: Option<PathBuf>,
    pub offline: bool,
}

impl CatalogLoadOptions {
    #[must_use]
    pub fn explicit_source(&self) -> Option<String> {
        self.cli_source
            .clone()
            .or_else(|| std::env::var(CATALOG_SOURCE_ENV).ok())
            .or_else(|| {
                self.robot_source
                    .as_ref()
                    .map(|path| path.display().to_string())
            })
    }

    #[must_use]
    pub fn is_default_source(&self) -> bool {
        self.explicit_source().is_none()
    }
}

/// Load one catalog without following heads. Custom URLs and local paths are
/// intentionally frozen single-catalog sources.
pub fn load_catalog(options: CatalogLoadOptions, interactive: bool) -> Result<Option<Catalog>> {
    if options.offline || offline_from_env() {
        return Ok(None);
    }
    match options.explicit_source() {
        Some(source) => read_source(&source, interactive).map(Some),
        None => fetch_https(DEFAULT_CATALOG_URL, interactive)
            .map(Some)
            .map_err(|error| {
                anyhow!(
                    "artifact catalog unavailable at {DEFAULT_CATALOG_URL}: {error:#}; no binaries can be resolved on a first run"
                )
            }),
    }
}

fn offline_from_env() -> bool {
    std::env::var(OFFLINE_ENV)
        .is_ok_and(|value| !value.is_empty() && !matches!(value.as_str(), "0" | "false" | "no"))
}

/// Resolve the snapshot used by a channel at first resolve/update time.
/// Stable follows the latest catalog's stable head to that release's frozen
/// catalog; nightly uses the latest snapshot. Non-default sources never chase
/// their informational head tags.
pub fn load_pinned_catalog(
    options: CatalogLoadOptions,
    channel: SelectionChannel,
    interactive: bool,
) -> Result<Option<Catalog>> {
    let is_default = options.is_default_source();
    let Some(latest) = load_catalog(options, interactive)? else {
        return Ok(None);
    };
    if !is_default {
        return Ok(Some(latest));
    }

    let head = match channel {
        SelectionChannel::Stable => {
            ensure!(
                !latest.heads.stable.is_empty(),
                "catalog heads.stable is empty; opt in explicitly with `artifacts.channel: nightly` in robot.yaml"
            );
            &latest.heads.stable
        }
        SelectionChannel::Nightly => {
            ensure!(
                !latest.heads.nightly.is_empty(),
                "catalog heads.nightly is empty; the framework catalog has no published build snapshot"
            );
            &latest.heads.nightly
        }
    };
    if head == &latest.build.tag {
        return Ok(Some(latest));
    }
    let url = snapshot_catalog_url(head)?;
    fetch_https(&url, interactive)
        .with_context(|| format!("failed to fetch frozen {channel} snapshot {head} from {url}"))
        .map(Some)
}

pub fn snapshot_catalog_url(tag: &str) -> Result<String> {
    ensure!(
        !tag.is_empty()
            && tag
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')),
        "catalog head contains an invalid release tag: {tag:?}"
    );
    Ok(format!(
        "https://github.com/phoxal/framework/releases/download/{tag}/catalog.json"
    ))
}

fn read_source(source: &str, interactive: bool) -> Result<Catalog> {
    if source.starts_with("https://") {
        fetch_https(source, interactive)
            .with_context(|| format!("failed to fetch artifact catalog from {source}"))
    } else if source.starts_with("http://") {
        bail!("artifact catalog source must use HTTPS or a local path: {source}");
    } else {
        read_catalog_path(Path::new(source))
    }
}

pub fn read_catalog_path(path: &Path) -> Result<Catalog> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    parse_catalog(&text).with_context(|| format!("{} is not a valid catalog", path.display()))
}

fn parse_catalog(text: &str) -> Result<Catalog> {
    let catalog: Catalog = serde_json::from_str(text).context("catalog was not valid JSON")?;
    ensure!(
        catalog.schema == phoxal::catalog::SCHEMA,
        "unsupported catalog schema {:?}; expected {:?}",
        catalog.schema,
        phoxal::catalog::SCHEMA
    );
    validate_catalog(&catalog)?;
    Ok(catalog)
}

fn validate_catalog(catalog: &Catalog) -> Result<()> {
    let mut identities = BTreeSet::new();
    for artifact in &catalog.artifacts {
        ensure!(
            !artifact.package.is_empty() && !artifact.version.is_empty(),
            "catalog contains an artifact with an empty package or version"
        );
        ensure!(
            identities.insert((&artifact.package, &artifact.version)),
            "catalog contains duplicate ({}, {}) entries",
            artifact.package,
            artifact.version
        );
        for (target, blob) in &artifact.targets {
            validate_blob(blob).with_context(|| {
                format!(
                    "invalid target blob for {} {} ({target})",
                    artifact.package, artifact.version
                )
            })?;
        }
        if let Some(blob) = &artifact.assets {
            validate_blob(blob).with_context(|| {
                format!(
                    "invalid assets blob for {} {}",
                    artifact.package, artifact.version
                )
            })?;
        }
    }
    Ok(())
}

fn validate_blob(blob: &Blob) -> Result<()> {
    ensure!(blob.url.starts_with("https://"), "blob URL must use HTTPS");
    ensure!(
        phoxal::catalog::is_sha256(&blob.sha256),
        "blob sha256 must be lowercase hexadecimal"
    );
    ensure!(blob.size > 0, "blob size must be greater than zero");
    Ok(())
}

fn fetch_https(url: &str, interactive: bool) -> Result<Catalog> {
    let progress =
        crate::progress::spinner(format!("fetching artifact catalog from {url}"), interactive);
    match fetch_https_inner(url) {
        Ok(catalog) => {
            progress.finish_and_clear();
            Ok(catalog)
        }
        Err(error) => {
            progress.abandon_with_message(format!("failed to fetch artifact catalog: {error:#}"));
            Err(error)
        }
    }
}

fn fetch_https_inner(url: &str) -> Result<Catalog> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("phoxal-cli")
        .connect_timeout(REQUEST_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()?;
    let response = client.get(url).send()?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        bail!("the framework catalog is not published yet: {url} returned HTTP {status}");
    }
    if !status.is_success() {
        bail!("catalog request {url} returned HTTP {status}");
    }
    parse_catalog(&response.text()?)
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ArtifactKind {
    Service,
    ComponentAssets,
    ComponentDriver,
    Tool,
    Simulator,
    Infrastructure,
}

impl ArtifactKind {
    #[must_use]
    pub const fn emit_apis_kind(self) -> &'static str {
        match self {
            Self::Service => "service",
            Self::ComponentAssets => "component_assets",
            Self::ComponentDriver => "driver",
            Self::Tool => "tool",
            Self::Simulator => "simulator",
            Self::Infrastructure => "infrastructure",
        }
    }

    #[must_use]
    pub const fn catalog_kind(self) -> &'static str {
        match self {
            Self::Service => "service",
            Self::ComponentAssets => "component_assets",
            Self::ComponentDriver => "component_driver",
            Self::Tool => "tool",
            Self::Simulator => "simulator",
            Self::Infrastructure => "infrastructure",
        }
    }
}

impl std::fmt::Display for ArtifactKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.catalog_kind())
    }
}

/// Standard site tools: every one of these is a hard-required participant.
/// `resolver::resolve` fails the whole robot if any is absent from the active
/// catalog snapshot - a catalog that cannot resolve the current standard set
/// is outdated, and the remediation is `phoxal update`, not a silent
/// degrade (product decision 9).
pub const OFFICIAL_TOOLS: &[(&str, &str)] = &[
    ("bus", "phoxal/tool-bus"),
    ("joypad", "phoxal/tool-joypad"),
    ("telemetry", "phoxal/tool-telemetry"),
];

pub const OFFICIAL_INFRASTRUCTURE: &[(&str, &str)] = &[("router", "phoxal/infrastructure-router")];

pub const OFFICIAL_SIMULATORS: &[(&str, &str)] = &[
    ("webots-controller", "phoxal/simulator-webots-controller"),
    ("webots-supervisor", "phoxal/simulator-webots-supervisor"),
];

#[must_use]
pub fn service_names(_catalog: &Catalog) -> Vec<String> {
    OFFICIAL_SERVICES
        .iter()
        .map(|(name, _)| (*name).to_string())
        .collect()
}

#[must_use]
pub fn latest_by_package(catalog: &Catalog) -> BTreeMap<&str, &Artifact> {
    let mut selected = BTreeMap::new();
    for artifact in &catalog.artifacts {
        selected
            .entry(artifact.package.as_str())
            .and_modify(|current: &mut &Artifact| {
                if compare_versions(&artifact.version, &current.version).is_gt() {
                    *current = artifact;
                }
            })
            .or_insert(artifact);
    }
    selected
}

pub fn select_artifact<'a>(
    catalog: &'a Catalog,
    package: &str,
    pin: Option<&phoxal::model::robot::v0::ArtifactPin>,
    target: &str,
) -> Result<&'a Artifact> {
    use phoxal::model::robot::v0::ArtifactPin;

    let candidates = catalog
        .artifacts
        .iter()
        .filter(|artifact| artifact.package == package)
        .collect::<Vec<_>>();
    let selected = match pin {
        Some(ArtifactPin::Version(pin)) => candidates
            .into_iter()
            .find(|artifact| normalize_version(&artifact.version) == normalize_version(&pin.0))
            .ok_or_else(|| anyhow!("{package} version pin {} is absent from the full catalog index", pin.0))?,
        Some(ArtifactPin::Sha256(pin)) => {
            let digest = pin.0.strip_prefix("sha256:").unwrap_or(&pin.0);
            let matches = candidates
                .into_iter()
                .filter(|artifact| {
                    artifact
                        .targets
                        .get(target)
                        .is_some_and(|blob| blob.sha256 == digest)
                })
                .collect::<Vec<_>>();
            ensure!(
                matches.len() == 1,
                "{package} sha256 pin {} matched {} versions for target {target}; expected exactly one",
                pin.0,
                matches.len()
            );
            matches[0]
        }
        Some(ArtifactPin::Path(_) | ArtifactPin::Git(_)) => {
            bail!("path/git pins bypass catalog selection for {package}")
        }
        None => candidates
            .into_iter()
            .max_by(|left, right| compare_versions(&left.version, &right.version))
            .ok_or_else(|| anyhow!(
                // Finding A6: aligned with every other "the active catalog
                // doesn't have what this CLI needs" remediation in the crate
                // (`resolver`/`native_artifacts`/`launch_plan`/`deploy` all
                // say `phoxal update`) instead of the old, inconsistent
                // "upgrade the CLI binary or switch channel" wording - `phoxal
                // update` is the direct action (re-resolve against the
                // current channel head and re-vendor whatever it requires).
                "expected package {package} is absent from snapshot {} (channel head); phoxal-cli {} expects it. Run `phoxal update`",
                catalog.build.tag,
                env!("CARGO_PKG_VERSION")
            ))?,
    };
    Ok(selected)
}

fn normalize_version(value: &str) -> &str {
    value.strip_prefix('v').unwrap_or(value)
}

fn compare_versions(left: &str, right: &str) -> std::cmp::Ordering {
    match (
        semver::Version::parse(normalize_version(left)),
        semver::Version::parse(normalize_version(right)),
    ) {
        (Ok(left), Ok(right)) => left.cmp(&right),
        _ => left.cmp(right),
    }
}

// Integration-test fixture helpers. These create only the current v0 wire
// shape; the kind marker is fixture-only platform-model input.
#[derive(Clone, Debug, PartialEq)]
pub struct CatalogFixtureEntry {
    pub kind: ArtifactKind,
    pub artifact: Artifact,
}

impl CatalogFixtureEntry {
    pub fn as_artifact_entry_mut(&mut self) -> &mut Artifact {
        &mut self.artifact
    }

    pub fn as_asset_entry_mut(&mut self) -> &mut Artifact {
        &mut self.artifact
    }
}

pub fn fixture_catalog_for_tests(entries: Vec<CatalogFixtureEntry>) -> Catalog {
    let mut artifacts = Vec::<Artifact>::new();
    for entry in entries {
        if let Some(existing) = artifacts.iter_mut().find(|artifact| {
            artifact.package == entry.artifact.package && artifact.version == entry.artifact.version
        }) {
            existing.targets.extend(entry.artifact.targets);
            if entry.artifact.assets.is_some() {
                existing.assets = entry.artifact.assets;
            }
        } else {
            artifacts.push(entry.artifact);
        }
    }
    let existing = artifacts
        .iter()
        .map(|artifact| artifact.package.clone())
        .collect::<BTreeSet<_>>();
    for (_, package) in OFFICIAL_SERVICES
        .iter()
        .chain(OFFICIAL_TOOLS)
        .chain(OFFICIAL_INFRASTRUCTURE)
        .chain(OFFICIAL_SIMULATORS)
    {
        if existing.contains(*package) {
            continue;
        }
        let mut targets = BTreeMap::new();
        for target in [
            crate::resolver::host_target_triple(),
            "aarch64-unknown-linux-gnu".to_string(),
            "aarch64-unknown-linux-musl".to_string(),
            "x86_64-unknown-linux-gnu".to_string(),
        ] {
            targets.insert(target.clone(), fixture_blob(package, &target));
        }
        artifacts.push(Artifact {
            package: (*package).to_string(),
            version: "0.1.0".to_string(),
            targets,
            assets: None,
        });
    }
    for component in ["ddsm115", "bench_camera", "catalog_motor"] {
        let package = format!("phoxal/component-{component}");
        if artifacts.iter().any(|artifact| artifact.package == package) {
            continue;
        }
        let mut targets = BTreeMap::new();
        for target in [
            crate::resolver::host_target_triple(),
            "aarch64-unknown-linux-gnu".to_string(),
        ] {
            targets.insert(target.clone(), fixture_blob(&package, &target));
        }
        artifacts.push(Artifact {
            package: package.clone(),
            version: "0.1.0".to_string(),
            targets,
            assets: Some(fixture_blob(&package, "assets")),
        });
    }
    Catalog::new(
        BuildProvenance {
            tag: "build-test".to_string(),
            run_number: 1,
            run_id: 1,
            commit: "test".to_string(),
            created_at: "2026-07-10T00:00:00Z".to_string(),
        },
        artifacts,
        Heads {
            stable: "build-test".to_string(),
            nightly: "build-test".to_string(),
        },
    )
}

fn fixture_blob(package: &str, target: &str) -> Blob {
    Blob {
        url: format!(
            "https://example.invalid/{}/{target}.tar.gz",
            package.replace('/', "-")
        ),
        sha256: "0".repeat(64),
        size: 1,
    }
}

fn fixture_entry(
    kind: ArtifactKind,
    package: String,
    version: &str,
    target: &str,
    published: bool,
) -> CatalogFixtureEntry {
    let mut targets = BTreeMap::new();
    let mut assets = None;
    if published {
        if kind == ArtifactKind::ComponentAssets {
            assets = Some(fixture_blob(&package, "assets"));
        } else {
            targets.insert(target.to_string(), fixture_blob(&package, target));
        }
    }
    CatalogFixtureEntry {
        kind,
        artifact: Artifact {
            package,
            version: version.to_string(),
            targets,
            assets,
        },
    }
}

pub fn fixture_service_entry_for_tests(
    name: &str,
    version: &str,
    _channel: SelectionChannel,
    target: &str,
    published: bool,
    _metadata: Vec<()>,
) -> CatalogFixtureEntry {
    fixture_entry(
        ArtifactKind::Service,
        format!("phoxal/service-{name}"),
        version,
        target,
        published,
    )
}

pub fn fixture_component_driver_entry_for_tests(
    name: &str,
    version: &str,
    _channel: SelectionChannel,
    target: &str,
    published: bool,
    _metadata: Vec<()>,
) -> CatalogFixtureEntry {
    fixture_entry(
        ArtifactKind::ComponentDriver,
        format!("phoxal/component-{name}"),
        version,
        target,
        published,
    )
}

pub fn fixture_component_assets_entry_for_tests(
    name: &str,
    version: &str,
    _channel: SelectionChannel,
) -> CatalogFixtureEntry {
    fixture_entry(
        ArtifactKind::ComponentAssets,
        format!("phoxal/component-{name}"),
        version,
        "assets",
        true,
    )
}

pub fn fixture_tool_entry_for_tests(
    name: &str,
    version: &str,
    _channel: SelectionChannel,
    target: &str,
    published: bool,
    _metadata: Vec<()>,
) -> CatalogFixtureEntry {
    fixture_entry(
        ArtifactKind::Tool,
        format!("phoxal/tool-{name}"),
        version,
        target,
        published,
    )
}

pub fn fixture_simulator_entry_for_tests(
    name: &str,
    version: &str,
    _channel: SelectionChannel,
    target: &str,
    published: bool,
    _metadata: Vec<()>,
) -> CatalogFixtureEntry {
    fixture_entry(
        ArtifactKind::Simulator,
        format!("phoxal/simulator-{name}"),
        version,
        target,
        published,
    )
}

/// Fixture metadata is intentionally discarded because v0 never carries it.
pub fn fixture_contract_for_tests(_family: &str, _role: &str) {}

#[must_use]
pub fn fixture_blob_for_tests(url: &str, sha256: &str, size: u64) -> Blob {
    Blob {
        url: url.to_string(),
        sha256: sha256.to_string(),
        size,
    }
}

#[must_use]
pub fn fixture_artifact_for_tests(url: &str, sha256: &str) -> Blob {
    fixture_blob_for_tests(url, sha256, 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_snapshot_url_is_validated() {
        assert_eq!(
            snapshot_catalog_url("build-20260710-42").unwrap(),
            "https://github.com/phoxal/framework/releases/download/build-20260710-42/catalog.json"
        );
        assert!(snapshot_catalog_url("../bad").is_err());
    }

    #[test]
    fn version_and_target_sha_pins_use_the_full_index() {
        let target = "aarch64-unknown-linux-musl";
        let mut old = fixture_service_entry_for_tests(
            "drive",
            "0.1.0",
            SelectionChannel::Stable,
            target,
            true,
            vec![],
        );
        old.artifact.targets.get_mut(target).unwrap().sha256 = "a".repeat(64);
        let new = fixture_service_entry_for_tests(
            "drive",
            "0.2.0",
            SelectionChannel::Stable,
            target,
            true,
            vec![],
        );
        let catalog = fixture_catalog_for_tests(vec![old, new]);
        let version = phoxal::model::robot::v0::ArtifactPin::Version(
            phoxal::model::robot::v0::VersionPin("0.1.0".into()),
        );
        assert_eq!(
            select_artifact(&catalog, "phoxal/service-drive", Some(&version), target)
                .unwrap()
                .version,
            "0.1.0"
        );
        let sha = phoxal::model::robot::v0::ArtifactPin::Sha256(
            phoxal::model::robot::v0::Sha256Pin(format!("sha256:{}", "a".repeat(64))),
        );
        assert_eq!(
            select_artifact(&catalog, "phoxal/service-drive", Some(&sha), target)
                .unwrap()
                .version,
            "0.1.0"
        );
    }
}
