//! Self-upgrade support.
//!
//! Asset naming contract: each release publishes an archive named
//! `phoxal-cli-<version-no-v>-<target>.tar.gz` containing a binary named
//! `phoxal-cli-<target>`. The archive has a sibling checksum asset named
//! `<archive>.sha256` whose content is `<hex>  <archive-filename>`.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand};
use reqwest::StatusCode;
use reqwest::blocking::Client;
use semver::Version;
use serde::Serialize;

use crate::commands::MessageFormat;
use crate::{AppContext, Ui};

const LATEST_RELEASE_URL: &str = "https://github.com/phoxal/phoxal-cli/releases/latest";
const DOWNLOAD_BASE_URL: &str = "https://github.com/phoxal/phoxal-cli/releases/download";
const USER_AGENT: &str = "phoxal-cli";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Args)]
pub struct SelfCmd {
    #[command(subcommand)]
    pub command: SelfSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum SelfSubcommand {
    #[command(about = "Upgrade this phoxal-cli executable.")]
    Upgrade(Upgrade),
}

#[derive(Debug, Args)]
pub struct Upgrade {
    #[arg(
        long,
        value_name = "tag",
        value_parser = parse_version_arg,
        help = "Install this release tag (e.g. v0.5.0) instead of the latest."
    )]
    pub version: Option<Version>,
    #[arg(long, help = "Reinstall even when already on the target version.")]
    pub force: bool,
    #[arg(
        long,
        value_enum,
        default_value_t = MessageFormat::Human,
        help = "Output format for the upgrade outcome."
    )]
    pub message_format: MessageFormat,
}

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

/// Structured outcome of `self upgrade`: what it did (or would report doing),
/// shared by the human and JSON output paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpgradeOutcome {
    pub version_from: String,
    pub version_to: String,
    pub source: String,
    pub upgraded: bool,
    pub action: UpgradeAction,
}

impl SelfCmd {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        match &self.command {
            SelfSubcommand::Upgrade(command) => command.run(app).await,
        }
    }
}

impl Upgrade {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let ui = app.ui;
        let message_format = self.message_format;
        let options = UpgradeOptions {
            requested_version: self.version.clone(),
            force: self.force,
        };
        let outcome = tokio::task::spawn_blocking(move || run_upgrade(options, ui, message_format))
            .await
            .context("self upgrade worker failed")??;
        crate::commands::print_message(
            &outcome,
            || {
                // The upgrade worker already announced the outcome (progress
                // lines on stderr, terminal line on stdout) as it ran; nothing
                // further to print for the human path.
                Ok(())
            },
            message_format,
        )
    }
}

pub fn parse_version_arg(raw: &str) -> std::result::Result<Version, String> {
    let normalized = normalize_version_tag(raw);
    Version::parse(normalized).map_err(|error| format!("invalid version '{raw}': {error}"))
}

fn run_upgrade(
    options: UpgradeOptions,
    ui: Ui,
    message_format: MessageFormat,
) -> Result<UpgradeOutcome> {
    let human = message_format == MessageFormat::Human;
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
            if human {
                ui.info("resolving latest phoxal-cli release");
            }
            discover_latest_version(&client)?
        }
    };

    if requested_version == current_version && !options.force {
        if human {
            println!("already up to date (v{requested_version})");
        }
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

    if human {
        ui.info(format!("downloading {}", asset.archive_url));
    }
    download_asset(&download_client, &asset.archive_url, &archive_path)?
        .context("release archive was not found")?;

    if human {
        ui.info(format!("downloading {}", asset.checksum_url));
    }
    match download_asset(&download_client, &asset.checksum_url, &checksum_path)? {
        Some(()) => verify_checksum(&archive_path, &checksum_path, &asset.archive_name)?,
        None if options.force && pinned && requested_version < current_version => {
            if human {
                ui.warn(format!(
                    "release v{requested_version} has no checksum; continuing because --force pinned an older version"
                ));
            }
        }
        None => bail!(
            "release v{requested_version} has no checksum asset {}; refusing to self-upgrade",
            asset.checksum_name
        ),
    }

    let new_binary_path = extract_binary(&archive_path, &asset.binary_name, temp_dir.path())
        .with_context(|| format!("failed to extract {}", asset.binary_name))?;
    let current_exe = current_executable()?;
    refuse_managed_install(&current_exe)?;
    replace_current_executable(&new_binary_path)?;

    let action = if requested_version < current_version {
        UpgradeAction::Switched
    } else {
        UpgradeAction::Upgraded
    };
    if human {
        let verb = match action {
            UpgradeAction::Switched => "switched",
            UpgradeAction::Upgraded => "upgraded",
            UpgradeAction::UpToDate => unreachable!("UpToDate returns earlier"),
        };
        ui.success(format!(
            "{verb} phoxal-cli v{current_version} -> v{requested_version}"
        ));
    }
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

fn extract_binary(archive_path: &Path, binary_name: &str, temp_root: &Path) -> Result<PathBuf> {
    let archive_file = fs::File::open(archive_path)
        .with_context(|| format!("failed to open {}", archive_path.display()))?;
    let decoder = flate2::read::GzDecoder::new(archive_file);
    let mut archive = tar::Archive::new(decoder);
    let extract_dir = temp_root.join("extract");
    fs::create_dir_all(&extract_dir)
        .with_context(|| format!("failed to create {}", extract_dir.display()))?;
    let destination = extract_dir.join(binary_name);

    for entry in archive.entries().context("failed to read tar archive")? {
        let mut entry = entry.context("failed to read tar entry")?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path().context("failed to read tar entry path")?;
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if file_name != binary_name {
            continue;
        }
        entry
            .unpack(&destination)
            .with_context(|| format!("failed to unpack {binary_name}"))?;
        make_executable(&destination)?;
        return Ok(destination);
    }

    bail!("tar archive did not contain {binary_name}")
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
            "refusing to self-upgrade cargo-installed phoxal-cli at {display}; reinstall with `cargo install --git https://github.com/phoxal/phoxal-cli`"
        );
    }
    if path.contains("/Cellar/") || path.contains("/homebrew/") {
        bail!(
            "refusing to self-upgrade Homebrew-managed phoxal-cli at {display}; use brew to upgrade or reinstall it"
        );
    }
    Ok(())
}

fn replace_current_executable(new_binary_path: &Path) -> Result<()> {
    self_replace::self_replace(new_binary_path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::PermissionDenied {
            anyhow!(
                "permission denied replacing the current executable; re-run with appropriate permissions or reinstall via the install script"
            )
        } else {
            anyhow!(error).context("failed to replace current executable")
        }
    })
}

struct ReleaseAsset {
    archive_name: String,
    binary_name: String,
    checksum_name: String,
    archive_url: String,
    checksum_url: String,
}

impl ReleaseAsset {
    fn new(version: &Version, target: &str) -> Self {
        let archive_name = format!("phoxal-cli-{version}-{target}.tar.gz");
        let binary_name = format!("phoxal-cli-{target}");
        let checksum_name = format!("{archive_name}.sha256");
        let archive_url = format!("{DOWNLOAD_BASE_URL}/v{version}/{archive_name}");
        let checksum_url = format!("{DOWNLOAD_BASE_URL}/v{version}/{checksum_name}");
        Self {
            archive_name,
            binary_name,
            checksum_name,
            archive_url,
            checksum_url,
        }
    }
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
        let outcome = run_upgrade(options, Ui::from_env(), MessageFormat::Human)?;

        assert_eq!(outcome.version_from, current_version().to_string());
        assert_eq!(outcome.version_to, current_version().to_string());
        assert_eq!(outcome.source, "pinned tag");
        assert!(!outcome.upgraded);
        assert_eq!(outcome.action, UpgradeAction::UpToDate);
        Ok(())
    }

    #[test]
    fn up_to_date_outcome_is_identical_across_human_and_json_message_formats() -> Result<()> {
        let human_outcome = run_upgrade(
            UpgradeOptions {
                requested_version: Some(current_version()),
                force: false,
            },
            Ui::from_env(),
            MessageFormat::Human,
        )?;
        let json_outcome = run_upgrade(
            UpgradeOptions {
                requested_version: Some(current_version()),
                force: false,
            },
            Ui::from_env(),
            MessageFormat::Json,
        )?;

        assert_eq!(human_outcome, json_outcome);
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
