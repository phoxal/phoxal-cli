//! Process bootstrap is owned by the binary entry point.
//!
//! `self upgrade` moves the whole CLI pair. `phoxal` and `phoxald` are one
//! product released under one version, so the archive
//! carries both, both are extracted before anything is touched, and the swap
//! either lands both or leaves the installation exactly as it was. A successful
//! upgrade that produced a mixed pair is not a degraded outcome to warn about -
//! it is an outcome this module must make impossible.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::StatusCode;
use reqwest::blocking::Client;
use semver::Version;
use serde::Serialize;

use crate::cli::{AppContext, Ui};

const LATEST_RELEASE_URL: &str = "https://github.com/phoxal/phoxal-cli/releases/latest";
const DOWNLOAD_BASE_URL: &str = "https://github.com/phoxal/phoxal-cli/releases/download";
const USER_AGENT: &str = "phoxal-cli";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
#[derive(Debug, Clone)]
struct UpgradeOptions {
    requested_version: Option<Version>,
    force: bool,
}

/// How the requested version compares to the version already installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpgradeAction {
    /// The requested version matched the running binary and `--force` was not set.
    UpToDate,
    /// The installed binary was replaced with an older release (`--version` pinned below current).
    Switched,
    /// The installed binary was replaced with a newer release.
    Upgraded,
}

/// Outcome of `self upgrade` retained as an internal testable value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpgradeOutcome {
    pub version_from: String,
    pub version_to: String,
    pub source: String,
    pub upgraded: bool,
    pub action: UpgradeAction,
}

pub(crate) fn parse_version_arg(raw: &str) -> std::result::Result<Version, String> {
    let normalized = normalize_version_tag(raw);
    Version::parse(normalized).map_err(|error| format!("invalid version '{raw}': {error}"))
}

fn run_upgrade(options: UpgradeOptions, ui: Ui) -> Result<UpgradeOutcome> {
    let target = target_triple()?;
    let current_version =
        Version::parse(env!("CARGO_PKG_VERSION")).context("invalid CARGO_PKG_VERSION")?;
    let pinned = options.requested_version.is_some();
    let source = if pinned {
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
    let temp_dir = tempfile::tempdir().context("failed to create self-upgrade temp directory")?;
    let archive_path = temp_dir.path().join(&asset.archive_name);
    let checksum_path = temp_dir.path().join(&asset.checksum_name);
    let download_client = build_client(true)?;

    ui.info(format!("downloading {}", asset.archive_url));
    download_asset(&download_client, &asset.archive_url, &archive_path)?
        .context("release archive was not found")?;

    ui.info(format!("downloading {}", asset.checksum_url));
    match download_asset(&download_client, &asset.checksum_url, &checksum_path)? {
        Some(()) => verify_checksum(&archive_path, &checksum_path, &asset.archive_name)?,
        None if options.force && pinned && requested_version < current_version => {
            ui.warn(format!(
                "release v{requested_version} has no checksum; continuing because --force pinned an older version"
            ));
        }
        None => bail!(
            "release v{requested_version} has no checksum asset {}; refusing to self-upgrade",
            asset.checksum_name
        ),
    }

    // Both halves are extracted and verified present before anything on disk is
    // replaced: an archive that carries only one is a broken release, and the
    // right moment to say so is while the installation is still untouched.
    let staged = extract_pair(&archive_path, &asset, temp_dir.path())?;
    let current_exe = current_executable()?;
    refuse_managed_install(&current_exe)?;
    let directory = current_exe
        .parent()
        .context("the running executable has no parent directory")?;
    install_pair(directory, &staged, &SelfReplaceInstaller)?;

    let action = if requested_version < current_version {
        UpgradeAction::Switched
    } else {
        UpgradeAction::Upgraded
    };
    let verb = match action {
        UpgradeAction::Switched => "switched",
        UpgradeAction::Upgraded => "upgraded",
        UpgradeAction::UpToDate => unreachable!("UpToDate returns earlier"),
    };
    ui.success(format!(
        "{verb} the phoxal + phoxald pair v{current_version} -> v{requested_version}"
    ));
    Ok(UpgradeOutcome {
        version_from: current_version.to_string(),
        version_to: requested_version.to_string(),
        source,
        upgraded: true,
        action,
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
    parse_version_tag(tag).with_context(|| {
        format!("latest release redirect pointed to invalid phoxal-cli tag '{tag}'")
    })
}

fn parse_version_tag(tag: &str) -> Result<Version> {
    let normalized = normalize_version_tag(tag);
    Version::parse(normalized).with_context(|| format!("invalid version tag '{tag}'"))
}

fn download_asset(client: &Client, url: &str, destination: &Path) -> Result<Option<()>> {
    let mut response = client
        .get(url)
        .send()
        .with_context(|| format!("failed to download {url}"))?;
    let status = response.status();
    if status == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        bail!("download {url} returned {status}");
    }
    let mut file = fs::File::create(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    response
        .copy_to(&mut file)
        .with_context(|| format!("failed to write {}", destination.display()))?;
    Ok(Some(()))
}

fn verify_checksum(archive_path: &Path, checksum_path: &Path, archive_name: &str) -> Result<()> {
    let checksum = fs::read_to_string(checksum_path)
        .with_context(|| format!("failed to read {}", checksum_path.display()))?;
    let expected = parse_checksum(&checksum, archive_name)?;
    let actual = sha256_file(archive_path)?;
    if actual != expected {
        bail!("checksum mismatch for {archive_name}: expected {expected}, got {actual}");
    }
    Ok(())
}

fn parse_checksum(contents: &str, archive_name: &str) -> Result<String> {
    let line = contents
        .lines()
        .find(|line| !line.trim().is_empty())
        .context("checksum asset was empty")?;
    let (hex, filename) = line.split_once("  ").with_context(|| {
        format!("checksum asset must contain '<hex>  {archive_name}' with two spaces")
    })?;
    let filename = filename.trim_end_matches('\r');
    if filename != archive_name {
        bail!("checksum asset names {filename}, expected {archive_name}");
    }
    if hex.len() != 64 || !hex.chars().all(|value| value.is_ascii_hexdigit()) {
        bail!("checksum asset contains invalid SHA256 digest {hex}");
    }
    Ok(hex.to_ascii_lowercase())
}

fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};

    let mut file =
        fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Both halves of the pair, extracted and not yet installed.
#[derive(Debug)]
struct StagedPair {
    client: PathBuf,
    daemon: PathBuf,
}

/// Extract `phoxal` and `phoxald` from one release archive.
///
/// An archive missing either half is rejected outright rather than half
/// applied: the pair is the unit of release, so half of one is not a partial
/// success.
fn extract_pair(archive_path: &Path, asset: &ReleaseAsset, temp_root: &Path) -> Result<StagedPair> {
    let archive_file = fs::File::open(archive_path)
        .with_context(|| format!("failed to open {}", archive_path.display()))?;
    let decoder = flate2::read::GzDecoder::new(archive_file);
    let mut archive = tar::Archive::new(decoder);
    let extract_dir = temp_root.join("extract");
    fs::create_dir_all(&extract_dir)
        .with_context(|| format!("failed to create {}", extract_dir.display()))?;
    let mut client = None;
    let mut daemon = None;

    for entry in archive.entries().context("failed to read tar archive")? {
        let mut entry = entry.context("failed to read tar entry")?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path().context("failed to read tar entry path")?;
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let file_name = file_name.to_owned();
        let slot = if file_name == asset.client_binary_name {
            &mut client
        } else if file_name == asset.daemon_binary_name {
            &mut daemon
        } else {
            continue;
        };
        let destination = extract_dir.join(&file_name);
        drop(path);
        entry
            .unpack(&destination)
            .with_context(|| format!("failed to unpack {file_name}"))?;
        make_executable(&destination)?;
        *slot = Some(destination);
    }

    match (client, daemon) {
        (Some(client), Some(daemon)) => Ok(StagedPair { client, daemon }),
        (client, daemon) => bail!(
            "release archive {} is not a complete CLI pair: it is missing {}. `phoxal` and \
             `phoxald` are released together, so this release cannot be installed",
            asset.archive_name,
            match (client.is_some(), daemon.is_some()) {
                (false, false) => format!(
                    "both {} and {}",
                    asset.client_binary_name, asset.daemon_binary_name
                ),
                (true, false) => asset.daemon_binary_name.clone(),
                (false, true) => asset.client_binary_name.clone(),
                (true, true) => unreachable!("both present is the success arm"),
            }
        ),
    }
}

/// Replacing the running executable.
///
/// Behind a trait because it is the one step that cannot be undone, and the
/// all-or-nothing behavior around it has to be provable without replacing the
/// test binary.
trait PairInstaller {
    fn replace_client(&self, new_binary: &Path) -> Result<()>;
}

struct SelfReplaceInstaller;

impl PairInstaller for SelfReplaceInstaller {
    fn replace_client(&self, new_binary: &Path) -> Result<()> {
        replace_current_executable(new_binary)
    }
}

/// Install both halves, or leave the installation exactly as it was.
///
/// The daemon moves first and the client last. That order is the whole point:
/// the daemon swap is a rename within one directory, so the previous bytes are
/// still there to put back, while replacing the running client is irreversible.
/// Doing the recoverable step first means the irreversible one can still fail
/// without ever leaving a mixed pair behind.
fn install_pair(
    directory: &Path,
    staged: &StagedPair,
    installer: &dyn PairInstaller,
) -> Result<()> {
    let daemon = directory.join(crate::pair::DAEMON_BINARY);
    let incoming = directory.join(format!(
        ".{}.incoming-{}",
        crate::pair::DAEMON_BINARY,
        std::process::id()
    ));
    let previous = directory.join(format!(
        ".{}.previous-{}",
        crate::pair::DAEMON_BINARY,
        std::process::id()
    ));

    // Copy into the install directory before renaming: a rename within one
    // directory is atomic, a move across filesystems is not, and the extracted
    // pair lives in a temp directory that is usually on another one.
    fs::copy(&staged.daemon, &incoming).with_context(|| {
        format!(
            "failed to stage the new {} in {}",
            crate::pair::DAEMON_BINARY,
            directory.display()
        )
    })?;
    make_executable(&incoming)?;

    let had_daemon = daemon.exists();
    if had_daemon {
        fs::rename(&daemon, &previous)
            .with_context(|| format!("failed to set the current {} aside", daemon.display()))?;
    }
    if let Err(error) = fs::rename(&incoming, &daemon) {
        restore_daemon(had_daemon, &previous, &daemon);
        let _ = fs::remove_file(&incoming);
        return Err(
            anyhow!(error).context(format!("failed to install the new {}", daemon.display()))
        );
    }

    if let Err(error) = installer.replace_client(&staged.client) {
        restore_daemon(had_daemon, &previous, &daemon);
        return Err(error.context(
            "the CLI pair was left untouched: the supervisor was restored because the client \
             could not be replaced",
        ));
    }

    if had_daemon {
        let _ = fs::remove_file(&previous);
    }
    Ok(())
}

/// Put the previous daemon back, or remove the one that was just installed when
/// there was no previous daemon at all.
fn restore_daemon(had_daemon: bool, previous: &Path, daemon: &Path) {
    if had_daemon {
        let _ = fs::rename(previous, daemon);
    } else {
        let _ = fs::remove_file(daemon);
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("failed to chmod {}", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

fn current_executable() -> Result<PathBuf> {
    let path = std::env::current_exe().context("failed to locate current executable")?;
    path.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize current executable {}",
            path.display()
        )
    })
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

fn replace_current_executable(new_binary_path: &Path) -> Result<()> {
    self_replace::self_replace(new_binary_path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::PermissionDenied {
            anyhow!(
                "permission denied replacing the current executable; re-run with appropriate permissions or reinstall via `curl -fsSL https://phoxal.com/install.sh | sh`"
            )
        } else {
            anyhow!(error).context("failed to replace current executable")
        }
    })
}

/// The published assets for one version and target.
///
/// One archive carries the whole pair, named for the target inside it, which is
/// the contract `.github/workflows/release.yml` produces and the phoxal.com
/// installer consumes.
struct ReleaseAsset {
    archive_name: String,
    client_binary_name: String,
    daemon_binary_name: String,
    checksum_name: String,
    archive_url: String,
    checksum_url: String,
}

impl ReleaseAsset {
    fn new(version: &Version, target: &str) -> Self {
        let archive_name = format!("phoxal-{version}-{target}.tar.gz");
        let client_binary_name = format!("{}-{target}", crate::pair::CLIENT_BINARY);
        let daemon_binary_name = format!("{}-{target}", crate::pair::DAEMON_BINARY);
        let checksum_name = format!("{archive_name}.sha256");
        let archive_url = format!("{DOWNLOAD_BASE_URL}/v{version}/{archive_name}");
        let checksum_url = format!("{DOWNLOAD_BASE_URL}/v{version}/{checksum_name}");
        Self {
            archive_name,
            client_binary_name,
            daemon_binary_name,
            checksum_name,
            archive_url,
            checksum_url,
        }
    }
}

pub(crate) async fn upgrade(
    app: &AppContext,
    requested_version: Option<Version>,
    force: bool,
) -> Result<()> {
    let ui = app.ui;
    let options = UpgradeOptions {
        requested_version,
        force,
    };
    tokio::task::spawn_blocking(move || run_upgrade(options, ui))
        .await
        .context("self upgrade worker failed")??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn current_version() -> Version {
        Version::parse(env!("CARGO_PKG_VERSION")).expect("CARGO_PKG_VERSION should be valid")
    }

    #[test]
    fn up_to_date_pinned_version_reports_up_to_date_without_upgrading() -> Result<()> {
        let options = UpgradeOptions {
            requested_version: Some(current_version()),
            force: false,
        };
        let outcome = run_upgrade(options, Ui::from_env())?;

        assert_eq!(outcome.version_from, current_version().to_string());
        assert_eq!(outcome.version_to, current_version().to_string());
        assert_eq!(outcome.source, "pinned tag");
        assert!(!outcome.upgraded);
        assert_eq!(outcome.action, UpgradeAction::UpToDate);
        Ok(())
    }

    #[test]
    fn upgrade_outcome_serializes_to_the_documented_json_shape() {
        let outcome = UpgradeOutcome {
            version_from: "0.5.0".to_string(),
            version_to: "0.6.0".to_string(),
            source: "latest release".to_string(),
            upgraded: true,
            action: UpgradeAction::Upgraded,
        };

        let value = serde_json::to_value(&outcome).expect("outcome should serialize");
        assert_eq!(
            value,
            serde_json::json!({
                "version_from": "0.5.0",
                "version_to": "0.6.0",
                "source": "latest release",
                "upgraded": true,
                "action": "upgraded",
            })
        );
    }

    /// Build a `.tar.gz` carrying exactly the named entries.
    fn archive_with(root: &Path, name: &str, entries: &[(&str, &str)]) -> PathBuf {
        let archive_path = root.join(name);
        let file = fs::File::create(&archive_path).expect("create archive");
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        for (entry_name, contents) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, entry_name, contents.as_bytes())
                .expect("append entry");
        }
        builder
            .into_inner()
            .expect("finish tar")
            .finish()
            .expect("finish gzip");
        archive_path
    }

    fn asset() -> ReleaseAsset {
        ReleaseAsset::new(
            &Version::parse("0.36.0").expect("version"),
            "aarch64-apple-darwin",
        )
    }

    /// An upgrade only proceeds from an archive that carries the whole pair.
    #[test]
    fn a_release_archive_must_carry_both_binaries() {
        let temp = tempfile::tempdir().expect("temp dir");
        let asset = asset();
        let both = archive_with(
            temp.path(),
            "both.tar.gz",
            &[
                (asset.client_binary_name.as_str(), "client bytes"),
                (asset.daemon_binary_name.as_str(), "daemon bytes"),
            ],
        );
        let staged = extract_pair(&both, &asset, temp.path()).expect("a complete pair extracts");
        assert_eq!(fs::read_to_string(&staged.client).unwrap(), "client bytes");
        assert_eq!(fs::read_to_string(&staged.daemon).unwrap(), "daemon bytes");

        for (name, entries, missing) in [
            (
                "client-only.tar.gz",
                vec![(asset.client_binary_name.as_str(), "client bytes")],
                asset.daemon_binary_name.as_str(),
            ),
            (
                "daemon-only.tar.gz",
                vec![(asset.daemon_binary_name.as_str(), "daemon bytes")],
                asset.client_binary_name.as_str(),
            ),
        ] {
            let archive = archive_with(temp.path(), name, &entries);
            let error = extract_pair(&archive, &asset, temp.path())
                .expect_err("half a pair is never installable")
                .to_string();
            assert!(error.contains(missing), "{error}");
            assert!(error.contains("released together"), "{error}");
        }
    }

    struct FakeInstaller {
        fail: bool,
        replaced: std::cell::RefCell<Option<PathBuf>>,
    }

    impl PairInstaller for FakeInstaller {
        fn replace_client(&self, new_binary: &Path) -> Result<()> {
            if self.fail {
                bail!("permission denied replacing the current executable");
            }
            *self.replaced.borrow_mut() = Some(new_binary.to_path_buf());
            Ok(())
        }
    }

    fn staged_pair(root: &Path) -> StagedPair {
        let client = root.join("new-phoxal");
        let daemon = root.join("new-phoxald");
        fs::write(&client, "new client").expect("write client");
        fs::write(&daemon, "new daemon").expect("write daemon");
        StagedPair { client, daemon }
    }

    /// The good path moves both halves.
    #[test]
    fn a_successful_upgrade_replaces_both_halves() {
        let temp = tempfile::tempdir().expect("temp dir");
        let install = temp.path().join("bin");
        fs::create_dir_all(&install).expect("install dir");
        fs::write(install.join(crate::pair::DAEMON_BINARY), "old daemon").expect("old daemon");
        let staged = staged_pair(temp.path());
        let installer = FakeInstaller {
            fail: false,
            replaced: std::cell::RefCell::new(None),
        };

        install_pair(&install, &staged, &installer).expect("the pair installs");

        assert_eq!(
            fs::read_to_string(install.join(crate::pair::DAEMON_BINARY)).unwrap(),
            "new daemon"
        );
        assert_eq!(installer.replaced.into_inner(), Some(staged.client.clone()));
        assert!(
            leftovers(&install).is_empty(),
            "no staging files are left behind: {:?}",
            leftovers(&install)
        );
    }

    /// A mixed pair must be impossible: if the irreversible half fails, the
    /// recoverable half is put back and the installation is unchanged
    ///.
    #[test]
    fn a_failed_client_replacement_restores_the_previous_daemon() {
        let temp = tempfile::tempdir().expect("temp dir");
        let install = temp.path().join("bin");
        fs::create_dir_all(&install).expect("install dir");
        fs::write(install.join(crate::pair::DAEMON_BINARY), "old daemon").expect("old daemon");
        let staged = staged_pair(temp.path());
        let installer = FakeInstaller {
            fail: true,
            replaced: std::cell::RefCell::new(None),
        };

        let error = install_pair(&install, &staged, &installer)
            .expect_err("a failed client replacement fails the upgrade")
            .to_string();

        assert!(error.contains("left untouched"), "{error}");
        assert_eq!(
            fs::read_to_string(install.join(crate::pair::DAEMON_BINARY)).unwrap(),
            "old daemon",
            "the previous supervisor is restored"
        );
        assert!(
            leftovers(&install).is_empty(),
            "no staging files are left behind: {:?}",
            leftovers(&install)
        );
    }

    /// The same rule with nothing to restore: a first-time install that fails
    /// its client half must not leave a lone daemon behind.
    #[test]
    fn a_failed_first_install_leaves_no_lone_daemon() {
        let temp = tempfile::tempdir().expect("temp dir");
        let install = temp.path().join("bin");
        fs::create_dir_all(&install).expect("install dir");
        let staged = staged_pair(temp.path());
        let installer = FakeInstaller {
            fail: true,
            replaced: std::cell::RefCell::new(None),
        };

        install_pair(&install, &staged, &installer).expect_err("the upgrade fails");

        assert!(!install.join(crate::pair::DAEMON_BINARY).exists());
        assert!(
            leftovers(&install).is_empty(),
            "no staging files are left behind: {:?}",
            leftovers(&install)
        );
    }

    /// Every file in the install directory that is not one of the two binaries.
    fn leftovers(install: &Path) -> Vec<String> {
        fs::read_dir(install)
            .expect("read install dir")
            .filter_map(std::result::Result::ok)
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| name != crate::pair::DAEMON_BINARY && name != crate::pair::CLIENT_BINARY)
            .collect()
    }

    /// The producer and the consumer of the release asset contract must agree.
    ///
    /// This reads the packaging plan rather than a built archive on purpose: a
    /// real cross-target release build is minutes of work to prove one naming
    /// contract, while the contract itself is entirely in the workflow's text -
    /// which packages are built and which file names go into the tar. What the
    /// archive must contain once built is proven by
    /// `a_release_archive_must_carry_both_binaries` against a real tar.
    #[test]
    fn the_release_workflow_packages_both_binaries_under_the_names_upgrade_expects() {
        let workflow = fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../.github/workflows/release.yml"),
        )
        .expect("the release workflow is readable");
        let asset = asset();

        for package in ["-p phoxal-cli-client", "-p phoxal-cli-supervisor"] {
            assert!(
                workflow.contains(package),
                "the release build must produce the whole pair: {package} is missing"
            );
        }
        assert!(
            workflow.contains("for bin in phoxal phoxald; do"),
            "both binaries must be copied into the archive directory"
        );
        for name in [&asset.client_binary_name, &asset.daemon_binary_name] {
            // The workflow writes the target through a matrix expression, so
            // the contract to check is the prefix plus the expansion.
            let (prefix, _) = name.split_once('-').expect("<bin>-<target>");
            assert!(
                workflow.contains(&format!("\"{prefix}-${{{{ matrix.target }}}}\"")),
                "{name} must be packaged under `<bin>-<target>`"
            );
        }
        assert!(
            workflow.contains(
                "archive=\"phoxal-${{ needs.plan.outputs.version }}-${{ matrix.target }}.tar.gz\""
            ),
            "the archive name must be the one `self upgrade` downloads: {}",
            asset.archive_name
        );
    }

    #[test]
    fn upgrade_action_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_value(UpgradeAction::UpToDate).unwrap(),
            serde_json::json!("up_to_date")
        );
        assert_eq!(
            serde_json::to_value(UpgradeAction::Switched).unwrap(),
            serde_json::json!("switched")
        );
        assert_eq!(
            serde_json::to_value(UpgradeAction::Upgraded).unwrap(),
            serde_json::json!("upgraded")
        );
    }
}
