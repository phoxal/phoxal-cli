//! Immutable per-framework-train suite loading and integrity validation.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};

pub use phoxal::suite::{Artifact, Blob, Kind, Suite};

pub const SUITE_SOURCE_ENV: &str = "PHOXAL_SUITE";
pub const OFFLINE_ENV: &str = "PHOXAL_OFFLINE";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuiteLoadOptions {
    pub cli_source: Option<String>,
    pub offline: bool,
}

impl SuiteLoadOptions {
    #[must_use]
    pub fn explicit_source(&self) -> Option<String> {
        self.cli_source
            .clone()
            .or_else(|| std::env::var(SUITE_SOURCE_ENV).ok())
    }
}

#[must_use]
pub fn release_url(version: &str) -> String {
    format!("https://github.com/phoxal/framework/releases/tag/v{version}")
}

#[must_use]
pub fn suite_url(version: &str) -> String {
    format!("https://github.com/phoxal/framework/releases/download/v{version}/suite.json")
}

pub fn load_suite(options: SuiteLoadOptions, locked_version: &str) -> Result<Option<Suite>> {
    let explicit_source = options.explicit_source();
    let offline = options.offline || offline_from_env();
    let source = if offline {
        let source = explicit_source.context(
            "offline resolution requires an explicit local suite; pass --suite <path> or PHOXAL_SUITE=<path>",
        )?;
        ensure!(
            !source.starts_with("https://") && !source.starts_with("http://"),
            "offline suite source must be a local path: {source}"
        );
        source
    } else {
        explicit_source.unwrap_or_else(|| suite_url(locked_version))
    };
    let suite = read_source(&source).with_context(|| {
        format!(
            "locked phoxal train {locked_version} is missing its immutable suite; expected {} (release {}). This framework publication is incomplete; retry after the release finishes",
            suite_url(locked_version),
            release_url(locked_version)
        )
    })?;
    ensure!(
        suite.version == locked_version,
        "suite integrity error: locked phoxal train is {locked_version}, but suite.json.version is {}; refuse to mix framework trains",
        suite.version
    );
    Ok(Some(suite))
}

pub fn read_suite_path(path: &Path) -> Result<Suite> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read suite {}", path.display()))?;
    parse_suite(&text).with_context(|| format!("{} is not a valid suite", path.display()))
}

fn read_source(source: &str) -> Result<Suite> {
    if source.starts_with("https://") {
        fetch_https(source)
    } else if source.starts_with("http://") {
        bail!("suite source must use HTTPS or a local path: {source}");
    } else {
        read_suite_path(Path::new(source))
    }
}

fn parse_suite(text: &str) -> Result<Suite> {
    let suite: Suite = serde_json::from_str(text).context("suite was not valid JSON")?;
    ensure!(
        suite.schema == phoxal::suite::SCHEMA,
        "unsupported suite schema {:?}; expected {:?}",
        suite.schema,
        phoxal::suite::SCHEMA
    );
    validate_suite(&suite)?;
    Ok(suite)
}

fn validate_suite(suite: &Suite) -> Result<()> {
    ensure!(!suite.version.is_empty(), "suite version is empty");
    let mut identities = BTreeSet::new();
    for artifact in &suite.artifacts {
        ensure!(
            !artifact.id.is_empty(),
            "suite contains an artifact with an empty id"
        );
        ensure!(
            identities.insert(artifact.id.as_str()),
            "suite contains duplicate artifact {}",
            artifact.id
        );
        ensure!(
            !artifact.targets.is_empty(),
            "suite artifact {} has no target archives",
            artifact.id
        );
        for (target, blob) in &artifact.targets {
            validate_blob(blob)
                .with_context(|| format!("invalid target blob for {} ({target})", artifact.id))?;
        }
        if let Some(blob) = &artifact.assets {
            validate_blob(blob)
                .with_context(|| format!("invalid assets blob for {}", artifact.id))?;
        }
    }
    Ok(())
}

fn validate_blob(blob: &Blob) -> Result<()> {
    ensure!(blob.url.starts_with("https://"), "blob URL must use HTTPS");
    ensure!(
        phoxal::suite::is_sha256(&blob.sha256),
        "blob sha256 must be lowercase hexadecimal"
    );
    ensure!(blob.size > 0, "blob size must be greater than zero");
    Ok(())
}

fn fetch_https(url: &str) -> Result<Suite> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("phoxal-cli")
        .connect_timeout(REQUEST_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()?;
    let response = client.get(url).send()?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        bail!("suite surface is missing: {url} returned HTTP {status}");
    }
    if !status.is_success() {
        bail!("suite request {url} returned HTTP {status}; retry without changing Cargo.lock");
    }
    parse_suite(&response.text()?)
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
}

impl std::fmt::Display for ArtifactKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.emit_apis_kind())
    }
}

#[must_use]
pub const fn artifact_kind(kind: Kind, assets: bool) -> ArtifactKind {
    match kind {
        Kind::Service => ArtifactKind::Service,
        Kind::Component if assets => ArtifactKind::ComponentAssets,
        Kind::Component => ArtifactKind::ComponentDriver,
        Kind::Tool => ArtifactKind::Tool,
        Kind::Simulator => ArtifactKind::Simulator,
        Kind::Infrastructure => ArtifactKind::Infrastructure,
    }
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

#[must_use]
pub fn artifacts_of_kind(suite: &Suite, kind: Kind) -> Vec<&Artifact> {
    suite
        .artifacts
        .iter()
        .filter(|artifact| artifact.kind == kind)
        .collect()
}

pub fn select_artifact<'a>(suite: &'a Suite, id: &str, target: &str) -> Result<&'a Artifact> {
    let artifact = suite
        .artifacts
        .iter()
        .find(|artifact| artifact.id == id)
        .with_context(|| {
            format!(
                "required artifact {id} is absent from train {} suite",
                suite.version
            )
        })?;
    ensure!(
        artifact.targets.contains_key(target),
        "framework train {} does not support target {target} for {id}; available targets: {}",
        suite.version,
        artifact
            .targets
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(artifact)
}

#[must_use]
pub fn offline_from_env() -> bool {
    std::env::var(OFFLINE_ENV)
        .is_ok_and(|value| !value.is_empty() && !matches!(value.as_str(), "0" | "false" | "no"))
}

#[doc(hidden)]
pub fn fixture_blob_for_tests(url: &str, sha256: &str, size: u64) -> Blob {
    Blob {
        url: url.into(),
        sha256: sha256.into(),
        size,
    }
}

#[doc(hidden)]
pub struct FixtureArtifact {
    pub artifact: Artifact,
    version: String,
}

impl FixtureArtifact {
    #[doc(hidden)]
    pub fn as_asset_entry_mut(&mut self) -> &mut Artifact {
        &mut self.artifact
    }

    #[doc(hidden)]
    pub fn as_artifact_entry_mut(&mut self) -> &mut Artifact {
        &mut self.artifact
    }
}

#[doc(hidden)]
pub fn fixture_suite_for_tests(entries: Vec<FixtureArtifact>) -> Suite {
    let version = entries
        .first()
        .map_or_else(|| "0.36.0".to_string(), |entry| entry.version.clone());
    let mut artifacts: Vec<Artifact> = Vec::new();
    for entry in entries {
        if let Some(existing) = artifacts
            .iter_mut()
            .find(|artifact| artifact.id == entry.artifact.id)
        {
            existing.targets.extend(entry.artifact.targets);
            if entry.artifact.assets.is_some() {
                existing.assets = entry.artifact.assets;
            }
        } else {
            artifacts.push(entry.artifact);
        }
    }
    Suite::new(version, artifacts)
}

#[doc(hidden)]
pub fn fixture_artifact_for_tests(filename: &str, sha256: &str) -> Blob {
    fixture_blob_for_tests(&format!("https://example.invalid/{filename}"), sha256, 1)
}

#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct FixtureContract;

#[doc(hidden)]
pub fn fixture_contract_for_tests(_name: &str, _role: &str) -> FixtureContract {
    FixtureContract
}

fn fixture_entry(
    id: String,
    kind: Kind,
    version: &str,
    target: &str,
    published: bool,
) -> FixtureArtifact {
    let blob = fixture_blob_for_tests(
        &format!("https://example.invalid/{id}/{target}"),
        &"a".repeat(64),
        1,
    );
    FixtureArtifact {
        artifact: Artifact {
            id,
            kind,
            targets: if published {
                [(target.to_string(), blob)].into()
            } else {
                Default::default()
            },
            assets: None,
        },
        version: version.into(),
    }
}

#[doc(hidden)]
pub fn fixture_service_entry_for_tests(
    name: &str,
    version: &str,
    target: &str,
    published: bool,
    _contracts: Vec<FixtureContract>,
) -> FixtureArtifact {
    fixture_entry(
        format!("phoxal/service-{name}"),
        Kind::Service,
        version,
        target,
        published,
    )
}

#[doc(hidden)]
pub fn fixture_simulator_entry_for_tests(
    name: &str,
    version: &str,
    target: &str,
    published: bool,
    _contracts: Vec<FixtureContract>,
) -> FixtureArtifact {
    fixture_entry(
        format!("phoxal/simulator-{name}"),
        Kind::Simulator,
        version,
        target,
        published,
    )
}

#[doc(hidden)]
pub fn fixture_tool_entry_for_tests(
    name: &str,
    version: &str,
    target: &str,
    published: bool,
    _contracts: Vec<FixtureContract>,
) -> FixtureArtifact {
    let kind = if name == "router" {
        Kind::Infrastructure
    } else {
        Kind::Tool
    };
    let prefix = if kind == Kind::Infrastructure {
        "infrastructure"
    } else {
        "tool"
    };
    fixture_entry(
        format!("phoxal/{prefix}-{name}"),
        kind,
        version,
        target,
        published,
    )
}

#[doc(hidden)]
pub fn fixture_component_assets_entry_for_tests(name: &str, version: &str) -> FixtureArtifact {
    let mut entry = fixture_entry(
        format!("phoxal/component-{name}"),
        Kind::Component,
        version,
        "fixture-target",
        true,
    );
    entry.artifact.assets = Some(fixture_blob_for_tests(
        &format!("https://example.invalid/phoxal/component-{name}/assets"),
        &"b".repeat(64),
        1,
    ));
    entry
}

#[doc(hidden)]
pub fn fixture_component_driver_entry_for_tests(
    name: &str,
    version: &str,
    target: &str,
    published: bool,
    _contracts: Vec<FixtureContract>,
) -> FixtureArtifact {
    fixture_entry(
        format!("phoxal/component-{name}"),
        Kind::Component,
        version,
        target,
        published,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suite_url_is_exact_train_release() {
        assert_eq!(
            suite_url("0.36.0"),
            "https://github.com/phoxal/framework/releases/download/v0.36.0/suite.json"
        );
    }

    #[test]
    fn version_mismatch_is_integrity_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("suite.json");
        fs::write(
            &path,
            r#"{"schema":"phoxal.suite/v0","version":"0.35.0","artifacts":[]}"#,
        )
        .unwrap();
        let error = load_suite(
            SuiteLoadOptions {
                cli_source: Some(path.display().to_string()),
                offline: false,
            },
            "0.36.0",
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("suite.json.version"));
    }

    #[test]
    fn one_cli_loads_two_historical_train_descriptors() {
        let dir = tempfile::tempdir().unwrap();
        for version in ["0.35.1", "0.36.0"] {
            let path = dir.path().join(format!("suite-{version}.json"));
            fs::write(
                &path,
                format!(r#"{{"schema":"phoxal.suite/v0","version":"{version}","artifacts":[]}}"#),
            )
            .unwrap();
            let suite = load_suite(
                SuiteLoadOptions {
                    cli_source: Some(path.display().to_string()),
                    offline: false,
                },
                version,
            )
            .unwrap()
            .unwrap();
            assert_eq!(suite.version, version);
        }
    }

    #[test]
    fn offline_load_requires_and_accepts_an_explicit_local_suite() {
        let missing = load_suite(
            SuiteLoadOptions {
                cli_source: None,
                offline: true,
            },
            "0.36.0",
        )
        .unwrap_err();
        assert!(format!("{missing:#}").contains("explicit local suite"));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("suite.json");
        fs::write(
            &path,
            r#"{"schema":"phoxal.suite/v0","version":"0.36.0","artifacts":[]}"#,
        )
        .unwrap();
        let suite = load_suite(
            SuiteLoadOptions {
                cli_source: Some(path.display().to_string()),
                offline: true,
            },
            "0.36.0",
        )
        .unwrap()
        .unwrap();
        assert_eq!(suite.version, "0.36.0");
    }

    #[test]
    fn offline_load_rejects_https_suite_sources() {
        let error = load_suite(
            SuiteLoadOptions {
                cli_source: Some(suite_url("0.36.0")),
                offline: true,
            },
            "0.36.0",
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("local path"));
    }

    #[test]
    fn malformed_integrity_metadata_is_rejected() {
        let error = parse_suite(
            r#"{"schema":"phoxal.suite/v0","version":"0.36.0","artifacts":[{"id":"phoxal/service-drive","kind":"service","targets":{"aarch64-unknown-linux-gnu":{"url":"https://example.invalid/drive","sha256":"bad","size":0}}}]}"#,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("sha256") || message.contains("size"),
            "{message}"
        );
    }

    #[test]
    fn missing_target_names_the_train_and_available_targets() {
        let suite = Suite::new(
            "0.36.0",
            vec![Artifact {
                id: "phoxal/service-drive".into(),
                kind: Kind::Service,
                targets: [(
                    "aarch64-unknown-linux-gnu".into(),
                    fixture_blob_for_tests("https://example.invalid/drive", &"a".repeat(64), 1),
                )]
                .into(),
                assets: None,
            }],
        );
        let error = select_artifact(&suite, "phoxal/service-drive", "x86_64-unknown-linux-gnu")
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("0.36.0"), "{message}");
        assert!(message.contains("aarch64-unknown-linux-gnu"), "{message}");
    }
}
