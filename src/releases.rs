use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, SecondsFormat, Utc};
use semver::Version;
use serde::{Deserialize, Serialize};

const RELEASES_URL: &str = "https://api.github.com/repos/phoxal/framework/releases?per_page=100";
const WORKSPACE_MANIFEST_URL: &str =
    "https://raw.githubusercontent.com/phoxal/framework/main/Cargo.toml";
const CACHE_TTL: Duration = Duration::from_secs(3600);
const CACHE_FILE: &str = "releases.json";

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReleasesSnapshot {
    #[serde(with = "system_time_rfc3339")]
    pub fetched_at: SystemTime,
    pub versions: Vec<String>,
}

impl ReleasesSnapshot {
    pub fn versions_semver(&self) -> Result<Vec<Version>> {
        self.versions
            .iter()
            .map(|raw| {
                Version::parse(raw).with_context(|| format!("invalid cached version '{raw}'"))
            })
            .collect()
    }
}

pub fn read_cache(cache_dir: &Path) -> Option<ReleasesSnapshot> {
    let path = cache_path(cache_dir);
    let contents = fs::read_to_string(path).ok()?;
    let snapshot: ReleasesSnapshot = serde_json::from_str(&contents).ok()?;
    match SystemTime::now().duration_since(snapshot.fetched_at) {
        Ok(age) if age <= CACHE_TTL => Some(snapshot),
        Err(_) => Some(snapshot),
        _ => None,
    }
}

pub fn write_cache(cache_dir: &Path, snapshot: &ReleasesSnapshot) -> Result<()> {
    fs::create_dir_all(cache_dir)
        .with_context(|| format!("failed to create {}", cache_dir.display()))?;
    let contents =
        serde_json::to_string_pretty(snapshot).context("failed to serialize releases cache")?;
    fs::write(cache_path(cache_dir), contents)
        .with_context(|| format!("failed to write releases cache in {}", cache_dir.display()))
}

pub fn fetch_remote() -> Result<ReleasesSnapshot> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("phoxal-cli")
        .timeout(Duration::from_secs(15))
        .build()?;
    let mut req = client.get(RELEASES_URL);
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        req = req.bearer_auth(token);
    }
    let response = req
        .send()
        .context("failed to fetch phoxal/framework releases")?;
    if !response.status().is_success() {
        let status = response.status();
        if status.as_u16() == 403 {
            bail!(
                "GitHub releases API returned 403 (likely anonymous rate limit \
                 of 60/hour from this network - wait an hour, or set \
                 GITHUB_TOKEN env var for 5000/hour)"
            );
        }
        bail!("releases fetch returned {status}");
    }

    #[derive(Deserialize)]
    struct GhRelease {
        tag_name: String,
        draft: bool,
        prerelease: bool,
    }

    let body: Vec<GhRelease> = response
        .json()
        .context("failed to parse phoxal/framework releases response")?;
    let mut versions = Vec::new();
    for release in body {
        if release.draft || release.prerelease {
            continue;
        }
        let trimmed = release
            .tag_name
            .strip_prefix('v')
            .unwrap_or(&release.tag_name);
        if let Ok(version) = Version::parse(trimmed) {
            let version = version.to_string();
            if !versions.contains(&version) {
                versions.push(version);
            }
        }
    }
    if versions.is_empty()
        && let Some(version) = fetch_workspace_manifest_version(&client)?
    {
        versions.push(version);
    }

    Ok(ReleasesSnapshot {
        fetched_at: SystemTime::now(),
        versions,
    })
}

fn fetch_workspace_manifest_version(client: &reqwest::blocking::Client) -> Result<Option<String>> {
    let response = client
        .get(WORKSPACE_MANIFEST_URL)
        .send()
        .context("failed to fetch phoxal/framework workspace manifest after empty release list")?;
    if !response.status().is_success() {
        bail!(
            "no phoxal/framework releases were published and workspace manifest fetch returned {}",
            response.status()
        );
    }
    let manifest = response
        .text()
        .context("failed to read phoxal/framework workspace manifest")?;
    let parsed = manifest
        .parse::<toml::Table>()
        .context("failed to parse phoxal/framework workspace manifest")?;
    let Some(version) = parsed
        .get("workspace")
        .and_then(|workspace| workspace.get("package"))
        .and_then(|package| package.get("version"))
        .and_then(toml::Value::as_str)
    else {
        return Ok(None);
    };
    Version::parse(version)
        .with_context(|| format!("invalid phoxal/framework workspace version '{version}'"))?;
    Ok(Some(version.to_string()))
}

pub fn known_releases(cache_dir: &Path) -> Result<Vec<Version>> {
    known_releases_snapshot(cache_dir)?.versions_semver()
}

pub fn known_releases_snapshot(cache_dir: &Path) -> Result<ReleasesSnapshot> {
    if let Some(snapshot) = read_cache(cache_dir) {
        return Ok(snapshot);
    }
    refresh(cache_dir)
}

pub fn refresh(cache_dir: &Path) -> Result<ReleasesSnapshot> {
    let path = cache_path(cache_dir);
    if path.is_file() {
        fs::remove_file(&path)
            .with_context(|| format!("failed to remove stale releases cache {}", path.display()))?;
    }
    let snapshot = fetch_remote()?;
    write_cache(cache_dir, &snapshot)?;
    Ok(snapshot)
}

fn cache_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join(CACHE_FILE)
}

mod system_time_rfc3339 {
    use super::*;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S>(time: &SystemTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let datetime = DateTime::<Utc>::from(*time);
        serializer.serialize_str(&datetime.to_rfc3339_opts(SecondsFormat::Secs, true))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<SystemTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        let datetime = DateTime::parse_from_rfc3339(&raw)
            .map_err(serde::de::Error::custom)?
            .with_timezone(&Utc);
        Ok(SystemTime::from(datetime))
    }
}
