//! Download and atomically replace the single `phoxal` application binary.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::StatusCode;
use reqwest::blocking::Client;
use semver::Version;
use serde::Serialize;

use crate::cli::context::AppContext;
use crate::cli::output::plain::Ui;

const LATEST_RELEASE_URL: &str = "https://github.com/phoxal/phoxal-cli/releases/latest";
const DOWNLOAD_BASE_URL: &str = "https://github.com/phoxal/phoxal-cli/releases/download";
const USER_AGENT: &str = "phoxal-cli";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
struct UpgradeOptions {
    requested_version: Option<Version>,
    force: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpgradeAction {
    UpToDate,
    Upgraded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpgradeOutcome {
    pub version_from: String,
    pub version_to: String,
    pub source: String,
    pub upgraded: bool,
    pub action: UpgradeAction,
}

pub(crate) fn parse_version_arg(raw: &str) -> std::result::Result<Version, String> {
    Version::parse(normalize_version_tag(raw))
        .map_err(|error| format!("invalid version '{raw}': {error}"))
}

fn run_upgrade(options: UpgradeOptions, ui: Ui) -> Result<UpgradeOutcome> {
    let target = target_triple()?;
    let current_version =
        Version::parse(env!("CARGO_PKG_VERSION")).context("invalid CARGO_PKG_VERSION")?;
    let source = if options.requested_version.is_some() {
        "pinned tag".to_string()
    } else {
        "latest release".to_string()
    };
    let client = build_client(false)?;
    let requested_version = match options.requested_version {
        Some(version) => version,
        None => {
            ui.info("resolving latest phoxal-cli release");
            discover_latest_version(&client)?
        }
    };

    anyhow::ensure!(
        requested_version >= current_version,
        "refusing to replace phoxal-cli v{current_version} with older release v{requested_version}"
    );
    if requested_version == current_version && !options.force {
        println!("already up to date (v{requested_version})");
        return Ok(UpgradeOutcome {
            version_from: current_version.to_string(),
            version_to: requested_version.to_string(),
            source,
            upgraded: false,
            action: UpgradeAction::UpToDate,
        });
    }

    let asset = ReleaseAsset::new(&requested_version, target);
    let temp = tempfile::tempdir().context("failed to create self-upgrade temp directory")?;
    let archive = temp.path().join(&asset.archive_name);
    let checksum = temp.path().join(format!("{}.sha256", asset.archive_name));
    let download_client = build_client(true)?;
    ui.info(format!("downloading {}", asset.archive_url));
    require_asset(
        &download_client,
        &asset.archive_url,
        &archive,
        "release archive",
    )?;
    require_asset(
        &download_client,
        &format!("{}.sha256", asset.archive_url),
        &checksum,
        "release checksum",
    )?;
    verify_published_archive(&archive, &checksum, &asset.archive_name)?;
    let staged = extract_binary(&archive, &asset, temp.path())?;
    let current_exe = current_executable()?;
    refuse_managed_install(&current_exe)?;
    replace_current_executable(&staged)?;

    ui.success(format!(
        "upgraded phoxal v{current_version} -> v{requested_version}"
    ));
    Ok(UpgradeOutcome {
        version_from: current_version.to_string(),
        version_to: requested_version.to_string(),
        source,
        upgraded: true,
        action: UpgradeAction::Upgraded,
    })
}

fn normalize_version_tag(raw: &str) -> &str {
    let trimmed = raw.trim();
    trimmed
        .strip_prefix('v')
        .or_else(|| trimmed.strip_prefix('V'))
        .unwrap_or(trimmed)
}

fn target_triple() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        (os, arch) => bail!(
            "unsupported target {os}-{arch}; self upgrade is available for macos-aarch64, linux-x86_64, and linux-aarch64"
        ),
    }
}

fn build_client(authenticated: bool) -> Result<Client> {
    let mut builder = Client::builder()
        .user_agent(USER_AGENT)
        .timeout(REQUEST_TIMEOUT);
    if !authenticated {
        builder = builder.redirect(reqwest::redirect::Policy::none());
    }
    builder.build().context("failed to build HTTP client")
}

fn discover_latest_version(client: &Client) -> Result<Version> {
    let response = client
        .get(LATEST_RELEASE_URL)
        .send()
        .context("failed to resolve latest phoxal-cli release")?;
    let status = response.status();
    if !status.is_redirection() {
        bail!("latest release lookup returned {status} instead of a redirect");
    }
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .context("latest release redirect did not include a Location header")?
        .to_str()
        .context("latest release Location header was not valid UTF-8")?;
    let tag = location
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .context("latest release Location header did not include a tag")?;
    Version::parse(normalize_version_tag(tag))
        .with_context(|| format!("latest release redirect pointed to invalid tag '{tag}'"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AssetDownload {
    Written,
    Absent,
}

fn download_asset(client: &Client, url: &str, destination: &Path) -> Result<AssetDownload> {
    let mut response = client
        .get(url)
        .send()
        .with_context(|| format!("failed to download {url}"))?;
    if response.status() == StatusCode::NOT_FOUND {
        return Ok(AssetDownload::Absent);
    }
    if !response.status().is_success() {
        bail!("download {url} returned {}", response.status());
    }
    let mut file = fs::File::create(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    response
        .copy_to(&mut file)
        .with_context(|| format!("failed to write {}", destination.display()))?;
    Ok(AssetDownload::Written)
}

fn require_asset(client: &Client, url: &str, destination: &Path, kind: &str) -> Result<()> {
    if download_asset(client, url, destination)? == AssetDownload::Absent {
        bail!("{kind} {url} was not found");
    }
    Ok(())
}

fn verify_published_archive(archive: &Path, checksum: &Path, name: &str) -> Result<()> {
    use sha2::{Digest, Sha256};

    let bytes = fs::read(archive)?;
    let published = fs::read_to_string(checksum)?;
    let published = published
        .split_whitespace()
        .next()
        .with_context(|| format!("the checksum published for {name} is empty"))?;
    let digest = hex::encode(Sha256::digest(&bytes));
    anyhow::ensure!(
        digest == published,
        "the published archive {name} has sha256 {digest} but its checksum records {published}"
    );
    Ok(())
}

struct ReleaseAsset {
    archive_name: String,
    binary_name: String,
    archive_url: String,
}

impl ReleaseAsset {
    fn new(version: &Version, target: &str) -> Self {
        let archive_name = format!("phoxal-{version}-{target}.tar.gz");
        Self {
            binary_name: format!("phoxal-{target}"),
            archive_url: format!("{DOWNLOAD_BASE_URL}/v{version}/{archive_name}"),
            archive_name,
        }
    }
}

fn extract_binary(archive_path: &Path, asset: &ReleaseAsset, temp_root: &Path) -> Result<PathBuf> {
    let archive_file = fs::File::open(archive_path)?;
    let decoder = flate2::read::GzDecoder::new(archive_file);
    let mut archive = tar::Archive::new(decoder);
    let destination = temp_root.join("phoxal");
    let mut found = false;
    for entry in archive.entries().context("failed to read tar archive")? {
        let mut entry = entry.context("failed to read tar entry")?;
        let path = entry.path()?;
        anyhow::ensure!(
            path.as_ref() == Path::new(&asset.binary_name),
            "release archive contains unexpected entry {}; expected only {}",
            path.display(),
            asset.binary_name
        );
        anyhow::ensure!(
            entry.header().entry_type().is_file(),
            "release archive entry {} is not a regular file",
            asset.binary_name
        );
        anyhow::ensure!(
            !found,
            "release archive contains duplicate {}",
            asset.binary_name
        );
        entry.unpack(&destination)?;
        make_executable(&destination)?;
        found = true;
    }
    anyhow::ensure!(
        found,
        "release archive {} is missing {}",
        asset.archive_name,
        asset.binary_name
    );
    Ok(destination)
}

fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

fn current_executable() -> Result<PathBuf> {
    let path = std::env::current_exe().context("failed to locate current executable")?;
    path.canonicalize()
        .with_context(|| format!("failed to canonicalize {}", path.display()))
}

fn refuse_managed_install(path: &Path) -> Result<()> {
    let display = path.display();
    let path = path.to_string_lossy();
    if path.contains("/.cargo/bin/") {
        bail!(
            "refusing to self-upgrade cargo-installed phoxal at {display}; reinstall with `cargo install --git https://github.com/phoxal/phoxal-cli`"
        );
    }
    if path.contains("/Cellar/") || path.contains("/homebrew/") {
        bail!(
            "refusing to self-upgrade Homebrew-managed phoxal at {display}; use brew to upgrade or reinstall it"
        );
    }
    Ok(())
}

fn replace_current_executable(new_binary: &Path) -> Result<()> {
    self_replace::self_replace(new_binary).map_err(|error| {
        if error.kind() == std::io::ErrorKind::PermissionDenied {
            anyhow!("permission denied replacing the current executable; re-run with appropriate permissions or reinstall it")
        } else {
            anyhow!(error).context("failed to replace current executable")
        }
    })
}

pub(crate) async fn upgrade(
    app: &AppContext,
    requested_version: Option<Version>,
    force: bool,
) -> Result<()> {
    let ui = app.ui;
    tokio::task::spawn_blocking(move || {
        run_upgrade(
            UpgradeOptions {
                requested_version,
                force,
            },
            ui,
        )
    })
    .await
    .context("self upgrade worker failed")??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive_with(root: &Path, name: &str, entries: &[(&str, &str)]) -> PathBuf {
        let path = root.join(name);
        let encoder = flate2::write::GzEncoder::new(
            fs::File::create(&path).unwrap(),
            flate2::Compression::fast(),
        );
        let mut builder = tar::Builder::new(encoder);
        for (entry_name, contents) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, entry_name, contents.as_bytes())
                .unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
        path
    }

    #[test]
    fn release_asset_is_target_and_version_specific() {
        let asset = ReleaseAsset::new(&Version::parse("9.9.9").unwrap(), "aarch64-linux-gnu");
        assert_eq!(asset.archive_name, "phoxal-9.9.9-aarch64-linux-gnu.tar.gz");
        assert_eq!(asset.binary_name, "phoxal-aarch64-linux-gnu");
        assert!(
            asset
                .archive_url
                .ends_with("/v9.9.9/phoxal-9.9.9-aarch64-linux-gnu.tar.gz")
        );
    }

    #[test]
    fn archive_requires_exactly_one_phoxal_binary() {
        let temp = tempfile::tempdir().unwrap();
        let asset = ReleaseAsset::new(&Version::parse("1.2.3").unwrap(), "target");
        let valid = archive_with(temp.path(), "valid.tgz", &[("phoxal-target", "bytes")]);
        let extracted = extract_binary(&valid, &asset, temp.path()).unwrap();
        assert_eq!(fs::read_to_string(extracted).unwrap(), "bytes");

        let missing = archive_with(temp.path(), "missing.tgz", &[("other", "bytes")]);
        assert!(extract_binary(&missing, &asset, temp.path()).is_err());
        let duplicate = archive_with(
            temp.path(),
            "duplicate.tgz",
            &[("phoxal-target", "one"), ("phoxal-target", "two")],
        );
        assert!(extract_binary(&duplicate, &asset, temp.path()).is_err());

        let nested = archive_with(
            temp.path(),
            "nested.tgz",
            &[("nested/phoxal-target", "bytes")],
        );
        assert!(extract_binary(&nested, &asset, temp.path()).is_err());

        let apple_double = archive_with(
            temp.path(),
            "apple-double.tgz",
            &[("phoxal-target", "cli"), ("._phoxal-target", "metadata")],
        );
        assert!(extract_binary(&apple_double, &asset, temp.path()).is_err());
    }

    #[test]
    fn checksum_mismatch_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("archive");
        let checksum = temp.path().join("archive.sha256");
        fs::write(&archive, "bytes").unwrap();
        fs::write(&checksum, format!("{}  archive\n", "0".repeat(64))).unwrap();
        assert!(verify_published_archive(&archive, &checksum, "archive").is_err());
    }

    #[test]
    fn managed_installations_are_not_self_replaced() {
        assert!(refuse_managed_install(Path::new("/x/.cargo/bin/phoxal")).is_err());
        assert!(refuse_managed_install(Path::new("/opt/homebrew/bin/phoxal")).is_err());
        assert!(refuse_managed_install(Path::new("/usr/local/bin/phoxal")).is_ok());
    }

    #[test]
    fn workflow_packages_the_one_binary_upgrade_expects() {
        let workflow = fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../.github/workflows/release.yml"),
        )
        .unwrap();
        assert!(workflow.contains("-p phoxal-cli"));
        assert!(workflow.contains("\"phoxal-${{ matrix.target }}\""));
        assert!(workflow.contains("COPYFILE_DISABLE=1 tar"));
    }

    #[test]
    fn upgrade_action_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_value(UpgradeAction::UpToDate).unwrap(),
            "up_to_date"
        );
    }
}
