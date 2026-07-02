use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const CATALOG_SCHEMA: &str = "phoxal.artifact-catalog/v0";
pub const CATALOG_FORMAT_VERSION: u32 = 1;
pub const CHECKSUM_CANONICALIZATION: &str = "json-v1-empty-revision-and-integrity-sha256";
pub const DEFAULT_CATALOG_URL: &str =
    "https://catalog.phoxal.com/artifact-catalog/v0/stable/latest.json";
pub const CATALOG_SOURCE_ENV: &str = "PHOXAL_ARTIFACT_CATALOG";

const CATALOG_CACHE_FILE: &str = "phoxal-artifact-catalog.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogLoadOptions {
    pub refresh: bool,
    pub cli_source: Option<String>,
    pub robot_source: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogRevision {
    pub schema: String,
    pub format_version: u32,
    pub revision: String,
    pub integrity: CatalogIntegrity,
    pub provenance: CatalogProvenance,
    pub signature: Option<CatalogSignature>,
    pub entries: Vec<CatalogEntry>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogIntegrity {
    pub algorithm: String,
    pub canonicalization: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogProvenance {
    pub generator: String,
    pub official_set: String,
    pub emit_apis: String,
    pub release_assets: String,
    pub plan_01: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogSignature {
    pub scheme: String,
    pub key_id: String,
    pub signature: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogEntry {
    pub artifact_id: String,
    pub kind: ArtifactKind,
    pub package: String,
    pub version: String,
    pub api_generation: String,
    pub contract_uses: Vec<ContractUse>,
    pub target_triples: Vec<String>,
    pub release_assets: BTreeMap<String, ReleaseAsset>,
    pub launch_facts: LaunchFacts,
    pub engine_versions: EngineVersions,
    pub channels: BTreeMap<Channel, String>,
    pub status: BTreeMap<String, ArtifactStatus>,
    pub changed_contracts: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Service,
    Driver,
    Tool,
    Simulator,
}

impl ArtifactKind {
    #[must_use]
    pub const fn emit_apis_kind(self) -> &'static str {
        match self {
            Self::Service => "service",
            Self::Driver => "driver",
            Self::Tool => "tool",
            Self::Simulator => "simulator",
        }
    }
}

impl std::fmt::Display for ArtifactKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.emit_apis_kind())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Stable,
    Preview,
}

impl Channel {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Preview => "preview",
        }
    }
}

impl From<phoxal::model::robot::v1::Channel> for Channel {
    fn from(channel: phoxal::model::robot::v1::Channel) -> Self {
        match channel {
            phoxal::model::robot::v1::Channel::Stable => Self::Stable,
            phoxal::model::robot::v1::Channel::Preview => Self::Preview,
        }
    }
}

impl std::fmt::Display for Channel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactStatus {
    Released,
    Pending,
    Failed,
    Blocked,
}

impl ArtifactStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Released => "released",
            Self::Pending => "pending",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
        }
    }
}

impl std::fmt::Display for ArtifactStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContractUse {
    pub family: String,
    pub topic_template: String,
    pub direction: String,
    pub schema_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseAsset {
    pub asset: String,
    pub sha256: String,
    pub metadata: ReleaseAssetMetadata,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseAssetMetadata {
    pub emit_apis: String,
    pub emit_apis_sha256: String,
    pub sha256_file: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchFacts {
    pub participant_kind: String,
    pub participant_class: String,
    pub router: RouterFacts,
    pub systemd: SystemdFacts,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RouterFacts {
    pub needs_zenoh_router: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SystemdFacts {
    pub groups: Vec<String>,
    pub devices: Vec<String>,
    pub source: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EngineVersions {
    pub phoxal: Option<String>,
    pub phoxal_bus: Option<String>,
    pub zenoh: Option<String>,
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

pub fn load_catalog(options: CatalogLoadOptions) -> Result<Option<CatalogRevision>> {
    if let Some(source) = explicit_source(options.cli_source, options.robot_source) {
        let revision = read_source(&source, options.refresh)?;
        return Ok(Some(revision));
    }

    let cache_path = cache_path()?;
    if options.refresh {
        let revision = fetch_https(DEFAULT_CATALOG_URL).map_err(|_| {
            crate::native_pending::error(
                "published artifact catalog revisions at the future default URL (06)",
            )
        })?;
        write_cache(&cache_path, &revision)?;
        return Ok(Some(revision));
    }

    if cache_path.is_file() {
        return read_catalog_path(&cache_path).map(Some);
    }

    Ok(None)
}

fn explicit_source(cli_source: Option<String>, robot_source: Option<PathBuf>) -> Option<String> {
    cli_source
        .or_else(|| std::env::var(CATALOG_SOURCE_ENV).ok())
        .or_else(|| robot_source.map(|path| path.display().to_string()))
}

fn read_source(source: &str, refresh: bool) -> Result<CatalogRevision> {
    if source.starts_with("https://") {
        let cache_path = cache_path()?;
        if refresh || !cache_path.is_file() {
            let revision = fetch_https(source)
                .with_context(|| format!("failed to fetch artifact catalog from {source}"))?;
            write_cache(&cache_path, &revision)?;
            Ok(revision)
        } else {
            read_catalog_path(&cache_path)
        }
    } else if source.starts_with("http://") {
        bail!("artifact catalog source must use HTTPS or a local path: {source}");
    } else {
        read_catalog_path(Path::new(source))
    }
}

pub fn read_catalog_path(path: &Path) -> Result<CatalogRevision> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let revision: CatalogRevision = serde_json::from_str(&text)
        .with_context(|| format!("{} is not a valid catalog JSON document", path.display()))?;
    revision
        .verify()
        .with_context(|| format!("catalog {} failed verification", path.display()))?;
    Ok(revision)
}

fn fetch_https(url: &str) -> Result<CatalogRevision> {
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
    let revision: CatalogRevision =
        serde_json::from_str(&text).context("fetched catalog was not valid JSON")?;
    revision.verify()?;
    Ok(revision)
}

fn cache_path() -> Result<PathBuf> {
    Ok(crate::host_paths::cache_dir()?
        .join("catalog")
        .join(CATALOG_CACHE_FILE))
}

fn write_cache(path: &Path, revision: &CatalogRevision) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create catalog cache dir {}", parent.display()))?;
    }
    let mut json =
        serde_json::to_string_pretty(revision).context("failed to serialize catalog cache")?;
    json.push('\n');
    let partial = path.with_extension("partial");
    fs::write(&partial, json)
        .with_context(|| format!("failed to write catalog cache {}", partial.display()))?;
    fs::rename(&partial, path)
        .with_context(|| format!("failed to finalize catalog cache {}", path.display()))
}

impl CatalogRevision {
    #[must_use]
    pub fn service_names(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|entry| entry.kind == ArtifactKind::Service)
            .filter_map(|entry| entry.artifact_name().map(str::to_string))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    #[must_use]
    pub fn generations(&self) -> Vec<String> {
        let mut generations = self
            .entries
            .iter()
            .map(|entry| entry.api_generation.clone())
            .collect::<Vec<_>>();
        generations.sort_by(|left, right| compare_generations(left, right));
        generations.dedup();
        generations
    }

    #[must_use]
    pub fn newest_generation_on_channel(&self, channel: Channel, target: &str) -> Option<String> {
        let mut generations = self
            .entries
            .iter()
            .filter(|entry| entry.channels.contains_key(&channel))
            .filter(|entry| entry.target_triples.iter().any(|triple| triple == target))
            .map(|entry| entry.api_generation.clone())
            .collect::<Vec<_>>();
        generations.sort_by(|left, right| compare_generations(left, right));
        generations.dedup();
        generations.pop()
    }

    #[must_use]
    pub fn generation_channel(&self, generation: &str) -> Option<Channel> {
        let mut channels = self
            .entries
            .iter()
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

    pub fn verify(&self) -> Result<()> {
        if self.schema != CATALOG_SCHEMA {
            bail!(
                "catalog schema '{}' did not match expected '{}'",
                self.schema,
                CATALOG_SCHEMA
            );
        }
        if self.format_version != CATALOG_FORMAT_VERSION {
            bail!(
                "catalog format_version {} did not match expected {}",
                self.format_version,
                CATALOG_FORMAT_VERSION
            );
        }
        if self.integrity.algorithm != "sha256" {
            bail!(
                "catalog integrity algorithm '{}' did not match expected 'sha256'",
                self.integrity.algorithm
            );
        }
        if self.integrity.canonicalization != CHECKSUM_CANONICALIZATION {
            bail!(
                "catalog checksum canonicalization '{}' did not match expected '{}'",
                self.integrity.canonicalization,
                CHECKSUM_CANONICALIZATION
            );
        }
        let expected = self.semantic_sha256()?;
        if self.integrity.sha256 != expected {
            bail!(
                "catalog checksum mismatch: recorded {}, computed {}",
                self.integrity.sha256,
                expected
            );
        }
        let expected_revision = format!("sha256:{expected}");
        if self.revision != expected_revision {
            bail!(
                "catalog revision '{}' did not match integrity '{}'",
                self.revision,
                expected_revision
            );
        }
        validate_entries(&self.entries)?;
        Ok(())
    }

    fn semantic_sha256(&self) -> Result<String> {
        let mut unsigned = self.clone();
        unsigned.revision.clear();
        unsigned.integrity.sha256.clear();
        let bytes =
            serde_json::to_vec(&unsigned).context("failed to serialize catalog for checksum")?;
        Ok(hex::encode(Sha256::digest(bytes)))
    }

    #[must_use]
    pub fn finalized_for_tests(mut self) -> Self {
        self.revision.clear();
        self.integrity.sha256.clear();
        let checksum = self
            .semantic_sha256()
            .expect("fixture catalog should serialize");
        self.revision = format!("sha256:{checksum}");
        self.integrity.sha256 = checksum;
        self
    }
}

impl CatalogEntry {
    #[must_use]
    pub fn artifact_name(&self) -> Option<&str> {
        match self.kind {
            ArtifactKind::Service => self.artifact_id.strip_prefix("service-"),
            ArtifactKind::Driver => self.artifact_id.strip_prefix("driver-"),
            ArtifactKind::Tool => self.artifact_id.strip_prefix("tool-"),
            ArtifactKind::Simulator => self.artifact_id.strip_prefix("simulator-"),
        }
    }

    #[must_use]
    pub fn status_for(&self, target: &str) -> Option<ArtifactStatus> {
        self.status.get(target).copied()
    }
}

fn validate_entries(entries: &[CatalogEntry]) -> Result<()> {
    if entries.is_empty() {
        bail!("catalog entries must not be empty");
    }

    let mut seen = BTreeSet::new();
    for entry in entries {
        if !seen.insert((&entry.artifact_id, &entry.version)) {
            bail!(
                "catalog contains duplicate entry for {} v{}",
                entry.artifact_id,
                entry.version
            );
        }
        if entry.artifact_name().is_none() {
            bail!(
                "{} does not match its catalog kind {}",
                entry.artifact_id,
                entry.kind
            );
        }
        if entry.contract_uses.is_empty() {
            bail!("{} contract_uses must not be empty", entry.artifact_id);
        }
        for contract in &entry.contract_uses {
            if contract.family.trim().is_empty() {
                bail!("{} has an empty contract family", entry.artifact_id);
            }
            if contract.topic_template.trim().is_empty() {
                bail!("{} has an empty topic_template", entry.artifact_id);
            }
            if !is_schema_id(&contract.schema_id) {
                bail!(
                    "{} has invalid schema_id '{}'",
                    entry.artifact_id,
                    contract.schema_id
                );
            }
        }
        if entry.target_triples.is_empty() {
            bail!("{} target_triples must not be empty", entry.artifact_id);
        }
        for triple in &entry.target_triples {
            let status = entry.status.get(triple).with_context(|| {
                format!(
                    "{} is missing status for target {triple}",
                    entry.artifact_id
                )
            })?;
            if *status == ArtifactStatus::Released && !entry.release_assets.contains_key(triple) {
                bail!(
                    "{} target {triple} is released but has no release asset",
                    entry.artifact_id
                );
            }
        }
        for (triple, asset) in &entry.release_assets {
            if !entry.target_triples.contains(triple) {
                bail!(
                    "{} release asset target {triple} is not in target_triples",
                    entry.artifact_id
                );
            }
            if !is_sha256(&asset.sha256) {
                bail!(
                    "{} release asset for {triple} has invalid sha256",
                    entry.artifact_id
                );
            }
            if !is_sha256(&asset.metadata.emit_apis_sha256) {
                bail!(
                    "{} emit-apis metadata for {triple} has invalid sha256",
                    entry.artifact_id
                );
            }
        }
        if entry.channels.is_empty() {
            bail!("{} channels must not be empty", entry.artifact_id);
        }
    }

    Ok(())
}

pub fn is_schema_id(value: &str) -> bool {
    value.len() == 16
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
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
    catalog: &CatalogRevision,
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
            if catalog.generation_channel(generation) == Some(Channel::Stable) {
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

pub fn fixture_catalog_for_tests(entries: Vec<CatalogEntry>) -> CatalogRevision {
    CatalogRevision {
        schema: CATALOG_SCHEMA.to_string(),
        format_version: CATALOG_FORMAT_VERSION,
        revision: String::new(),
        integrity: CatalogIntegrity {
            algorithm: "sha256".to_string(),
            canonicalization: CHECKSUM_CANONICALIZATION.to_string(),
            sha256: String::new(),
        },
        provenance: CatalogProvenance {
            generator: "test fixture".to_string(),
            official_set: "test fixture".to_string(),
            emit_apis: "test fixture".to_string(),
            release_assets: "test fixture".to_string(),
            plan_01: "test fixture".to_string(),
        },
        signature: None,
        entries,
    }
    .finalized_for_tests()
}

pub fn fixture_service_entry_for_tests(
    name: &str,
    generation: &str,
    version: &str,
    channel: Channel,
    target: &str,
    status: ArtifactStatus,
    contracts: Vec<ContractUse>,
) -> CatalogEntry {
    let mut channels = BTreeMap::new();
    channels.insert(channel, version.to_string());
    let mut statuses = BTreeMap::new();
    statuses.insert(target.to_string(), status);
    CatalogEntry {
        artifact_id: format!("service-{name}"),
        kind: ArtifactKind::Service,
        package: format!("phoxal-service-{name}"),
        version: version.to_string(),
        api_generation: generation.to_string(),
        contract_uses: contracts,
        target_triples: vec![target.to_string()],
        release_assets: BTreeMap::new(),
        launch_facts: LaunchFacts {
            participant_kind: "service".to_string(),
            participant_class: "checked".to_string(),
            router: RouterFacts {
                needs_zenoh_router: true,
            },
            systemd: SystemdFacts {
                groups: Vec::new(),
                devices: Vec::new(),
                source: "test fixture".to_string(),
            },
        },
        engine_versions: EngineVersions {
            phoxal: Some("0.22.0".to_string()),
            phoxal_bus: None,
            zenoh: None,
        },
        channels,
        status: statuses,
        changed_contracts: Vec::new(),
    }
}

pub fn fixture_driver_entry_for_tests(
    name: &str,
    generation: &str,
    version: &str,
    channel: Channel,
    target: &str,
    status: ArtifactStatus,
    contracts: Vec<ContractUse>,
) -> CatalogEntry {
    let mut entry = fixture_service_entry_for_tests(
        name, generation, version, channel, target, status, contracts,
    );
    entry.artifact_id = format!("driver-{name}");
    entry.kind = ArtifactKind::Driver;
    entry.package = format!("phoxal-driver-{name}");
    entry.launch_facts.participant_kind = "driver".to_string();
    entry
}

pub fn fixture_contract_for_tests(
    family: &str,
    topic_template: &str,
    direction: &str,
    schema_id: &str,
) -> ContractUse {
    ContractUse {
        family: family.to_string(),
        topic_template: topic_template.to_string(),
        direction: direction.to_string(),
        schema_id: schema_id.to_string(),
    }
}

pub fn unavailable_catalog_error() -> anyhow::Error {
    anyhow!(
        "no artifact catalog revision is available; pass --catalog <path-or-https-url>, set {CATALOG_SOURCE_ENV}, add phoxal_artifacts.catalog in robot.yaml, or run `phoxal-cli pull --catalog <source>` to refresh the cache. The future default catalog URL is {DEFAULT_CATALOG_URL}."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_verification_rejects_edits() {
        let mut revision = fixture_catalog_for_tests(vec![fixture_service_entry_for_tests(
            "drive",
            "y2026_1",
            "0.1.0",
            Channel::Stable,
            "aarch64-unknown-linux-gnu",
            ArtifactStatus::Pending,
            vec![fixture_contract_for_tests(
                "drive::Target",
                "drive/target",
                "publish",
                "0123456789abcdef",
            )],
        )]);
        revision.entries[0].package = "phoxal-service-edited".to_string();

        let error = revision.verify().expect_err("edited catalog should fail");
        assert!(format!("{error:#}").contains("checksum mismatch"));
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
            ArtifactStatus::Released,
            vec![fixture_contract_for_tests(
                "drive::Target",
                "drive/target",
                "publish",
                "0123456789abcdef",
            )],
        );
        entry.release_assets.insert(
            "aarch64-unknown-linux-gnu".to_string(),
            ReleaseAsset {
                asset: "drive.tar.zst".to_string(),
                sha256: "a".repeat(64),
                metadata: ReleaseAssetMetadata {
                    emit_apis: "drive.emit-apis.json".to_string(),
                    emit_apis_sha256: "b".repeat(64),
                    sha256_file: "drive.tar.zst.sha256".to_string(),
                },
            },
        );
        let catalog = fixture_catalog_for_tests(vec![entry]);

        let promoted = promoted_preview_features(&manifest_path, &catalog)?;

        assert_eq!(promoted.len(), 1);
        assert_eq!(promoted[0].generation, "y2026_2");
        assert_eq!(promoted[0].line_number, 2);
        Ok(())
    }
}
