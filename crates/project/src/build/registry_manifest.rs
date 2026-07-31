//! Verified, immutable access to one manifest from the pinned sparse registry.
//!
//! Runtime crates are binary-only packages, so Cargo exposes no manifest-only
//! download operation. This module is the CLI's sole HTTP boundary for reading
//! their build declarations without materializing their full source trees.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use flate2::read::GzDecoder;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_HTTP_BYTES: u64 = 64 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

fn read_bounded(mut reader: impl Read, label: &str, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {label}"))?;
    ensure!(
        bytes.len() as u64 <= limit,
        "{label} exceeds the {limit}-byte limit"
    );
    Ok(bytes)
}

/// Injectable transport so unit tests never touch the network.
pub trait RegistryHttp {
    fn get(&self, url: &str) -> Result<Vec<u8>>;
}

/// Blocking HTTP implementation used by the synchronous build path.
pub struct HttpClient {
    client: reqwest::blocking::Client,
}

impl HttpClient {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: reqwest::blocking::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .user_agent("phoxal-cli registry manifest reader")
                .build()
                .context("failed to construct the registry HTTP client")?,
        })
    }
}

impl RegistryHttp for HttpClient {
    fn get(&self, url: &str) -> Result<Vec<u8>> {
        let response = self
            .client
            .get(url)
            .send()
            .with_context(|| format!("failed to GET {url}"))?
            .error_for_status()
            .with_context(|| format!("registry GET {url} returned an error"))?;
        if let Some(length) = response.content_length() {
            ensure!(
                length <= MAX_HTTP_BYTES,
                "registry response from {url} is {length} bytes, above the {MAX_HTTP_BYTES}-byte limit"
            );
        }
        read_bounded(
            response,
            &format!("registry response from {url}"),
            MAX_HTTP_BYTES,
        )
    }
}

#[derive(Debug, Clone, Deserialize)]
struct RegistryConfig {
    dl: String,
}

/// Immutable manifest cache rooted under a project's `.phoxal` directory.
pub struct ManifestCache {
    root: PathBuf,
    config: Mutex<Option<RegistryConfig>>,
}

impl ManifestCache {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            config: Mutex::new(None),
        }
    }

    fn manifest_path(&self, package: &str, version: &str) -> Result<PathBuf> {
        ensure!(
            package
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
                && !package.is_empty(),
            "invalid registry package name {package:?}"
        );
        semver::Version::parse(version)
            .with_context(|| format!("invalid registry package version {version:?}"))?;
        Ok(self.root.join(format!("{package}-{version}.toml")))
    }

    fn config(&self, http: &dyn RegistryHttp, base: &str) -> Result<RegistryConfig> {
        let mut memo = self
            .config
            .lock()
            .map_err(|_| anyhow::anyhow!("registry configuration cache lock was poisoned"))?;
        if let Some(config) = memo.as_ref() {
            return Ok(config.clone());
        }
        let url = format!("{base}/config.json");
        let bytes = http.get(&url)?;
        let config: RegistryConfig = serde_json::from_slice(&bytes)
            .with_context(|| format!("registry configuration at {url} is not valid JSON"))?;
        *memo = Some(config.clone());
        Ok(config)
    }
}

#[derive(Deserialize)]
struct IndexEntry {
    vers: String,
    cksum: String,
}

/// The sparse-registry path for a Cargo package name.
fn index_path(package: &str) -> Result<String> {
    ensure!(
        !package.is_empty() && package.is_ascii(),
        "registry package name must be non-empty ASCII"
    );
    let name = package.to_ascii_lowercase();
    Ok(match name.len() {
        1 => format!("1/{name}"),
        2 => format!("2/{name}"),
        3 => format!("3/{}/{name}", &name[..1]),
        _ => format!("{}/{}/{name}", &name[..2], &name[2..4]),
    })
}

fn prefix(package: &str) -> Result<String> {
    ensure!(
        !package.is_empty() && package.is_ascii(),
        "registry package name must be non-empty ASCII"
    );
    Ok(match package.len() {
        1 => "1".to_string(),
        2 => "2".to_string(),
        3 => format!("3/{}", &package[..1]),
        _ => format!("{}/{}", &package[..2], &package[2..4]),
    })
}

fn download_url(template: &str, package: &str, version: &str) -> Result<String> {
    let lower = package.to_ascii_lowercase();
    let package_prefix = prefix(package)?;
    let lower_prefix = prefix(&lower)?;
    let has_marker = ["{crate}", "{version}", "{prefix}", "{lowerprefix}"]
        .iter()
        .any(|marker| template.contains(marker));
    let mut rendered = template
        .replace("{crate}", package)
        .replace("{version}", version)
        .replace("{prefix}", &package_prefix)
        .replace("{lowerprefix}", &lower_prefix);
    if !has_marker {
        rendered = format!(
            "{}/{package}/{version}/download",
            template.trim_end_matches('/')
        );
    }
    Ok(rendered)
}

fn read_manifest_from_crate(bytes: &[u8], package: &str, version: &str) -> Result<String> {
    let expected = PathBuf::from(format!("{package}-{version}/Cargo.toml"));
    let mut archive = tar::Archive::new(GzDecoder::new(bytes));
    for entry in archive.entries().context("failed to read .crate archive")? {
        let entry = entry.context("failed to read .crate archive entry")?;
        if entry
            .path()
            .context("invalid .crate archive path")?
            .as_ref()
            == expected
        {
            ensure!(
                entry.size() <= MAX_MANIFEST_BYTES,
                "published Cargo.toml for {package}@{version} exceeds the {MAX_MANIFEST_BYTES}-byte limit"
            );
            let bytes = read_bounded(
                entry,
                &format!("published Cargo.toml for {package}@{version}"),
                MAX_MANIFEST_BYTES,
            )?;
            return String::from_utf8(bytes).context("published Cargo.toml is not valid UTF-8");
        }
    }
    bail!(
        "published package {package}@{version} does not contain {}",
        expected.display()
    )
}

fn verify_manifest_identity(source: &str, package: &str, version: &str) -> Result<()> {
    let manifest: toml::Value = toml::from_str(source)
        .with_context(|| format!("published manifest for {package}@{version} is invalid TOML"))?;
    let actual_name = manifest
        .get("package")
        .and_then(|value| value.get("name"))
        .and_then(toml::Value::as_str);
    let actual_version = manifest
        .get("package")
        .and_then(|value| value.get("version"))
        .and_then(toml::Value::as_str);
    ensure!(
        actual_name == Some(package) && actual_version == Some(version),
        "published manifest identity mismatch for {package}@{version}: found name={actual_name:?}, version={actual_version:?}"
    );
    Ok(())
}

fn write_atomic(path: &Path, source: &str) -> Result<()> {
    let parent = path
        .parent()
        .context("manifest cache path has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create manifest cache {}", parent.display()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create a temporary file in {}", parent.display()))?;
    temporary
        .write_all(source.as_bytes())
        .context("failed to write verified manifest cache entry")?;
    temporary
        .as_file_mut()
        .sync_all()
        .context("failed to sync verified manifest cache entry")?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to publish manifest cache entry {}", path.display()))?;
    Ok(())
}

/// Fetch, verify, and immutably cache one published runtime manifest.
pub fn fetch_runtime_manifest(
    http: &dyn RegistryHttp,
    cache: &ManifestCache,
    package: &str,
    version: &str,
    offline: bool,
) -> Result<String> {
    let cache_path = cache.manifest_path(package, version)?;
    if cache_path.is_file() {
        return fs::read_to_string(&cache_path)
            .with_context(|| format!("failed to read manifest cache {}", cache_path.display()));
    }
    if offline {
        bail!(
            "offline build needs the cached manifest for {package}@{version}, but {} is missing; rerun the same `phoxal build --builder container` command without `--offline` once to fill it",
            cache_path.display()
        );
    }

    let base = phoxal_cli_core::project::catalog::REGISTRY_INDEX
        .strip_prefix("sparse+")
        .context("the configured registry index is not a sparse URL")?
        .trim_end_matches('/');
    let config = cache.config(http, base)?;
    let index_url = format!("{base}/{}", index_path(package)?);
    let index = String::from_utf8(http.get(&index_url)?)
        .with_context(|| format!("registry index {index_url} is not UTF-8"))?;
    let mut expected_checksum = None;
    for line in index.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(entry) = serde_json::from_str::<IndexEntry>(line) else {
            continue;
        };
        if entry.vers == version {
            expected_checksum = Some(entry.cksum);
            break;
        }
    }
    let expected_checksum = expected_checksum.with_context(|| {
        format!("registry index {index_url} has no exact {package}@{version} entry")
    })?;
    let crate_url = download_url(&config.dl, package, version)?;
    let crate_bytes = http.get(&crate_url)?;
    let actual_checksum = hex::encode(Sha256::digest(&crate_bytes));
    ensure!(
        actual_checksum == expected_checksum,
        "checksum mismatch for {package}@{version} from {crate_url}: expected {expected_checksum}, got {actual_checksum}"
    );
    let source = read_manifest_from_crate(&crate_bytes, package, version)?;
    verify_manifest_identity(&source, package, version)?;
    write_atomic(&cache_path, &source)?;
    Ok(source)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::*;

    struct FakeHttp {
        responses: BTreeMap<String, Vec<u8>>,
        calls: AtomicUsize,
        panic_on_call: bool,
    }

    impl RegistryHttp for FakeHttp {
        fn get(&self, url: &str) -> Result<Vec<u8>> {
            assert!(!self.panic_on_call, "cache hit touched the network: {url}");
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.responses
                .get(url)
                .cloned()
                .with_context(|| format!("unexpected fake URL {url}"))
        }
    }

    fn crate_bytes(package: &str, version: &str, manifest: &str) -> Vec<u8> {
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut archive = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(manifest.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive
            .append_data(
                &mut header,
                format!("{package}-{version}/Cargo.toml"),
                manifest.as_bytes(),
            )
            .unwrap();
        archive.into_inner().unwrap().finish().unwrap()
    }

    fn fixture(
        package: &str,
        version: &str,
        archived_package: &str,
        archived_version: &str,
        corrupt_checksum: bool,
        include_version: bool,
    ) -> FakeHttp {
        let base = "https://phoxal.github.io/registry";
        let manifest =
            format!("[package]\nname = {archived_package:?}\nversion = {archived_version:?}\n");
        let bytes = crate_bytes(package, version, &manifest);
        let checksum = if corrupt_checksum {
            "00".repeat(32)
        } else {
            hex::encode(Sha256::digest(&bytes))
        };
        let mut responses = BTreeMap::new();
        responses.insert(
            format!("{base}/config.json"),
            br#"{"dl":"https://download.invalid/{lowerprefix}/{crate}/{version}.crate"}"#.to_vec(),
        );
        let index = if include_version {
            format!("not-json\n{{\"vers\":\"{version}\",\"cksum\":\"{checksum}\"}}")
        } else {
            format!(r#"{{"vers":"0.0.1","cksum":"{checksum}"}}"#)
        };
        responses.insert(
            format!("{base}/{}", index_path(package).unwrap()),
            index.into_bytes(),
        );
        responses.insert(
            format!(
                "https://download.invalid/{}/{package}/{version}.crate",
                index_path(package).unwrap().rsplit_once('/').unwrap().0
            ),
            bytes,
        );
        FakeHttp {
            responses,
            calls: AtomicUsize::new(0),
            panic_on_call: false,
        }
    }

    #[test]
    fn a_verified_manifest_is_fetched_extracted_and_cached() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let cache = ManifestCache::new(temp.path().to_path_buf());
        let http = fixture(
            "phoxal-tool-joypad",
            "1.2.3",
            "phoxal-tool-joypad",
            "1.2.3",
            false,
            true,
        );
        let source = fetch_runtime_manifest(&http, &cache, "phoxal-tool-joypad", "1.2.3", false)?;
        assert!(source.contains("phoxal-tool-joypad"));
        assert!(temp.path().join("phoxal-tool-joypad-1.2.3.toml").is_file());
        assert_eq!(http.calls.load(Ordering::SeqCst), 3);
        Ok(())
    }

    #[test]
    fn a_cache_hit_never_touches_the_network() -> Result<()> {
        let temp = tempfile::tempdir()?;
        fs::write(temp.path().join("pkg-1.2.3.toml"), "cached")?;
        let cache = ManifestCache::new(temp.path().to_path_buf());
        let http = FakeHttp {
            responses: BTreeMap::new(),
            calls: AtomicUsize::new(0),
            panic_on_call: true,
        };
        assert_eq!(
            fetch_runtime_manifest(&http, &cache, "pkg", "1.2.3", true)?,
            "cached"
        );
        Ok(())
    }

    #[test]
    fn a_checksum_mismatch_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let cache = ManifestCache::new(temp.path().to_path_buf());
        let http = fixture("pkg", "1.2.3", "pkg", "1.2.3", true, true);
        assert!(
            fetch_runtime_manifest(&http, &cache, "pkg", "1.2.3", false)
                .unwrap_err()
                .to_string()
                .contains("checksum mismatch")
        );
    }

    #[test]
    fn a_missing_exact_version_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let cache = ManifestCache::new(temp.path().to_path_buf());
        let http = fixture("pkg", "1.2.3", "pkg", "1.2.3", false, false);
        assert!(
            fetch_runtime_manifest(&http, &cache, "pkg", "1.2.3", false)
                .unwrap_err()
                .to_string()
                .contains("no exact pkg@1.2.3")
        );
    }

    #[test]
    fn a_mismatched_archived_identity_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let cache = ManifestCache::new(temp.path().to_path_buf());
        let http = fixture("pkg", "1.2.3", "other", "1.2.3", false, true);
        assert!(
            fetch_runtime_manifest(&http, &cache, "pkg", "1.2.3", false)
                .unwrap_err()
                .to_string()
                .contains("identity mismatch")
        );
    }

    #[test]
    fn offline_with_a_cold_cache_fails_actionably() {
        let temp = tempfile::tempdir().unwrap();
        let cache = ManifestCache::new(temp.path().to_path_buf());
        let http = FakeHttp {
            responses: BTreeMap::new(),
            calls: AtomicUsize::new(0),
            panic_on_call: true,
        };
        let error = fetch_runtime_manifest(&http, &cache, "pkg", "1.2.3", true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("without `--offline`"), "{error}");
        assert!(error.contains("pkg-1.2.3.toml"), "{error}");
    }

    #[test]
    fn index_paths_follow_the_sparse_registry_rules() -> Result<()> {
        assert_eq!(index_path("a")?, "1/a");
        assert_eq!(index_path("ab")?, "2/ab");
        assert_eq!(index_path("abc")?, "3/a/abc");
        assert_eq!(
            index_path("Phoxal-Tool-Joypad")?,
            "ph/ox/phoxal-tool-joypad"
        );
        Ok(())
    }

    #[test]
    fn fallback_download_urls_follow_the_sparse_protocol() -> Result<()> {
        assert_eq!(
            download_url("https://example.invalid/api", "pkg", "1.2.3")?,
            "https://example.invalid/api/pkg/1.2.3/download"
        );
        assert_eq!(
            download_url(
                "https://example.invalid/{prefix}/{lowerprefix}/{crate}/{version}",
                "AbCd",
                "1.2.3"
            )?,
            "https://example.invalid/Ab/Cd/ab/cd/AbCd/1.2.3"
        );
        Ok(())
    }

    #[test]
    fn bounded_reads_reject_oversized_payloads() {
        let error = read_bounded(std::io::Cursor::new(vec![0; 9]), "fixture", 8)
            .expect_err("one byte above the limit must fail");
        assert!(error.to_string().contains("8-byte limit"), "{error:#}");
    }

    #[test]
    fn index_paths_reject_non_ascii_names_without_panicking() {
        let error = index_path("éclair").expect_err("non-ASCII names must fail");
        assert!(error.to_string().contains("ASCII"), "{error:#}");
    }
}
