//! Host-native tool provisioning.
//!
//! The Webots controller + supervisor are host-native binaries that ship as a
//! single `phoxal/framework` release tarball (`phoxal-simulator-<version>-<target>.tar.gz`).
//! The resolver records the explicit tool version and expected asset/binary
//! names; this module performs the actual download + extraction into the tool
//! cache so a live `simulate` can spawn the controllers without the user
//! fetching anything by hand.

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

const SIMULATOR_WEBOTS_CONTROLLER: &str = "simulator_webots_controller";
const SIMULATOR_WEBOTS_SUPERVISOR: &str = "simulator_webots_supervisor";

/// Ensure the host-native Webots controller + supervisor binaries are present in
/// the tool cache, downloading them when they are missing.
///
/// The two binaries ride a single `phoxal/framework` simulator release tarball,
/// so when either is missing we download that tarball once, verify it against
/// the release's published sha256, and extract both binaries into
/// `~/.phoxal/cache/tools/<tool>/<version>/`.
///
/// Idempotent: binaries already in the cache are left untouched, so a warm cache
/// makes this a no-op. Intended for the live `simulate` path only — a dry-run
/// must stay offline and therefore does not call this.
pub fn ensure_simulator_binaries(ui: &Ui, resolved: &ResolvedRobot) -> Result<()> {
    let mut wanted: Vec<(&ResolvedTool, PathBuf)> = Vec::new();
    for tool_name in [SIMULATOR_WEBOTS_CONTROLLER, SIMULATOR_WEBOTS_SUPERVISOR] {
        let Some(tool) = resolved.tools.iter().find(|tool| tool.name == tool_name) else {
            // The resolver always emits both simulator tools; their absence is a
            // resolution contract break, not a recoverable provisioning miss.
            bail!(
                "resolved tool {tool_name} is missing from the resolution; cannot provision the Webots binaries"
            );
        };
        let dest = cached_tool_path(&tool.name, &tool.resolved, &tool.binary_name)?;
        wanted.push((tool, dest));
    }

    // Controller + supervisor are version-matched and share one tarball (same
    // repo + version + asset). Assert that invariant across BOTH tools — not
    // just the missing ones — so that a cached-one/missing-one split can't mix a
    // stale cached binary with one freshly downloaded from a different asset if a
    // future catalog change splits them.
    let (lead_tool, _) = &wanted[0];
    let repo = lead_tool.repo.as_str();
    let version = lead_tool.resolved.as_str();
    let asset = lead_tool.asset.as_str();
    for (tool, _) in &wanted {
        if tool.repo != repo || tool.resolved != version || tool.asset != asset {
            bail!(
                "simulator tools disagree on release asset ({} {} {} vs {} {} {}); cannot provision from a single tarball",
                lead_tool.name,
                version,
                asset,
                tool.name,
                tool.resolved,
                tool.asset
            );
        }
    }

    let missing: Vec<&(&ResolvedTool, PathBuf)> =
        wanted.iter().filter(|(_, dest)| !dest.is_file()).collect();
    if missing.is_empty() {
        return Ok(());
    }

    ui.info(format!(
        "provisioning Webots simulator binaries from {repo} v{version}"
    ));
    let tarball = download_release_asset(ui, repo, version, asset)?;
    for (tool, dest) in &missing {
        extract_binary(&tarball, &tool.binary_name, dest)
            .with_context(|| format!("failed to extract {} from {asset}", tool.binary_name))?;
        ui.success(format!(
            "staged {} into {}",
            tool.binary_name,
            dest.display()
        ));
    }
    Ok(())
}

/// Download a `phoxal/<repo>` release asset and verify it against the release's
/// published sha256 digest.
///
/// We re-fetch the authoritative digest from the GitHub release API rather than
/// trusting the lockfile's per-tool sha: during the pre-publish recovery period
/// that field is a content-addressing placeholder (see `resolver::fake_sha`),
/// not a real artifact hash.
fn download_release_asset(ui: &Ui, repo: &str, version: &str, asset: &str) -> Result<Vec<u8>> {
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

    match resolve_release_asset_sha256(repo, version, asset)? {
        Some(expected) => {
            let actual = hex::encode(Sha256::digest(&bytes));
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
    Ok(bytes)
}

/// Extract the single archive member named `binary_name` from an in-memory
/// `.tar.gz` into `dest`, marking it executable. Writes to a sibling `.partial`
/// file first and renames into place so a crash mid-write can't leave a
/// truncated binary in the cache.
fn extract_binary(tarball: &[u8], binary_name: &str, dest: &Path) -> Result<()> {
    let mut archive = Archive::new(GzDecoder::new(tarball));
    for entry in archive
        .entries()
        .context("failed to read simulator tarball")?
    {
        let mut entry = entry.context("failed to read simulator tarball entry")?;
        let entry_path = entry
            .path()
            .context("simulator tarball entry has no path")?;
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
    bail!("simulator tarball does not contain expected binary {binary_name}")
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
        let bytes = download_release_asset(&Ui, "phoxal/framework", "0.8.0", asset)
            .expect("download + sha256-verify the simulator tarball");
        assert!(bytes.len() > 1_000_000, "tarball should be multi-MB");

        let dir = tempfile::tempdir().expect("tempdir");
        for binary in [
            "phoxal-simulator-webots-controller-aarch64-apple-darwin",
            "phoxal-simulator-webots-supervisor-aarch64-apple-darwin",
        ] {
            let dest = dir.path().join(binary);
            extract_binary(&bytes, binary, &dest).expect("extract binary");
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
