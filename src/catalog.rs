//! CLI-side consumption of the framework's published `phoxal::catalog`
//! schema (`phoxal-artifacts.json`).
//!
//! The framework owns the wire schema itself (`Manifest`, `AssetEntry`,
//! `ArtifactEntry`, `Artifact`, `Channel`, `Contract`) - this module re-exports
//! those types and adds only the CLI-local concerns layered on top: fetching
//! the REMOTE catalog (a local path or an HTTPS URL, always fetched fresh -
//! there is no on-disk cache of the remote fetch; see [`load_catalog`]), the
//! `ArtifactKind` tag the CLI uses to remember which of the manifest's five
//! arrays an entry came from (the published schema does not tag entries with
//! a `kind` - "the array an entry lives in *is* its kind"), and the LOCAL
//! download manifest ([`upsert_local_manifest_entry`]/[`prune_local_manifest`])
//! that records which official artifacts are actually present in
//! `cache/artifacts/` - a wholly different document from the remote catalog,
//! never a cached copy of it (docs D64: no lockfile, no cached remote
//! catalog).
//!
//! There is no API-generation comparison/ordering here anymore (D1, X-tools
//! slice): [`ArtifactEntry`] carries no `api_generation`, so there is nothing
//! left to compare/order by generation. Per-package resolution is purely
//! semver-ordered now (see `crate::resolver::select_latest_artifact_entries`).
//! The preview-feature-promotion scanner is gone with it: it existed to warn
//! when a `preview-<generation>` Cargo feature's generation had been promoted
//! to the stable channel, which needed [`Channel`] lookup by generation - a
//! question the catalog can no longer answer at all.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogLoadOptions {
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
pub fn to_catalog_channel(channel: phoxal::model::robot::v0::Channel) -> Channel {
    match channel {
        phoxal::model::robot::v0::Channel::Stable => Channel::Stable,
        phoxal::model::robot::v0::Channel::Preview => Channel::Preview,
    }
}

/// Load the remote artifact catalog, always fetched fresh into memory -
/// there is no on-disk cache of this document (docs D64: no lockfile, no
/// cached copy of the remote catalog). An explicit source (`--catalog`, the
/// `PHOXAL_ARTIFACT_CATALOG` env var, or `robot.yaml`'s `artifacts.catalog`)
/// takes precedence; a local path is read directly, an HTTPS URL is fetched
/// live. With no explicit source, this fetches [`DEFAULT_CATALOG_URL`].
///
/// A fully offline session can still validate previously-downloaded official
/// artifacts by pointing `--catalog` at the LOCAL download manifest
/// ([`crate::host_paths::local_manifest_path`]) directly - it is itself a
/// valid `Manifest`, just scoped to what is actually cached locally.
pub fn load_catalog(options: CatalogLoadOptions) -> Result<Option<Manifest>> {
    if let Some(source) = explicit_source(options.cli_source, options.robot_source) {
        return read_source(&source).map(Some);
    }
    fetch_default_catalog().map(Some)
}

fn explicit_source(cli_source: Option<String>, robot_source: Option<PathBuf>) -> Option<String> {
    cli_source
        .or_else(|| std::env::var(CATALOG_SOURCE_ENV).ok())
        .or_else(|| robot_source.map(|path| path.display().to_string()))
}

fn read_source(source: &str) -> Result<Manifest> {
    if source.starts_with("https://") {
        fetch_https(source)
            .with_context(|| format!("failed to fetch artifact catalog from {source}"))
    } else if source.starts_with("http://") {
        bail!("artifact catalog source must use HTTPS or a local path: {source}");
    } else {
        read_catalog_path(Path::new(source))
    }
}

fn fetch_default_catalog() -> Result<Manifest> {
    fetch_https(DEFAULT_CATALOG_URL).map_err(unavailable_catalog_error_with_fetch_error)
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

/// Serialize `manifest` as pretty JSON and write it to `path` atomically
/// (write to a unique sibling temp file, then rename into place) so a reader
/// never observes a half-written manifest. Used by the local download
/// manifest; the remote catalog itself is never written to disk (see the
/// [`load_catalog`] docs).
fn write_manifest_atomic(path: &Path, manifest: &Manifest) -> Result<()> {
    let parent = path
        .parent()
        .context("local manifest path did not have a parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let mut json =
        serde_json::to_string_pretty(manifest).context("failed to serialize local manifest")?;
    json.push('\n');
    let mut partial = tempfile::Builder::new()
        .prefix(".phoxal-artifacts-")
        .suffix(".partial")
        .tempfile_in(parent)
        .with_context(|| format!("failed to create temp manifest in {}", parent.display()))?;
    partial
        .write_all(json.as_bytes())
        .with_context(|| format!("failed to write {}", partial.path().display()))?;
    partial
        .persist(path)
        .map(|_| ())
        .map_err(|error| error.error)
        .with_context(|| format!("failed to finalize {}", path.display()))
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

pub fn unavailable_catalog_error() -> anyhow::Error {
    anyhow!(
        "no artifact catalog revision is available; tried the default catalog URL {DEFAULT_CATALOG_URL}. Pass --catalog <path-or-https-url>, set {CATALOG_SOURCE_ENV}, add artifacts.catalog in robot.yaml, or point --catalog at the local download manifest ({LOCAL_MANIFEST_HINT}) to validate offline against already-cached artifacts."
    )
}

fn unavailable_catalog_error_with_fetch_error(source: anyhow::Error) -> anyhow::Error {
    anyhow!(
        "no artifact catalog revision is available; fetching the default catalog URL {DEFAULT_CATALOG_URL} failed: {source:#}. Pass --catalog <path-or-https-url>, set {CATALOG_SOURCE_ENV}, add artifacts.catalog in robot.yaml, or point --catalog at the local download manifest ({LOCAL_MANIFEST_HINT}) to validate offline against already-cached artifacts."
    )
}

/// Human-readable hint for the local download manifest path, used only in
/// diagnostics (not resolved against the real `PHOXAL_HOME` - a fixed
/// `~/.phoxal/...` string reads better in an error message than a
/// possibly-relocated absolute path).
const LOCAL_MANIFEST_HINT: &str = "~/.phoxal/cache/phoxal-artifacts.json";

// ---------------------------------------------------------------------------
// Local download manifest (`cache/phoxal-artifacts.json`).
//
// A wholly local document: it records which official artifacts are actually
// present in `cache/artifacts/`, with their contracts/config inlined exactly
// like the remote catalog's own `ArtifactEntry`/`AssetEntry` shapes - so
// `check`/`run` can validate a cached artifact's contracts without any
// network access, and so the file itself is a valid `--catalog` source for a
// fully offline session. It is NOT a cache of the remote catalog: an entry
// only exists here once its tarball has actually been downloaded, and
// `cache clean` prunes entries whose tarball no longer exists.
// ---------------------------------------------------------------------------

/// One official artifact's data needed to upsert its entry into the local
/// download manifest once its tarball lands in `cache/artifacts/`.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalManifestUpsert {
    pub kind: ArtifactKind,
    pub package: String,
    pub version: String,
    pub contracts: Vec<Contract>,
    pub config_schema: Option<serde_json::Value>,
    pub changed_contracts: Vec<String>,
    pub channel: Channel,
    /// The target triple this tarball was built for, or
    /// [`TARGET_INDEPENDENT_SCOPE`] for a `component_assets` bundle.
    pub target: String,
    pub tarball: String,
    pub sha256: String,
}

/// Read the local download manifest, or an empty (but validly finalized)
/// manifest if it does not exist yet.
pub fn load_local_manifest() -> Result<Manifest> {
    let path = crate::host_paths::local_manifest_path()?;
    read_local_manifest_at(&path)
}

fn read_local_manifest_at(path: &Path) -> Result<Manifest> {
    if !path.is_file() {
        return empty_manifest();
    }
    read_catalog_path(path)
}

fn empty_manifest() -> Result<Manifest> {
    Manifest::new(Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new())
        .finalize()
        .context("failed to build an empty local download manifest")
}

/// Insert or replace `upsert`'s entry (keyed by `package`) in the local
/// download manifest and write it back atomically. Called once a tarball has
/// been freshly downloaded into `cache/artifacts/` (see
/// `native_artifacts::stage_descriptor`); idempotent to call again with the
/// same data.
pub fn upsert_local_manifest_entry(upsert: LocalManifestUpsert) -> Result<()> {
    let path = crate::host_paths::local_manifest_path()?;
    let mut manifest = read_local_manifest_at(&path)?;

    if upsert.kind == ArtifactKind::ComponentAssets {
        let existing = manifest
            .assets
            .iter()
            .find(|entry| entry.package == upsert.package && entry.version == upsert.version);
        let mut artifacts = existing
            .map(|entry| entry.artifacts.clone())
            .unwrap_or_default();
        artifacts.insert(
            upsert.target.clone(),
            Artifact {
                tarball: upsert.tarball.clone(),
                sha256: upsert.sha256.clone(),
            },
        );
        let mut channels = existing
            .map(|entry| entry.channels.clone())
            .unwrap_or_default();
        channels.insert(upsert.channel, upsert.version.clone());
        manifest
            .assets
            .retain(|entry| entry.package != upsert.package);
        manifest.assets.push(AssetEntry {
            package: upsert.package,
            version: upsert.version,
            artifacts,
            channels,
        });
    } else {
        let list = match upsert.kind {
            ArtifactKind::Service => &mut manifest.services,
            ArtifactKind::ComponentDriver => &mut manifest.drivers,
            ArtifactKind::Tool => &mut manifest.tools,
            ArtifactKind::Simulator => &mut manifest.simulators,
            ArtifactKind::ComponentAssets => unreachable!("handled above"),
        };
        let existing = list
            .iter()
            .find(|entry| entry.package == upsert.package && entry.version == upsert.version);
        let mut artifacts = existing
            .map(|entry| entry.artifacts.clone())
            .unwrap_or_default();
        artifacts.insert(
            upsert.target.clone(),
            Artifact {
                tarball: upsert.tarball.clone(),
                sha256: upsert.sha256.clone(),
            },
        );
        let mut channels = existing
            .map(|entry| entry.channels.clone())
            .unwrap_or_default();
        channels.insert(upsert.channel, upsert.version.clone());
        let entry = ArtifactEntry {
            package: upsert.package.clone(),
            version: upsert.version,
            contracts: upsert.contracts,
            config_schema: upsert.config_schema,
            artifacts,
            channels,
            changed_contracts: upsert.changed_contracts,
        };
        list.retain(|existing| existing.package != upsert.package);
        list.push(entry);
    }

    let manifest = manifest
        .finalize()
        .context("failed to finalize the local download manifest")?;
    write_manifest_atomic(&path, &manifest)
}

/// Drop every local-manifest entry whose tarball(s) are no longer present
/// under `cache/artifacts/`, and report how many entries were dropped. A
/// no-op (returns `0`) if the local manifest does not exist. Used by
/// `phoxal-cli cache clean` after clearing `cache/artifacts/`, and generally
/// keeps the manifest honest if a tarball is ever removed by hand.
pub fn prune_local_manifest() -> Result<usize> {
    let path = crate::host_paths::local_manifest_path()?;
    if !path.is_file() {
        return Ok(0);
    }
    let manifest = read_catalog_path(&path)?;
    let artifacts_dir = crate::host_paths::artifacts_dir()?;
    let tarball_present = |artifacts: &BTreeMap<String, Artifact>| {
        artifacts
            .values()
            .all(|artifact| artifacts_dir.join(&artifact.tarball).is_file())
    };

    let mut dropped = 0usize;
    let mut retain_counting = |present: bool| {
        if !present {
            dropped += 1;
        }
        present
    };
    let mut assets = manifest.assets;
    assets.retain(|entry| retain_counting(tarball_present(&entry.artifacts)));
    let mut services = manifest.services;
    services.retain(|entry| retain_counting(tarball_present(&entry.artifacts)));
    let mut drivers = manifest.drivers;
    drivers.retain(|entry| retain_counting(tarball_present(&entry.artifacts)));
    let mut tools = manifest.tools;
    tools.retain(|entry| retain_counting(tarball_present(&entry.artifacts)));
    let mut simulators = manifest.simulators;
    simulators.retain(|entry| retain_counting(tarball_present(&entry.artifacts)));

    if dropped == 0 {
        return Ok(0);
    }
    let pruned = Manifest::new(assets, services, drivers, tools, simulators)
        .finalize()
        .context("failed to finalize the pruned local download manifest")?;
    write_manifest_atomic(&path, &pruned)?;
    Ok(dropped)
}

/// The number of entries the local download manifest currently holds, with no
/// side effects - used by `phoxal-cli cache clean --dry-run` to preview how
/// many entries a real clean would prune (a full `cache clean` always empties
/// `cache/artifacts/` entirely, so every current entry would lose its
/// tarball and be dropped).
pub fn local_manifest_entry_count() -> Result<usize> {
    let path = crate::host_paths::local_manifest_path()?;
    if !path.is_file() {
        return Ok(0);
    }
    let manifest = read_catalog_path(&path)?;
    Ok(manifest.assets.len()
        + manifest.services.len()
        + manifest.drivers.len()
        + manifest.tools.len()
        + manifest.simulators.len())
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

/// No `generation` parameter (D1, X-tools slice): [`ArtifactEntry`] carries
/// no `api_generation` field to populate one from anymore.
pub fn fixture_service_entry_for_tests(
    name: &str,
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
        contracts,
        config_schema: None,
        artifacts: fixture_artifacts(target, published, &package.replace('/', "-")),
        channels: fixture_channels(channel, version),
        changed_contracts: Vec::new(),
    })
}

pub fn fixture_component_driver_entry_for_tests(
    name: &str,
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
        contracts,
        config_schema: None,
        artifacts: fixture_artifacts(target, published, &package.replace('/', "-")),
        channels: fixture_channels(channel, version),
        changed_contracts: Vec::new(),
    })
}

pub fn fixture_component_assets_entry_for_tests(
    name: &str,
    version: &str,
    channel: Channel,
) -> CatalogFixtureEntry {
    let package = format!("phoxal/component-{name}-assets");
    CatalogFixtureEntry::Asset(AssetEntry {
        package,
        version: version.to_string(),
        artifacts: std::collections::BTreeMap::new(),
        channels: fixture_channels(channel, version),
    })
}

pub fn fixture_tool_entry_for_tests(
    name: &str,
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
        contracts,
        config_schema: None,
        artifacts: fixture_artifacts(target, published, &package.replace('/', "-")),
        channels: fixture_channels(channel, version),
        changed_contracts: Vec::new(),
    })
}

pub fn fixture_simulator_entry_for_tests(
    name: &str,
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
        contracts,
        config_schema: None,
        artifacts: fixture_artifacts(target, published, &package.replace('/', "-")),
        channels: fixture_channels(channel, version),
        changed_contracts: Vec::new(),
    })
}

/// `role` is `"publish"`, `"subscribe"`, `"serve"`, or `"ask"` (D1: the
/// catalog's `Contract` is now `{family, role}`, not `{family, schema_id}` -
/// name identity alone decides compatibility, so there is no schema hash
/// left to fixture).
pub fn fixture_contract_for_tests(family: &str, role: &str) -> Contract {
    Contract {
        family: family.to_string(),
        role: role.to_string(),
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
    use crate::host_paths::test_support::ScratchPhoxalHome;

    #[test]
    fn checksum_verification_rejects_edits() {
        let mut manifest = fixture_catalog_for_tests(vec![fixture_service_entry_for_tests(
            "drive",
            "0.1.0",
            Channel::Stable,
            "aarch64-unknown-linux-gnu",
            false,
            vec![fixture_contract_for_tests(
                "y2026_1::drive::Target",
                "publish",
            )],
        )]);
        manifest.services[0].package = "phoxal-service-edited".to_string();

        let error = manifest.verify().expect_err("edited catalog should fail");
        assert!(format!("{error:#}").contains("did not match computed"));
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

    #[test]
    fn local_manifest_upsert_merges_targets_for_same_package_version() -> Result<()> {
        let _guard = ScratchPhoxalHome::new()?;
        let first = LocalManifestUpsert {
            kind: ArtifactKind::Service,
            package: "phoxal/service-drive".to_string(),
            version: "0.1.0".to_string(),
            contracts: Vec::new(),
            config_schema: None,
            changed_contracts: Vec::new(),
            channel: Channel::Stable,
            target: "aarch64-unknown-linux-gnu".to_string(),
            tarball: "phoxal-service-drive-v0.1.0-aarch64-unknown-linux-gnu.tar.zst".to_string(),
            sha256: "0".repeat(64),
        };
        let mut second = first.clone();
        second.target = "x86_64-unknown-linux-gnu".to_string();
        second.tarball = "phoxal-service-drive-v0.1.0-x86_64-unknown-linux-gnu.tar.zst".to_string();
        second.sha256 = "1".repeat(64);

        upsert_local_manifest_entry(first)?;
        upsert_local_manifest_entry(second)?;

        let manifest = load_local_manifest()?;
        assert_eq!(manifest.services.len(), 1);
        let entry = &manifest.services[0];
        assert_eq!(entry.package, "phoxal/service-drive");
        assert_eq!(entry.artifacts.len(), 2);
        assert!(entry.artifacts.contains_key("aarch64-unknown-linux-gnu"));
        assert!(entry.artifacts.contains_key("x86_64-unknown-linux-gnu"));
        Ok(())
    }
}
