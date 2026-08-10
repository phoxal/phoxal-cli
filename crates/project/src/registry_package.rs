//! Verified, immutable access to packages from the pinned sparse registry.
//!
//! This is deliberately independent of Cargo's private cache. Both container
//! build metadata and authored component roots consume the same checked archive.

use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use flate2::read::GzDecoder;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_HTTP_BYTES: u64 = 64 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 10_000;
const MAX_EXPANDED_BYTES: u64 = 256 * 1024 * 1024;
const EXTRACTION_MARKER: &str = ".phoxal-verified-package";

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

fn read_cached_archive(path: &Path) -> Result<Vec<u8>> {
    let length = fs::metadata(path)
        .with_context(|| format!("failed to inspect cached package {}", path.display()))?
        .len();
    ensure!(
        length <= MAX_HTTP_BYTES,
        "cached package {} is {length} bytes, above the {MAX_HTTP_BYTES}-byte limit",
        path.display()
    );
    let file =
        fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    read_bounded(
        file,
        &format!("cached package {}", path.display()),
        MAX_HTTP_BYTES,
    )
}

/// Injectable transport so unit tests never touch the network.
pub(crate) trait RegistryHttp {
    fn get(&self, url: &str) -> Result<Vec<u8>>;
}

/// Blocking HTTP implementation used by the synchronous build path.
pub(crate) struct HttpClient {
    client: reqwest::blocking::Client,
}

impl HttpClient {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            client: reqwest::blocking::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .user_agent("phoxal-cli registry package reader")
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

/// Immutable verified-package cache rooted at `.phoxal/cache/registry`.
pub(crate) struct PackageCache {
    root: PathBuf,
    config: Mutex<Option<RegistryConfig>>,
}

impl PackageCache {
    #[must_use]
    pub(crate) fn new(root: PathBuf) -> Self {
        Self {
            root,
            config: Mutex::new(None),
        }
    }

    fn package_dir(&self, package: &str, version: &str) -> Result<PathBuf> {
        ensure!(
            package
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
                && !package.is_empty(),
            "invalid registry package name {package:?}"
        );
        semver::Version::parse(version)
            .with_context(|| format!("invalid registry package version {version:?}"))?;
        Ok(self.root.join(package).join(version))
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
pub(crate) fn index_path(package: &str) -> Result<String> {
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

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("package cache path has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create package cache {}", parent.display()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create a temporary file in {}", parent.display()))?;
    temporary
        .write_all(bytes)
        .context("failed to write verified package cache entry")?;
    temporary
        .as_file_mut()
        .sync_all()
        .context("failed to sync verified package cache entry")?;
    // `rename` atomically replaces a known-corrupt online entry on supported
    // Unix hosts; the temporary is created beside the destination so the
    // operation cannot cross filesystems.
    fs::rename(temporary.path(), path)
        .with_context(|| format!("failed to publish package cache entry {}", path.display()))?;
    temporary.disable_cleanup(true);
    Ok(())
}

/// One checksum-verified sparse-registry package. Its archive remains the
/// authoritative cache entry; manifest and component-root views are derived
/// from it rather than maintaining separate cache lifecycles.
#[derive(Debug, Clone)]
pub(crate) struct VerifiedPackage {
    package: String,
    version: String,
    checksum: String,
    archive: PathBuf,
    extracted: PathBuf,
}

impl VerifiedPackage {
    pub(crate) fn manifest(&self) -> Result<String> {
        let bytes = read_cached_archive(&self.archive)?;
        let source = read_manifest_from_crate(&bytes, &self.package, &self.version)?;
        verify_manifest_identity(&source, &self.package, &self.version)?;
        Ok(source)
    }

    /// Return an atomically extracted, path-safe package root for the manifest
    /// compiler. The archive checksum was verified before this view exists.
    pub(crate) fn extracted_root(&self) -> Result<PathBuf> {
        if self.extraction_is_complete()? {
            return Ok(self.extracted.clone());
        }
        let parent = self
            .extracted
            .parent()
            .context("package extraction path has no parent")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create package cache {}", parent.display()))?;
        let mut temporary = tempfile::Builder::new()
            .prefix("extract-")
            .tempdir_in(parent)
            .with_context(|| {
                format!(
                    "failed to create extraction directory in {}",
                    parent.display()
                )
            })?;
        extract_checked_archive(
            &self.archive,
            temporary.path(),
            &self.package,
            &self.version,
        )?;
        fs::write(temporary.path().join(EXTRACTION_MARKER), &self.checksum)
            .context("failed to mark verified package extraction")?;
        // A concurrent resolver may have won the race with an identical
        // immutable archive. Its complete root is as good as ours.
        if self.extraction_is_complete()? {
            return Ok(self.extracted.clone());
        }
        ensure!(
            !self.extracted.exists(),
            "cached package extraction {} is incomplete or corrupt; remove that extraction directory and rerun the same command",
            self.extracted.display(),
        );
        match fs::rename(temporary.path(), &self.extracted) {
            Ok(()) => {
                temporary.disable_cleanup(true);
                Ok(self.extracted.clone())
            }
            Err(_) if self.extraction_is_complete()? => Ok(self.extracted.clone()),
            Err(error) if self.extracted.exists() => Err(error).with_context(|| {
                format!(
                    "cached package extraction {} is incomplete or corrupt; remove that extraction directory and rerun the same command",
                    self.extracted.display()
                )
            }),
            Err(error) => Err(error).with_context(|| {
                format!(
                    "failed to publish package extraction {}",
                    self.extracted.display()
                )
            }),
        }
    }

    fn extraction_is_complete(&self) -> Result<bool> {
        if !self.extracted.is_dir() {
            return Ok(false);
        }
        let marker = fs::read_to_string(self.extracted.join(EXTRACTION_MARKER)).ok();
        if marker.as_deref().map(str::trim) != Some(self.checksum.as_str()) {
            return Ok(false);
        }
        Ok(
            fs::read_to_string(self.extracted.join("Cargo.toml")).is_ok_and(|source| {
                verify_manifest_identity(&source, &self.package, &self.version).is_ok()
            }),
        )
    }

    /// Registry component definitions ride in the package that owns their
    /// driver. A future passive catalog needs a real distribution contract;
    /// accepting a binless crate here would silently revive the old anchor
    /// convention. Current packages may still have a library target during the
    /// capability rollout, but must declare exactly one driver bin. Returns
    /// its declared conventional source path for validation after extraction.
    pub(crate) fn require_component_driver_bin(&self) -> Result<PathBuf> {
        let manifest: toml::Value = toml::from_str(&self.manifest()?).with_context(|| {
            format!(
                "published manifest for {}@{} is invalid TOML",
                self.package, self.version
            )
        })?;
        let bins = manifest
            .get("bin")
            .and_then(toml::Value::as_array)
            .map(|bins| bins.iter().collect::<Vec<_>>())
            .unwrap_or_default();
        ensure!(
            bins.len() == 1
                && bins[0].get("name").and_then(toml::Value::as_str) == Some(&self.package),
            "registry component package {}@{} must declare exactly one driver bin named {}; asset-only registry components are unsupported",
            self.package,
            self.version,
            self.package,
        );
        let source = bins[0]
            .get("path")
            .and_then(toml::Value::as_str)
            .unwrap_or("src/main.rs");
        let source = PathBuf::from(source);
        ensure!(
            source == Path::new("src/main.rs"),
            "registry component package {}@{} must use the conventional driver source src/main.rs",
            self.package,
            self.version,
        );
        Ok(source)
    }

    pub(crate) fn require_component_driver_source(&self, root: &Path, source: &Path) -> Result<()> {
        ensure!(
            root.join(source).is_file(),
            "registry component package {}@{} declares driver source {} but the verified archive does not contain it",
            self.package,
            self.version,
            source.display(),
        );
        Ok(())
    }
}

/// Fetch, verify, and cache an exact sparse-registry package. Online calls
/// always compare cache bytes to the current index checksum; offline calls
/// verify bytes against their checksum-keyed filename and never use HTTP.
pub(crate) fn fetch_registry_package(
    http: &dyn RegistryHttp,
    cache: &PackageCache,
    package: &str,
    version: &str,
    offline: bool,
) -> Result<VerifiedPackage> {
    let package_dir = cache.package_dir(package, version)?;
    if offline {
        return offline_cached_package(package, version, &package_dir);
    }

    let base = phoxal_cli_catalog::REGISTRY_INDEX
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
    ensure!(
        expected_checksum.len() == 64
            && expected_checksum
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit()),
        "registry index {index_url} has an invalid checksum for {package}@{version}"
    );
    let archive = package_dir.join(format!("{expected_checksum}.crate"));
    if archive.is_file() && archive_checksum_matches(&archive, &expected_checksum).unwrap_or(false)
    {
        prune_obsolete_checksum_archives(&package_dir, &expected_checksum);
        return Ok(VerifiedPackage {
            package: package.to_owned(),
            version: version.to_owned(),
            extracted: package_dir.join(format!("{expected_checksum}.root")),
            archive,
            checksum: expected_checksum,
        });
    }
    let crate_url = download_url(&config.dl, package, version)?;
    let crate_bytes = http.get(&crate_url)?;
    let actual_checksum = hex::encode(Sha256::digest(&crate_bytes));
    ensure!(
        actual_checksum == expected_checksum,
        "checksum mismatch for {package}@{version} from {crate_url}: expected {expected_checksum}, got {actual_checksum}"
    );
    let source = read_manifest_from_crate(&crate_bytes, package, version)?;
    verify_manifest_identity(&source, package, version)?;
    write_atomic(&archive, &crate_bytes)?;
    prune_obsolete_checksum_archives(&package_dir, &expected_checksum);
    Ok(VerifiedPackage {
        package: package.to_owned(),
        version: version.to_owned(),
        extracted: package_dir.join(format!("{expected_checksum}.root")),
        archive,
        checksum: expected_checksum,
    })
}

/// An online index establishes the current immutable archive checksum. This
/// best-effort janitor touches only obsolete archive siblings: extraction
/// roots may be in active use by another unlocked command and are left for a
/// future explicit cache-cleanup lifecycle.
fn prune_obsolete_checksum_archives(package_dir: &Path, current_checksum: &str) {
    let Ok(entries) = fs::read_dir(package_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let is_checksum = stem.len() == 64 && stem.bytes().all(|byte| byte.is_ascii_hexdigit());
        if !is_checksum
            || stem == current_checksum
            || path
                .extension()
                .is_none_or(|extension| extension != "crate")
        {
            continue;
        }
        let _ = fs::remove_file(path);
    }
}

fn archive_checksum_matches(path: &Path, expected: &str) -> Result<bool> {
    let bytes = read_cached_archive(path)?;
    Ok(hex::encode(Sha256::digest(bytes)) == expected)
}

fn offline_cached_package(
    package: &str,
    version: &str,
    package_dir: &Path,
) -> Result<VerifiedPackage> {
    let entries = match fs::read_dir(package_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!(
                "offline resolution needs one verified cached package for {package}@{version} under {}; rerun the same phoxal command without `--offline` once to populate it",
                package_dir.display()
            );
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to inspect offline package cache for {package}@{version} under {}",
                    package_dir.display()
                )
            });
        }
    };
    let entries = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "crate")
        })
        .filter_map(|archive| {
            let checksum = archive.file_stem()?.to_str()?.to_owned();
            (checksum.len() == 64 && checksum.bytes().all(|byte| byte.is_ascii_hexdigit()))
                .then_some((archive, checksum))
        })
        .collect::<Vec<_>>();
    if entries.is_empty() {
        bail!(
            "offline resolution needs one verified cached package for {package}@{version} under {}; rerun the same phoxal command without `--offline` once to populate it",
            package_dir.display()
        );
    }
    ensure!(
        entries.len() == 1,
        "offline package cache for {package}@{version} is ambiguous: found {} checksum-named archives under {}",
        entries.len(),
        package_dir.display()
    );
    let (archive, checksum) = entries
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("verified package cache entry disappeared"))?;
    let checksum_matches = archive_checksum_matches(&archive, &checksum).with_context(|| {
        format!(
            "cached package {package}@{version} at {} is corrupt",
            archive.display()
        )
    })?;
    ensure!(
        checksum_matches,
        "cached package {package}@{version} at {} is corrupt: checksum mismatch",
        archive.display()
    );
    let verified = VerifiedPackage {
        package: package.to_owned(),
        version: version.to_owned(),
        extracted: package_dir.join(format!("{checksum}.root")),
        archive,
        checksum,
    };
    verified.manifest().with_context(|| {
        format!("offline cached package {package}@{version} failed identity verification")
    })?;
    Ok(verified)
}

fn extract_checked_archive(
    archive_path: &Path,
    destination: &Path,
    package: &str,
    version: &str,
) -> Result<()> {
    extract_checked_archive_with_limits(
        archive_path,
        destination,
        package,
        version,
        MAX_ARCHIVE_ENTRIES,
        MAX_EXPANDED_BYTES,
    )
}

fn extract_checked_archive_with_limits(
    archive_path: &Path,
    destination: &Path,
    package: &str,
    version: &str,
    max_entries: usize,
    max_expanded_bytes: u64,
) -> Result<()> {
    let archive_file = fs::File::open(archive_path)
        .with_context(|| format!("failed to open {}", archive_path.display()))?;
    let expected_root = format!("{package}-{version}");
    let mut archive = tar::Archive::new(GzDecoder::new(archive_file));
    let mut entries = 0usize;
    let mut expanded = 0u64;
    let mut saw_manifest = false;
    for entry in archive
        .entries()
        .context("failed to read cached .crate archive")?
    {
        entries += 1;
        ensure!(
            entries <= max_entries,
            "package {package}@{version} exceeds the {max_entries}-entry archive limit"
        );
        let mut entry = entry.context("failed to read cached .crate entry")?;
        let path = entry
            .path()
            .context("cached .crate has an invalid path")?
            .into_owned();
        let relative =
            archive_relative(&path, &expected_root, entry.header().entry_type().is_dir())?;
        let entry_type = entry.header().entry_type();
        let target = destination.join(&relative);
        ensure!(
            supported_archive_entry_type(entry_type),
            "package {package}@{version} contains unsupported archive entry {}",
            path.display()
        );
        if entry_type.is_dir() {
            fs::create_dir_all(&target)
                .with_context(|| format!("failed to create {}", target.display()))?;
        } else if entry_type.is_file() {
            expanded = expanded.saturating_add(entry.size());
            ensure!(
                expanded <= max_expanded_bytes,
                "package {package}@{version} exceeds the {max_expanded_bytes}-byte expanded limit"
            );
            let parent = target.parent().context("archive file has no parent")?;
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
            let mut output = fs::File::create(&target)
                .with_context(|| format!("failed to create {}", target.display()))?;
            std::io::copy(&mut entry, &mut output)
                .with_context(|| format!("failed to extract {}", path.display()))?;
            if relative == Path::new("Cargo.toml") {
                saw_manifest = true;
            }
        }
    }
    ensure!(
        saw_manifest,
        "package {package}@{version} does not contain {expected_root}/Cargo.toml"
    );
    let manifest = fs::read_to_string(destination.join("Cargo.toml"))?;
    verify_manifest_identity(&manifest, package, version)
}

fn supported_archive_entry_type(entry_type: tar::EntryType) -> bool {
    entry_type.is_dir() || entry_type.is_file()
}

fn archive_relative(path: &Path, expected_root: &str, is_directory: bool) -> Result<PathBuf> {
    let mut components = path.components();
    ensure!(
        matches!(components.next(), Some(Component::Normal(root)) if root == std::ffi::OsStr::new(expected_root)),
        "package archive has an entry outside its {expected_root}/ root: {}",
        path.display()
    );
    let relative = components.collect::<PathBuf>();
    ensure!(
        !relative.as_os_str().is_empty() || is_directory,
        "package archive has an invalid archive root entry"
    );
    ensure!(
        relative
            .components()
            .all(|component| matches!(component, Component::Normal(_))),
        "package archive has an unsafe path {}",
        path.display()
    );
    Ok(relative)
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

    fn crate_bytes_with_files(
        package: &str,
        version: &str,
        manifest: &str,
        files: &[(&str, &[u8])],
    ) -> Vec<u8> {
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut archive = tar::Builder::new(encoder);
        for (path, contents) in
            std::iter::once(("Cargo.toml", manifest.as_bytes())).chain(files.iter().copied())
        {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive
                .append_data(
                    &mut header,
                    format!("{package}-{version}/{path}"),
                    std::io::Cursor::new(contents),
                )
                .unwrap();
        }
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
        let manifest =
            format!("[package]\nname = {archived_package:?}\nversion = {archived_version:?}\n");
        let bytes = crate_bytes(package, version, &manifest);
        fixture_for_archive(package, version, bytes, corrupt_checksum, include_version)
    }

    fn fixture_for_archive(
        package: &str,
        version: &str,
        bytes: Vec<u8>,
        corrupt_checksum: bool,
        include_version: bool,
    ) -> FakeHttp {
        let base = "https://phoxal.github.io/registry";
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
    fn a_verified_package_is_fetched_and_cached() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let cache = PackageCache::new(temp.path().to_path_buf());
        let http = fixture(
            "phoxal-tool-joypad",
            "1.2.3",
            "phoxal-tool-joypad",
            "1.2.3",
            false,
            true,
        );
        let source = fetch_registry_package(&http, &cache, "phoxal-tool-joypad", "1.2.3", false)?
            .manifest()?;
        assert!(source.contains("phoxal-tool-joypad"));
        assert!(temp.path().join("phoxal-tool-joypad/1.2.3").is_dir());
        assert_eq!(http.calls.load(Ordering::SeqCst), 3);
        Ok(())
    }

    #[test]
    fn a_checksum_mismatch_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let cache = PackageCache::new(temp.path().to_path_buf());
        let http = fixture("pkg", "1.2.3", "pkg", "1.2.3", true, true);
        assert!(
            fetch_registry_package(&http, &cache, "pkg", "1.2.3", false)
                .unwrap_err()
                .to_string()
                .contains("checksum mismatch")
        );
    }

    #[test]
    fn a_missing_exact_version_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let cache = PackageCache::new(temp.path().to_path_buf());
        let http = fixture("pkg", "1.2.3", "pkg", "1.2.3", false, false);
        assert!(
            fetch_registry_package(&http, &cache, "pkg", "1.2.3", false)
                .unwrap_err()
                .to_string()
                .contains("no exact pkg@1.2.3")
        );
    }

    #[test]
    fn a_mismatched_archived_identity_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let cache = PackageCache::new(temp.path().to_path_buf());
        let http = fixture("pkg", "1.2.3", "other", "1.2.3", false, true);
        assert!(
            fetch_registry_package(&http, &cache, "pkg", "1.2.3", false)
                .unwrap_err()
                .to_string()
                .contains("identity mismatch")
        );
    }

    #[test]
    fn offline_with_a_cold_cache_fails_actionably() {
        let temp = tempfile::tempdir().unwrap();
        let cache = PackageCache::new(temp.path().to_path_buf());
        let http = FakeHttp {
            responses: BTreeMap::new(),
            calls: AtomicUsize::new(0),
            panic_on_call: true,
        };
        let error = fetch_registry_package(&http, &cache, "pkg", "1.2.3", true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("without `--offline`"), "{error}");
    }

    #[test]
    fn warm_offline_cache_never_uses_http_and_rejects_corruption() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let cache = PackageCache::new(temp.path().to_path_buf());
        let online = fixture("pkg", "1.2.3", "pkg", "1.2.3", false, true);
        let verified = fetch_registry_package(&online, &cache, "pkg", "1.2.3", false)?;
        let offline = FakeHttp {
            responses: BTreeMap::new(),
            calls: AtomicUsize::new(0),
            panic_on_call: true,
        };
        assert!(
            fetch_registry_package(&offline, &cache, "pkg", "1.2.3", true)?
                .manifest()?
                .contains("name = \"pkg\"")
        );
        assert_eq!(offline.calls.load(Ordering::SeqCst), 0);
        fs::write(&verified.archive, b"corrupt")?;
        let error = fetch_registry_package(&offline, &cache, "pkg", "1.2.3", true)
            .expect_err("a warm corrupt archive must be named as corruption");
        assert!(
            error.to_string().contains("corrupt: checksum mismatch"),
            "{error:#}"
        );
        assert_eq!(offline.calls.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[test]
    fn online_replaces_a_corrupt_cache_entry() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let cache = PackageCache::new(temp.path().to_path_buf());
        let http = fixture("pkg", "1.2.3", "pkg", "1.2.3", false, true);
        let verified = fetch_registry_package(&http, &cache, "pkg", "1.2.3", false)?;
        fs::write(&verified.archive, b"corrupt")?;
        let repaired = fetch_registry_package(&http, &cache, "pkg", "1.2.3", false)?;
        assert!(repaired.manifest()?.contains("name = \"pkg\""));
        Ok(())
    }

    #[test]
    fn online_best_effort_prunes_only_obsolete_checksum_archives() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let cache = PackageCache::new(temp.path().to_path_buf());
        let http = fixture("pkg", "1.2.3", "pkg", "1.2.3", false, true);
        let verified = fetch_registry_package(&http, &cache, "pkg", "1.2.3", false)?;
        let stale = "ab".repeat(32);
        let stale_archive = verified
            .archive
            .parent()
            .unwrap()
            .join(format!("{stale}.crate"));
        let stale_root = verified
            .extracted
            .parent()
            .unwrap()
            .join(format!("{stale}.root"));
        fs::write(&stale_archive, b"stale")?;
        fs::create_dir_all(&stale_root)?;
        fs::write(stale_root.join("old"), b"stale")?;

        let warm = fetch_registry_package(&http, &cache, "pkg", "1.2.3", false)?;
        assert_eq!(warm.archive, verified.archive);
        assert!(verified.archive.is_file());
        assert!(!stale_archive.exists());
        assert!(
            stale_root.exists(),
            "an unlocked command may still be reading it"
        );
        Ok(())
    }

    #[test]
    fn online_replaces_an_oversized_sparse_cache_entry_without_reading_it() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let cache = PackageCache::new(temp.path().to_path_buf());
        let http = fixture("pkg", "1.2.3", "pkg", "1.2.3", false, true);
        let verified = fetch_registry_package(&http, &cache, "pkg", "1.2.3", false)?;
        fs::OpenOptions::new()
            .write(true)
            .open(&verified.archive)?
            .set_len(MAX_HTTP_BYTES + 1)?;
        let repaired = fetch_registry_package(&http, &cache, "pkg", "1.2.3", false)?;
        assert!(repaired.manifest()?.contains("name = \"pkg\""));
        Ok(())
    }

    #[test]
    fn offline_rejects_a_checksum_named_archive_with_the_wrong_identity() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let cache = PackageCache::new(temp.path().to_path_buf());
        let bytes = crate_bytes(
            "pkg",
            "1.2.3",
            "[package]\nname = \"other\"\nversion = \"1.2.3\"\n",
        );
        let checksum = hex::encode(Sha256::digest(&bytes));
        let path = temp.path().join("pkg/1.2.3");
        fs::create_dir_all(&path)?;
        fs::write(path.join(format!("{checksum}.crate")), bytes)?;
        let no_http = FakeHttp {
            responses: BTreeMap::new(),
            calls: AtomicUsize::new(0),
            panic_on_call: true,
        };
        let error = fetch_registry_package(&no_http, &cache, "pkg", "1.2.3", true)
            .expect_err("identity mismatch must not become a cache hit");
        assert!(error.to_string().contains("identity"), "{error:#}");
        Ok(())
    }

    #[test]
    fn component_registry_package_requires_its_exact_driver_bin() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let cache = PackageCache::new(temp.path().to_path_buf());
        let package = "phoxal-component-wheel";
        let anchored = crate_bytes_with_files(
            package,
            "1.2.3",
            &format!(
                "[package]\nname = {package:?}\nversion = \"1.2.3\"\n\n[lib]\npath = \"src/lib.rs\"\n\n[[bin]]\nname = {package:?}\npath = \"src/main.rs\"\n"
            ),
            &[("src/main.rs", b"fn main() {}")],
        );
        let good = fixture_for_archive(package, "1.2.3", anchored, false, true);
        let verified = fetch_registry_package(&good, &cache, package, "1.2.3", false)?;
        let source = verified.require_component_driver_bin()?;
        verified.require_component_driver_source(&verified.extracted_root()?, &source)?;

        let no_source = crate_bytes(
            package,
            "1.2.31",
            &format!(
                "[package]\nname = {package:?}\nversion = \"1.2.31\"\n\n[[bin]]\nname = {package:?}\npath = \"src/main.rs\"\n"
            ),
        );
        let no_source_http = fixture_for_archive(package, "1.2.31", no_source, false, true);
        let no_source = fetch_registry_package(&no_source_http, &cache, package, "1.2.31", false)?;
        let source = no_source.require_component_driver_bin()?;
        let error = no_source
            .require_component_driver_source(&no_source.extracted_root()?, &source)
            .expect_err("declared driver source must be packaged");
        assert!(error.to_string().contains("does not contain"), "{error:#}");

        let nonconventional = crate_bytes_with_files(
            package,
            "1.2.32",
            &format!(
                "[package]\nname = {package:?}\nversion = \"1.2.32\"\n\n[[bin]]\nname = {package:?}\npath = \"src/driver.rs\"\n"
            ),
            &[("src/driver.rs", b"fn main() {}")],
        );
        let nonconventional_http =
            fixture_for_archive(package, "1.2.32", nonconventional, false, true);
        let error =
            fetch_registry_package(&nonconventional_http, &cache, package, "1.2.32", false)?
                .require_component_driver_bin()
                .expect_err("registry drivers must keep the conventional source path");
        assert!(
            error.to_string().contains("conventional driver source"),
            "{error:#}"
        );

        let missing = crate_bytes(
            package,
            "1.2.4",
            &format!("[package]\nname = {package:?}\nversion = \"1.2.4\"\n"),
        );
        let missing_http = fixture_for_archive(package, "1.2.4", missing, false, true);
        assert!(
            fetch_registry_package(&missing_http, &cache, package, "1.2.4", false)?
                .require_component_driver_bin()
                .is_err()
        );

        let wrong = crate_bytes(
            package,
            "1.2.5",
            &format!(
                "[package]\nname = {package:?}\nversion = \"1.2.5\"\n\n[[bin]]\nname = \"another\"\npath = \"src/main.rs\"\n"
            ),
        );
        let wrong_http = fixture_for_archive(package, "1.2.5", wrong, false, true);
        assert!(
            fetch_registry_package(&wrong_http, &cache, package, "1.2.5", false)?
                .require_component_driver_bin()
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn concurrent_extraction_publishes_one_complete_verified_root() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let cache = PackageCache::new(temp.path().to_path_buf());
        let http = fixture("pkg", "1.2.3", "pkg", "1.2.3", false, true);
        let package = fetch_registry_package(&http, &cache, "pkg", "1.2.3", false)?;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let left = package.clone();
        let right = package.clone();
        let left_barrier = barrier.clone();
        let right_barrier = barrier.clone();
        let (left, right) = std::thread::scope(|scope| -> Result<_> {
            let left = scope.spawn(move || {
                left_barrier.wait();
                left.extracted_root()
            });
            let right = scope.spawn(move || {
                right_barrier.wait();
                right.extracted_root()
            });
            Ok((
                left.join().expect("left extraction thread panicked")?,
                right.join().expect("right extraction thread panicked")?,
            ))
        })?;
        assert_eq!(left, right);
        assert!(left.join(EXTRACTION_MARKER).is_file());
        assert!(left.join("Cargo.toml").is_file());
        Ok(())
    }

    #[test]
    fn extraction_never_deletes_a_preexisting_incomplete_root() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let cache = PackageCache::new(temp.path().to_path_buf());
        let http = fixture("pkg", "1.2.3", "pkg", "1.2.3", false, true);
        let package = fetch_registry_package(&http, &cache, "pkg", "1.2.3", false)?;
        fs::create_dir_all(&package.extracted)?;
        fs::write(package.extracted.join("interrupted"), "keep")?;
        let error = package
            .extracted_root()
            .expect_err("incomplete final cache roots must be reported, never replaced");
        assert!(
            error.to_string().contains("incomplete or corrupt"),
            "{error:#}"
        );
        assert!(package.extracted.join("interrupted").is_file());
        Ok(())
    }

    #[test]
    fn extraction_refuses_wrong_roots_missing_manifests_and_links() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let archive = temp.path().join("package.crate");
        fs::write(
            &archive,
            crate_bytes(
                "other",
                "1.2.3",
                "[package]\nname = \"other\"\nversion = \"1.2.3\"\n",
            ),
        )?;
        assert!(
            extract_checked_archive(&archive, &temp.path().join("wrong"), "pkg", "1.2.3").is_err()
        );

        let empty = GzEncoder::new(Vec::new(), Compression::default());
        let archive_writer = tar::Builder::new(empty);
        fs::write(&archive, archive_writer.into_inner()?.finish()?)?;
        assert!(
            extract_checked_archive(&archive, &temp.path().join("missing"), "pkg", "1.2.3")
                .is_err()
        );

        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut writer = tar::Builder::new(encoder);
        let manifest = "[package]\nname = \"pkg\"\nversion = \"1.2.3\"\n";
        let mut manifest_header = tar::Header::new_gnu();
        manifest_header.set_size(manifest.len() as u64);
        manifest_header.set_mode(0o644);
        manifest_header.set_cksum();
        writer.append_data(
            &mut manifest_header,
            "pkg-1.2.3/Cargo.toml",
            manifest.as_bytes(),
        )?;
        let mut link_header = tar::Header::new_gnu();
        link_header.set_entry_type(tar::EntryType::symlink());
        link_header.set_size(0);
        link_header.set_mode(0o777);
        link_header.set_cksum();
        writer.append_link(&mut link_header, "pkg-1.2.3/escape", "../../outside")?;
        fs::write(&archive, writer.into_inner()?.finish()?)?;
        assert!(
            extract_checked_archive(&archive, &temp.path().join("link"), "pkg", "1.2.3").is_err()
        );
        Ok(())
    }

    #[test]
    fn archive_path_and_type_guards_cover_all_unsafe_forms() {
        for path in [
            Path::new("other-1.2.3/Cargo.toml"),
            Path::new("/pkg-1.2.3/Cargo.toml"),
            Path::new("pkg-1.2.3/../escape"),
        ] {
            assert!(
                archive_relative(path, "pkg-1.2.3", false).is_err(),
                "{path:?}"
            );
        }
        assert!(archive_relative(Path::new("pkg-1.2.3/Cargo.toml"), "pkg-1.2.3", false).is_ok());
        for entry_type in [
            tar::EntryType::symlink(),
            tar::EntryType::hard_link(),
            tar::EntryType::Char,
            tar::EntryType::Block,
            tar::EntryType::Fifo,
        ] {
            assert!(!supported_archive_entry_type(entry_type));
        }
    }

    #[test]
    fn extraction_enforces_entry_and_expanded_byte_limits_without_publishing() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let archive = temp.path().join("package.crate");
        let manifest = "[package]\nname = \"pkg\"\nversion = \"1.2.3\"\n";
        fs::write(
            &archive,
            crate_bytes_with_files("pkg", "1.2.3", manifest, &[("asset", b"12")]),
        )?;
        assert!(
            extract_checked_archive_with_limits(
                &archive,
                &temp.path().join("too-many"),
                "pkg",
                "1.2.3",
                1,
                10,
            )
            .is_err()
        );
        assert!(
            extract_checked_archive_with_limits(
                &archive,
                &temp.path().join("too-large"),
                "pkg",
                "1.2.3",
                10,
                1,
            )
            .is_err()
        );

        let empty = GzEncoder::new(Vec::new(), Compression::default());
        fs::write(&archive, tar::Builder::new(empty).into_inner()?.finish()?)?;

        let broken = VerifiedPackage {
            package: "pkg".to_owned(),
            version: "1.2.3".to_owned(),
            checksum: hex::encode(Sha256::digest(read_cached_archive(&archive)?)),
            archive,
            extracted: temp.path().join("never-published.root"),
        };
        assert!(broken.extracted_root().is_err());
        assert!(!broken.extracted.exists());
        Ok(())
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
