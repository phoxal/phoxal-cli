//! CLI-side consumption of the framework's published `phoxal::catalog`
//! schema (`phoxal-artifacts.json`).
//!
//! The framework owns the wire schema itself (`Manifest`, `AssetEntry`,
//! `ArtifactEntry`, `Artifact`, `Channel`, `Contract`) - this module re-exports
//! those types and adds only the CLI-local concerns layered on top: fetching
//! the catalog file (a local path or an HTTPS URL), on-disk caching of the
//! loaded manifest, the `ArtifactKind` tag the CLI uses to remember which of
//! the manifest's five arrays an entry came from (the published schema does
//! not tag entries with a `kind` - "the array an entry lives in *is* its
//! kind"), API-generation comparison/ordering, and the preview-feature
//! promotion scanner. It does not redefine the manifest's own wire shape or
//! its cache LAYOUT (that is a separate slice's job).
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

pub use phoxal::catalog::is_schema_id;
pub use phoxal::catalog::{Artifact, ArtifactEntry, AssetEntry, Channel, Contract, Manifest};

/// A loaded catalog document is one immutable revision of the manifest; kept
/// as the name most CLI call sites already use for "the loaded catalog".
pub type CatalogRevision = Manifest;

/// [`Channel`] does not implement [`std::fmt::Display`] (it is defined in
/// `phoxal`, so a local impl would violate the orphan rule) - this is the
/// CLI's display projection wherever a channel needs to render as text.
#[must_use]
pub const fn channel_as_str(channel: Channel) -> &'static str {
    match channel {
        Channel::Stable => "stable",
        Channel::Preview => "preview",
    }
}

/// The catalog is hosted as the `latest.json` asset of the single mutable
/// `stable` front-door release (docs follow-ups/22 + decision D25), not a git
/// ref. The per-artifact "warehouse" releases (`--latest=false`) hold the
/// tarballs the catalog points at.
pub const DEFAULT_CATALOG_URL: &str =
    "https://github.com/phoxal/framework/releases/download/stable/latest.json";
pub const CATALOG_SOURCE_ENV: &str = "PHOXAL_ARTIFACT_CATALOG";
/// The target scope a [`ArtifactKind::ComponentAssets`] entry declares instead
/// of a per-triple binary matrix: asset bundles are target-independent files,
/// not per-architecture binaries (docs #21). Mirrors the framework's own
/// `xtask::workspace::TARGET_INDEPENDENT_SCOPE`.
pub const TARGET_INDEPENDENT_SCOPE: &str = "target-independent";

const CATALOG_CACHE_FILE: &str = "phoxal-artifacts.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogLoadOptions {
    pub refresh: bool,
    pub cli_source: Option<String>,
    pub robot_source: Option<PathBuf>,
}

/// Canonical CLI-local artifact kinds (docs #21): `Driver` becomes the
/// `component_driver`/`component_assets` split. The published
/// [`phoxal::catalog::Manifest`] does not carry this tag - it is implied by
/// which of the manifest's five arrays an entry lives in - so the CLI
/// reconstructs the same domain concept as a plain enum wherever it needs to
/// remember which array a given entry came from. The runtime's own
/// `emit-apis` vocabulary is intentionally different (a driver participant
/// reports `artifact.kind: "driver"`, not `component_driver`) - see
/// [`ArtifactKind::emit_apis_kind`] and the vocabulary-reconciliation callers
/// in `commands::check`.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ArtifactKind {
    Service,
    /// Target-independent component files: `component.yaml`,
    /// `simulation.yaml`, `structure.urdf`, meshes, and other asset data.
    ComponentAssets,
    /// The optional target-specific checked driver binary for a component.
    ComponentDriver,
    Tool,
    Simulator,
}

impl ArtifactKind {
    /// The `artifact.kind` string a real participant binary reports from its
    /// own `emit-apis` output - the runtime's own vocabulary
    /// (`phoxal-macros`' authoring kind, `phoxal::check::ParticipantKind`).
    /// Intentionally distinct from the catalog's own kind tag: the runtime
    /// authoring surface still calls a component driver `"driver"` and knows
    /// nothing about the CLI's `component_driver`/`component_assets` split.
    /// `ComponentAssets` has no runtime binary at all, so this value is never
    /// asserted against a real subprocess for that kind.
    #[must_use]
    pub const fn emit_apis_kind(self) -> &'static str {
        match self {
            Self::Service => "service",
            Self::ComponentAssets => "component_assets",
            Self::ComponentDriver => "driver",
            Self::Tool => "tool",
            Self::Simulator => "simulator",
        }
    }

    /// The CLI's own kind label - the vocabulary this module's diagnostics
    /// and JSON output expose, distinct from [`Self::emit_apis_kind`].
    #[must_use]
    pub const fn catalog_kind(self) -> &'static str {
        match self {
            Self::Service => "service",
            Self::ComponentAssets => "component_assets",
            Self::ComponentDriver => "component_driver",
            Self::Tool => "tool",
            Self::Simulator => "simulator",
        }
    }

    /// The package-id prefix segment (the token before `-` in
    /// `phoxal/<prefix>-<name>`) this kind uses when deriving `artifact_name`
    /// from `package`.
    const fn package_prefix(self) -> &'static str {
        match self {
            Self::Service => "service",
            Self::ComponentAssets | Self::ComponentDriver => "component",
            Self::Tool => "tool",
            Self::Simulator => "simulator",
        }
    }
}

impl std::fmt::Display for ArtifactKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.catalog_kind())
    }
}

/// The bare artifact name derived from `package` for a given `kind`, stripping
/// the provider (`phoxal/`), the kind prefix (`service-`, `component-`,
/// `tool-`, `simulator-`), and - for the two component kinds - the trailing
/// `-assets`/`-driver` suffix. `phoxal/component-ddsm115-assets` and
/// `phoxal/component-ddsm115-driver` both derive `ddsm115`;
/// `phoxal/simulator-webots-supervisor` derives `webots-supervisor` (only the
/// leading `simulator-` is a prefix, the rest is the name).
#[must_use]
pub fn artifact_name(kind: ArtifactKind, package: &str) -> Option<&str> {
    let name = package.split_once('/').map(|(_, name)| name)?;
    let prefix = kind.package_prefix();
    let rest = name.strip_prefix(prefix)?.strip_prefix('-')?;
    match kind {
        ArtifactKind::ComponentAssets => rest.strip_suffix("-assets"),
        ArtifactKind::ComponentDriver => rest.strip_suffix("-driver"),
        ArtifactKind::Service | ArtifactKind::Tool | ArtifactKind::Simulator => Some(rest),
    }
}

/// The provider segment of `package` (`phoxal` in `phoxal/service-drive`).
#[must_use]
pub fn provider(package: &str) -> Option<&str> {
    package.split_once('/').map(|(provider, _)| provider)
}

/// Map the robot manifest's own channel enum to the catalog's, without an
/// orphan-rule-violating `From` impl (both types are foreign to this crate
/// once the catalog schema moved into `phoxal::catalog`).
#[must_use]
pub fn to_catalog_channel(channel: phoxal::model::robot::v1::Channel) -> Channel {
    match channel {
        phoxal::model::robot::v1::Channel::Stable => Channel::Stable,
        phoxal::model::robot::v1::Channel::Preview => Channel::Preview,
    }
}

pub fn load_catalog(options: CatalogLoadOptions) -> Result<Option<Manifest>> {
    if let Some(source) = explicit_source(options.cli_source, options.robot_source) {
        let manifest = read_source(&source, options.refresh)?;
        return Ok(Some(manifest));
    }

    let cache_path = cache_path()?;
    if options.refresh {
        let manifest = fetch_default_catalog(&cache_path, true)?;
        write_cache(&cache_path, &manifest)?;
        return Ok(Some(manifest));
    }

    if cache_path.is_file() {
        return read_catalog_path(&cache_path).map(Some);
    }

    let manifest = fetch_default_catalog(&cache_path, false)?;
    write_cache(&cache_path, &manifest)?;
    Ok(Some(manifest))
}

fn explicit_source(cli_source: Option<String>, robot_source: Option<PathBuf>) -> Option<String> {
    cli_source
        .or_else(|| std::env::var(CATALOG_SOURCE_ENV).ok())
        .or_else(|| robot_source.map(|path| path.display().to_string()))
}

fn read_source(source: &str, refresh: bool) -> Result<Manifest> {
    if source.starts_with("https://") {
        let cache_path = cache_path()?;
        if refresh || !cache_path.is_file() {
            let manifest = fetch_https(source)
                .with_context(|| format!("failed to fetch artifact catalog from {source}"))?;
            write_cache(&cache_path, &manifest)?;
            Ok(manifest)
        } else {
            read_catalog_path(&cache_path)
        }
    } else if source.starts_with("http://") {
        bail!("artifact catalog source must use HTTPS or a local path: {source}");
    } else {
        read_catalog_path(Path::new(source))
    }
}

fn fetch_default_catalog(cache_path: &Path, refresh: bool) -> Result<Manifest> {
    let mut tried = Vec::new();
    if refresh {
        tried.push(format!("default catalog URL {DEFAULT_CATALOG_URL}"));
    } else {
        tried.push(format!("cached catalog {}", cache_path.display()));
        tried.push(format!("default catalog URL {DEFAULT_CATALOG_URL}"));
    }
    fetch_https(DEFAULT_CATALOG_URL)
        .map_err(|error| unavailable_catalog_error_with_attempts(&tried, error))
}

pub fn read_catalog_path(path: &Path) -> Result<Manifest> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let manifest: Manifest = serde_json::from_str(&text)
        .with_context(|| format!("{} is not a valid catalog JSON document", path.display()))?;
    manifest
        .verify()
        .with_context(|| format!("catalog {} failed verification", path.display()))?;
    Ok(manifest)
}

fn fetch_https(url: &str) -> Result<Manifest> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("phoxal-cli")
        .timeout(Duration::from_secs(20))
        .build()?;
    let response = client.get(url).send()?;
    let status = response.status();
    if !status.is_success() {
        bail!("artifact catalog request returned {status}");
    }
    let text = response.text()?;
    let manifest: Manifest =
        serde_json::from_str(&text).context("fetched catalog was not valid JSON")?;
    manifest.verify()?;
    Ok(manifest)
}

fn cache_path() -> Result<PathBuf> {
    Ok(crate::host_paths::cache_dir()?
        .join("catalog")
        .join(CATALOG_CACHE_FILE))
}

fn write_cache(path: &Path, manifest: &Manifest) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create catalog cache dir {}", parent.display()))?;
    }
    let mut json =
        serde_json::to_string_pretty(manifest).context("failed to serialize catalog cache")?;
    json.push('\n');
    let partial = path.with_extension("partial");
    fs::write(&partial, json)
        .with_context(|| format!("failed to write catalog cache {}", partial.display()))?;
    fs::rename(&partial, path)
        .with_context(|| format!("failed to finalize catalog cache {}", path.display()))
}

/// Every official service name in the manifest, deduplicated and sorted.
#[must_use]
pub fn service_names(manifest: &Manifest) -> Vec<String> {
    manifest
        .services
        .iter()
        .filter_map(|entry| artifact_name(ArtifactKind::Service, &entry.package))
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Every distinct API generation across the manifest's runtime-binary
/// entries (services/drivers/tools/simulators - `assets` carry no
/// `api_generation` at all, since an asset bundle has no wire contracts to
/// version), sorted oldest to newest.
#[must_use]
pub fn generations(manifest: &Manifest) -> Vec<String> {
    let mut generations = manifest
        .artifact_entries()
        .map(|entry| entry.api_generation.clone())
        .collect::<Vec<_>>();
    generations.sort_by(|left, right| compare_generations(left, right));
    generations.dedup();
    generations
}

/// The newest generation with at least one entry on `channel`.
///
/// Deliberately does NOT require a built [`Artifact`] for `target`: which
/// generation to auto-select is a channel/generation question (mirrors
/// `resolver::select_latest_artifact_entries`), while whether a concrete
/// tarball exists for `target` is a later, per-package staging concern. A
/// metadata-only catalog (`xtask catalog generate --metadata-only`, no
/// triples built anywhere yet) must still resolve a target generation so
/// `check` can validate contracts/config against it even before the first
/// real release populates `artifacts`. `target` is kept for call-site
/// symmetry with the rest of the resolution API and in case a future catalog
/// re-introduces a per-target generation split; a robot that truly needs a
/// specific triple's build to exist finds out at staging time
/// (`ensure_catalog_availability`, `native_artifacts::stage_runtime`), which
/// is where the lean schema actually carries that information.
#[must_use]
pub fn newest_generation_on_channel(
    manifest: &Manifest,
    channel: Channel,
    _target: &str,
) -> Option<String> {
    let mut generations = manifest
        .artifact_entries()
        .filter(|entry| entry.channels.contains_key(&channel))
        .map(|entry| entry.api_generation.clone())
        .collect::<Vec<_>>();
    generations.sort_by(|left, right| compare_generations(left, right));
    generations.dedup();
    generations.pop()
}

/// The single channel every entry for `generation` agrees on, or `None` if
/// there is no such generation or its entries disagree.
#[must_use]
pub fn generation_channel(manifest: &Manifest, generation: &str) -> Option<Channel> {
    let mut channels = manifest
        .artifact_entries()
        .filter(|entry| entry.api_generation == generation)
        .flat_map(|entry| entry.channels.keys().copied())
        .collect::<Vec<_>>();
    channels.sort();
    channels.dedup();
    match channels.as_slice() {
        [channel] => Some(*channel),
        _ => None,
    }
}

pub fn compare_generations(left: &str, right: &str) -> std::cmp::Ordering {
    generation_key(left).cmp(&generation_key(right))
}

fn generation_key(value: &str) -> (u32, u32, &str) {
    let Some(rest) = value.strip_prefix('y') else {
        return (0, 0, value);
    };
    let Some((year, generation)) = rest.split_once('_') else {
        return (0, 0, value);
    };
    (
        year.parse::<u32>().unwrap_or(0),
        generation.parse::<u32>().unwrap_or(0),
        value,
    )
}

pub fn parse_generation_feature(feature: &str) -> Option<&str> {
    feature.strip_prefix("preview-")
}

pub fn promoted_preview_features(
    manifest_path: &Path,
    catalog: &Manifest,
) -> Result<Vec<PromotedPreviewFeature>> {
    let text = fs::read_to_string(manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let mut promoted = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        for feature in preview_features_in_line(line) {
            let Some(generation) = parse_generation_feature(feature) else {
                continue;
            };
            if generation_channel(catalog, generation) == Some(Channel::Stable) {
                promoted.push(PromotedPreviewFeature {
                    generation: generation.to_string(),
                    manifest_path: manifest_path.to_path_buf(),
                    line_number: index + 1,
                    line: line.trim().to_string(),
                });
            }
        }
    }
    Ok(promoted)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromotedPreviewFeature {
    pub generation: String,
    pub manifest_path: PathBuf,
    pub line_number: usize,
    pub line: String,
}

fn preview_features_in_line(line: &str) -> Vec<&str> {
    let mut features = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find("preview-") {
        rest = &rest[start..];
        let end = rest
            .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'))
            .unwrap_or(rest.len());
        features.push(&rest[..end]);
        rest = &rest[end..];
    }
    features
}

pub fn unavailable_catalog_error() -> anyhow::Error {
    anyhow!(
        "no artifact catalog revision is available; tried cached catalog and default catalog URL {DEFAULT_CATALOG_URL}. Pass --catalog <path-or-https-url>, set {CATALOG_SOURCE_ENV}, add artifacts.catalog in robot.yaml, or run `phoxal-cli pull` with network access to refresh the cache."
    )
}

fn unavailable_catalog_error_with_attempts(
    tried: &[String],
    source: anyhow::Error,
) -> anyhow::Error {
    anyhow!(
        "no artifact catalog revision is available; tried {}. Default fetch failed: {source:#}. Pass --catalog <path-or-https-url>, set {CATALOG_SOURCE_ENV}, add artifacts.catalog in robot.yaml, or run `phoxal-cli pull` with network access to refresh the cache.",
        tried.join(", ")
    )
}

// ---------------------------------------------------------------------------
// Test fixtures.
//
// These are plain `pub fn`s (not `#[cfg(test)]`) because the CLI's own
// integration tests under `tests/` build the fixtures as an external crate.
// `CatalogFixtureEntry` exists only so callers can keep writing a single
// mixed-kind `vec![fixture_service_entry_for_tests(...), ...]` literal even
// though the published `Manifest` itself requires five separately-typed
// arrays.
// ---------------------------------------------------------------------------

/// One fixture entry, tagged with which of [`Manifest`]'s five arrays it
/// belongs in. [`fixture_catalog_for_tests`] partitions a mixed `Vec` of
/// these into the manifest's real shape.
#[derive(Clone, Debug, PartialEq)]
pub enum CatalogFixtureEntry {
    Asset(AssetEntry),
    Service(ArtifactEntry),
    Driver(ArtifactEntry),
    Tool(ArtifactEntry),
    Simulator(ArtifactEntry),
}

impl CatalogFixtureEntry {
    /// Borrow the inner [`ArtifactEntry`] mutably, for fixture call sites
    /// that populate `artifacts`/`changed_contracts` after construction.
    /// Panics for [`Self::Asset`], which wraps an [`AssetEntry`] instead -
    /// use [`Self::as_asset_entry_mut`] there.
    pub fn as_artifact_entry_mut(&mut self) -> &mut ArtifactEntry {
        match self {
            Self::Service(entry)
            | Self::Driver(entry)
            | Self::Tool(entry)
            | Self::Simulator(entry) => entry,
            Self::Asset(_) => panic!("fixture asset entry has no ArtifactEntry shape"),
        }
    }

    /// Borrow the inner [`AssetEntry`] mutably. Panics for any non-asset
    /// variant.
    pub fn as_asset_entry_mut(&mut self) -> &mut AssetEntry {
        match self {
            Self::Asset(entry) => entry,
            _ => panic!("fixture entry is not a component_assets entry"),
        }
    }
}

pub fn fixture_catalog_for_tests(entries: Vec<CatalogFixtureEntry>) -> Manifest {
    let mut assets = Vec::new();
    let mut services = Vec::new();
    let mut drivers = Vec::new();
    let mut tools = Vec::new();
    let mut simulators = Vec::new();
    for entry in entries {
        match entry {
            CatalogFixtureEntry::Asset(entry) => assets.push(entry),
            CatalogFixtureEntry::Service(entry) => services.push(entry),
            CatalogFixtureEntry::Driver(entry) => drivers.push(entry),
            CatalogFixtureEntry::Tool(entry) => tools.push(entry),
            CatalogFixtureEntry::Simulator(entry) => simulators.push(entry),
        }
    }
    Manifest::new(assets, services, drivers, tools, simulators)
        .finalize()
        .expect("fixture manifest should serialize for its checksum")
}

fn fixture_channels(
    channel: Channel,
    version: &str,
) -> std::collections::BTreeMap<Channel, String> {
    let mut channels = std::collections::BTreeMap::new();
    channels.insert(channel, version.to_string());
    channels
}

/// `published` controls whether the fixture entry carries a built
/// [`Artifact`] for `target`: `true` inserts a deterministic placeholder
/// tarball/sha256 (fixtures that need a specific asset/sha256 can overwrite
/// `entry.artifacts` afterward via [`CatalogFixtureEntry::as_artifact_entry_mut`]);
/// `false` leaves `artifacts` empty for `target`, mirroring a metadata-only /
/// not-yet-published catalog entry.
fn fixture_artifacts(
    target: &str,
    published: bool,
    tag: &str,
) -> std::collections::BTreeMap<String, Artifact> {
    let mut artifacts = std::collections::BTreeMap::new();
    if published {
        artifacts.insert(
            target.to_string(),
            Artifact {
                tarball: format!("{tag}-{target}.tar.zst"),
                sha256: "0".repeat(64),
            },
        );
    }
    artifacts
}

#[allow(clippy::too_many_arguments)]
pub fn fixture_service_entry_for_tests(
    name: &str,
    generation: &str,
    version: &str,
    channel: Channel,
    target: &str,
    published: bool,
    contracts: Vec<Contract>,
) -> CatalogFixtureEntry {
    let package = format!("phoxal/service-{name}");
    CatalogFixtureEntry::Service(ArtifactEntry {
        package: package.clone(),
        version: version.to_string(),
        api_generation: generation.to_string(),
        contracts,
        config_schema: None,
        bus_abi: "phoxal-bus/v0".to_string(),
        artifacts: fixture_artifacts(target, published, &package.replace('/', "-")),
        channels: fixture_channels(channel, version),
        changed_contracts: Vec::new(),
    })
}

#[allow(clippy::too_many_arguments)]
pub fn fixture_component_driver_entry_for_tests(
    name: &str,
    generation: &str,
    version: &str,
    channel: Channel,
    target: &str,
    published: bool,
    contracts: Vec<Contract>,
) -> CatalogFixtureEntry {
    let package = format!("phoxal/component-{name}-driver");
    CatalogFixtureEntry::Driver(ArtifactEntry {
        package: package.clone(),
        version: version.to_string(),
        api_generation: generation.to_string(),
        contracts,
        config_schema: None,
        bus_abi: "phoxal-bus/v0".to_string(),
        artifacts: fixture_artifacts(target, published, &package.replace('/', "-")),
        channels: fixture_channels(channel, version),
        changed_contracts: Vec::new(),
    })
}

pub fn fixture_component_assets_entry_for_tests(
    name: &str,
    generation: &str,
    version: &str,
    channel: Channel,
) -> CatalogFixtureEntry {
    let _ = generation; // AssetEntry carries no api_generation (docs: asset bundles have no wire contracts).
    let package = format!("phoxal/component-{name}-assets");
    CatalogFixtureEntry::Asset(AssetEntry {
        package,
        version: version.to_string(),
        artifacts: std::collections::BTreeMap::new(),
        channels: fixture_channels(channel, version),
    })
}

#[allow(clippy::too_many_arguments)]
pub fn fixture_tool_entry_for_tests(
    name: &str,
    generation: &str,
    version: &str,
    channel: Channel,
    target: &str,
    published: bool,
    contracts: Vec<Contract>,
) -> CatalogFixtureEntry {
    let package = format!("phoxal/tool-{name}");
    CatalogFixtureEntry::Tool(ArtifactEntry {
        package: package.clone(),
        version: version.to_string(),
        api_generation: generation.to_string(),
        contracts,
        config_schema: None,
        bus_abi: "phoxal-bus/v0".to_string(),
        artifacts: fixture_artifacts(target, published, &package.replace('/', "-")),
        channels: fixture_channels(channel, version),
        changed_contracts: Vec::new(),
    })
}

#[allow(clippy::too_many_arguments)]
pub fn fixture_simulator_entry_for_tests(
    name: &str,
    generation: &str,
    version: &str,
    channel: Channel,
    target: &str,
    published: bool,
    contracts: Vec<Contract>,
) -> CatalogFixtureEntry {
    let package = format!("phoxal/simulator-{name}");
    CatalogFixtureEntry::Simulator(ArtifactEntry {
        package: package.clone(),
        version: version.to_string(),
        api_generation: generation.to_string(),
        contracts,
        config_schema: None,
        bus_abi: "phoxal-bus/v0".to_string(),
        artifacts: fixture_artifacts(target, published, &package.replace('/', "-")),
        channels: fixture_channels(channel, version),
        changed_contracts: Vec::new(),
    })
}

pub fn fixture_contract_for_tests(family: &str, schema_id: &str) -> Contract {
    Contract {
        family: family.to_string(),
        schema_id: schema_id.to_string(),
    }
}

/// A placeholder built [`Artifact`] a fixture can insert directly into
/// `entry.artifacts` (via [`CatalogFixtureEntry::as_artifact_entry_mut`] or
/// [`CatalogFixtureEntry::as_asset_entry_mut`]) when it needs a specific
/// tarball name / sha256 rather than the [`fixture_artifacts`] placeholder.
#[must_use]
pub fn fixture_artifact_for_tests(tarball: &str, sha256: &str) -> Artifact {
    Artifact {
        tarball: tarball.to_string(),
        sha256: sha256.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_verification_rejects_edits() {
        let mut manifest = fixture_catalog_for_tests(vec![fixture_service_entry_for_tests(
            "drive",
            "y2026_1",
            "0.1.0",
            Channel::Stable,
            "aarch64-unknown-linux-gnu",
            false,
            vec![fixture_contract_for_tests(
                "drive::Target",
                "0123456789abcdef",
            )],
        )]);
        manifest.services[0].package = "phoxal-service-edited".to_string();

        let error = manifest.verify().expect_err("edited catalog should fail");
        assert!(format!("{error:#}").contains("did not match computed"));
    }

    #[test]
    fn preview_feature_scanner_finds_promoted_generation_lines() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let manifest_path = temp.path().join("Cargo.toml");
        fs::write(
            &manifest_path,
            r#"[dependencies]
phoxal-api = { version = "0.19", features = ["preview-y2026_2"] }
"#,
        )?;
        let mut entry = fixture_service_entry_for_tests(
            "drive",
            "y2026_2",
            "0.2.0",
            Channel::Stable,
            "aarch64-unknown-linux-gnu",
            true,
            vec![fixture_contract_for_tests(
                "drive::Target",
                "0123456789abcdef",
            )],
        );
        let catalog = fixture_catalog_for_tests(vec![entry.clone()]);
        let _ = entry.as_artifact_entry_mut();

        let promoted = promoted_preview_features(&manifest_path, &catalog)?;

        assert_eq!(promoted.len(), 1);
        assert_eq!(promoted[0].generation, "y2026_2");
        assert_eq!(promoted[0].line_number, 2);
        Ok(())
    }

    #[test]
    fn artifact_name_strips_provider_kind_prefix_and_component_suffix() {
        assert_eq!(
            artifact_name(ArtifactKind::Service, "phoxal/service-drive"),
            Some("drive")
        );
        assert_eq!(
            artifact_name(
                ArtifactKind::ComponentAssets,
                "phoxal/component-ddsm115-assets"
            ),
            Some("ddsm115")
        );
        assert_eq!(
            artifact_name(
                ArtifactKind::ComponentDriver,
                "phoxal/component-ddsm115-driver"
            ),
            Some("ddsm115")
        );
        assert_eq!(
            artifact_name(
                ArtifactKind::Simulator,
                "phoxal/simulator-webots-supervisor"
            ),
            Some("webots-supervisor")
        );
    }
}
