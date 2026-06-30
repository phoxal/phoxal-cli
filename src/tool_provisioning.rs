//! Host-native tool provisioning.
//!
//! Webots controllers, joypad, and future host-native tools ship as release
//! tarballs rather than Docker images. The resolver records each tool's
//! explicit version plus expected asset/binary names; this module performs the
//! download + extraction into the tool cache before commands run the binaries.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use tar::Archive;

use crate::resolver::{ResolvedRobot, ResolvedTool, resolve_release_asset_sha256};
use crate::simulator_staging::cached_tool_path;
use crate::ui::Ui;
use crate::utils::make_executable;

type ToolAssetKey = (String, String, String);
type MissingToolBinary<'a> = (&'a ResolvedTool, PathBuf);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisioningMode {
    MissingOnly,
    Refresh,
}

impl ProvisioningMode {
    #[must_use]
    pub const fn from_pull(pull: bool) -> Self {
        if pull {
            Self::Refresh
        } else {
            Self::MissingOnly
        }
    }
}

/// Ensure the requested host-native tool binaries are present in the cache,
/// downloading and extracting release assets when binaries are missing.
///
/// Tools sharing the same `(repo, version, asset)` are grouped so one tarball
/// download can satisfy multiple binaries, for example the Webots controller
/// and supervisor. Idempotent: binaries already in the cache are left untouched.
pub fn ensure_tool_binaries<I, S>(ui: &Ui, resolved: &ResolvedRobot, tool_names: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    ensure_tool_binaries_with_mode(ui, resolved, tool_names, ProvisioningMode::MissingOnly)
}

pub fn ensure_tool_binaries_with_mode<I, S>(
    ui: &Ui,
    resolved: &ResolvedRobot,
    tool_names: I,
    mode: ProvisioningMode,
) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let requested = tool_names
        .into_iter()
        .map(|name| name.as_ref().to_string())
        .collect::<BTreeSet<_>>();
    if requested.is_empty() {
        return Ok(());
    }

    let mut missing_by_asset: BTreeMap<ToolAssetKey, Vec<MissingToolBinary<'_>>> = BTreeMap::new();
    for tool_name in requested {
        let Some(tool) = resolved.tools.iter().find(|tool| tool.name == tool_name) else {
            bail!(
                "resolved tool {tool_name} is missing from the resolution; cannot provision requested host tools"
            );
        };
        let dest = cached_tool_path(&tool.name, &tool.resolved, &tool.binary_name)?;
        if mode == ProvisioningMode::MissingOnly && dest.is_file() {
            continue;
        }
        missing_by_asset
            .entry((tool.repo.clone(), tool.resolved.clone(), tool.asset.clone()))
            .or_default()
            .push((tool, dest));
    }

    for ((repo, version, asset), missing) in missing_by_asset {
        ui.info(format!("provisioning host tools from {repo} v{version}"));
        let tarball = download_release_asset(ui, &repo, &version, &asset)?;
        let asset_path = cached_tool_asset_path(&repo, &version, &asset)?;
        write_cached_tool_asset(&asset_path, &tarball.bytes)?;
        for (tool, dest) in missing {
            extract_binary(&tarball.bytes, &tool.binary_name, &dest)
                .with_context(|| format!("failed to extract {} from {asset}", tool.binary_name))?;
            ui.success(format!(
                "staged {} into {}",
                tool.binary_name,
                dest.display()
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DownloadedAsset {
    bytes: Vec<u8>,
}

/// Download a `phoxal/<repo>` release asset and verify it against the release's
/// published sha256 digest.
///
/// We fetch the authoritative digest from the GitHub release API rather than
/// trusting the resolved per-tool sha: during the pre-publish recovery period
/// that field is a content-addressing placeholder (see `resolver::fake_sha`),
/// not a real artifact hash.
fn download_release_asset(
    ui: &Ui,
    repo: &str,
    version: &str,
    asset: &str,
) -> Result<DownloadedAsset> {
    let url = format!("https://github.com/{repo}/releases/download/v{version}/{asset}");
    let client = reqwest::blocking::Client::builder()
        .user_agent("phoxal-cli")
        .timeout(Duration::from_secs(120))
        .build()?;
    let mut request = client.get(&url);
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .with_context(|| format!("failed to download {url}"))?;
    if !response.status().is_success() {
        bail!(
            "download of {asset} from {repo} v{version} returned {}",
            response.status()
        );
    }
    let bytes = response
        .bytes()
        .with_context(|| format!("failed to read {asset} body"))?
        .to_vec();

    let actual = hex::encode(Sha256::digest(&bytes));
    match resolve_release_asset_sha256(repo, version, asset)? {
        Some(expected) => {
            if actual != expected {
                bail!("sha256 mismatch for {asset}: expected {expected}, got {actual}");
            }
        }
        None => {
            ui.warn(format!(
                "{repo} v{version} {asset} exposes no published sha256; skipping integrity check"
            ));
        }
    }
    Ok(DownloadedAsset { bytes })
}

pub(crate) fn cached_tool_asset_path(repo: &str, version: &str, asset: &str) -> Result<PathBuf> {
    let repo = repo.replace('/', "__");
    Ok(crate::host_paths::tools_cache_dir()?
        .join("_assets")
        .join(repo)
        .join(version)
        .join(asset))
}

pub(crate) fn cached_tool_asset_sha256(tool: &ResolvedTool) -> Result<Option<String>> {
    let asset_path = cached_tool_asset_path(&tool.repo, &tool.resolved, &tool.asset)?;
    if !asset_path.is_file() {
        return Ok(None);
    }
    Ok(Some(hex::encode(Sha256::digest(
        fs::read(&asset_path).with_context(|| {
            format!("failed to read cached tool asset {}", asset_path.display())
        })?,
    ))))
}

fn write_cached_tool_asset(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let partial = path.with_extension("partial");
    fs::write(&partial, bytes).with_context(|| format!("failed to write {}", partial.display()))?;
    fs::rename(&partial, path).with_context(|| format!("failed to finalize {}", path.display()))
}

/// Extract the single archive member named `binary_name` from an in-memory
/// `.tar.gz` into `dest`, marking it executable. Writes to a sibling `.partial`
/// file first and renames into place so a crash mid-write can't leave a
/// truncated binary in the cache.
fn extract_binary(tarball: &[u8], binary_name: &str, dest: &Path) -> Result<()> {
    let mut archive = Archive::new(GzDecoder::new(tarball));
    for entry in archive.entries().context("failed to read tool tarball")? {
        let mut entry = entry.context("failed to read tool tarball entry")?;
        let entry_path = entry.path().context("tool tarball entry has no path")?;
        if entry_path.file_name().and_then(|name| name.to_str()) != Some(binary_name) {
            continue;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let partial = dest.with_extension("partial");
        let mut out = fs::File::create(&partial)
            .with_context(|| format!("failed to create {}", partial.display()))?;
        io::copy(&mut entry, &mut out)
            .with_context(|| format!("failed to write {}", partial.display()))?;
        drop(out);
        make_executable(&partial)?;
        fs::rename(&partial, dest)
            .with_context(|| format!("failed to finalize {}", dest.display()))?;
        return Ok(());
    }
    bail!("tool tarball does not contain expected binary {binary_name}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;

    fn tar_gz_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
        for (name, body) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, name, &body[..])
                .expect("append tar entry");
        }
        builder
            .into_inner()
            .expect("finish tar")
            .finish()
            .expect("finish gzip")
    }

    #[test]
    fn extract_binary_writes_named_member_and_marks_executable() {
        let tarball = tar_gz_with(&[
            (
                "phoxal-simulator-webots-controller-aarch64-apple-darwin",
                b"CTRL",
            ),
            (
                "phoxal-simulator-webots-supervisor-aarch64-apple-darwin",
                b"SUPER",
            ),
        ]);
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir
            .path()
            .join("phoxal-simulator-webots-controller-aarch64-apple-darwin");

        extract_binary(
            &tarball,
            "phoxal-simulator-webots-controller-aarch64-apple-darwin",
            &dest,
        )
        .expect("extract controller");

        assert_eq!(fs::read(&dest).expect("read extracted"), b"CTRL");
        assert!(!dest.with_extension("partial").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&dest).expect("metadata").permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "extracted binary must be executable");
        }
    }

    // Real end-to-end check against the published phoxal/framework release:
    // downloads the simulator tarball, verifies it against the release's sha256,
    // and extracts both host-native binaries. Ignored by default (network +
    // host-target specific); run manually with:
    //   cargo test -p phoxal-cli --lib -- --ignored provisions_real_simulator_release
    #[test]
    #[ignore = "hits the network and is specific to aarch64-apple-darwin"]
    fn provisions_real_simulator_release_from_github() {
        let asset = "phoxal-simulator-0.8.0-aarch64-apple-darwin.tar.gz";
        let tarball = download_release_asset(&Ui, "phoxal/framework", "0.8.0", asset)
            .expect("download + sha256-verify the simulator tarball");
        assert!(
            tarball.bytes.len() > 1_000_000,
            "tarball should be multi-MB"
        );

        let dir = tempfile::tempdir().expect("tempdir");
        for binary in [
            "phoxal-simulator-webots-controller-aarch64-apple-darwin",
            "phoxal-simulator-webots-supervisor-aarch64-apple-darwin",
        ] {
            let dest = dir.path().join(binary);
            extract_binary(&tarball.bytes, binary, &dest).expect("extract binary");
            assert!(
                fs::metadata(&dest).expect("stat extracted").len() > 0,
                "extracted {binary} must be non-empty"
            );
        }
    }

    #[test]
    fn extract_binary_errors_when_member_absent() {
        let tarball = tar_gz_with(&[("some-other-binary", b"x")]);
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("missing");

        let err =
            extract_binary(&tarball, "not-present", &dest).expect_err("missing member must error");
        assert!(
            err.to_string().contains("not-present"),
            "error should name the missing binary, got: {err}"
        );
    }
}
