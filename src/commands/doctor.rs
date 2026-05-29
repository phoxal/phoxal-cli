use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Args;
use semver::{Version, VersionReq};

use crate::AppContext;
use crate::host_paths;
use crate::shell;

use crate::lockfile::{LOCKFILE_NAME, LockedTool, Lockfile};

#[derive(Debug, Args)]
pub struct Doctor {
    #[arg(long, help = "Download missing pinned Phoxal-owned tool binaries.")]
    pub fix: bool,
}

impl Doctor {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        check_cli_min_version(app)?;
        check_docker(app)?;
        check_rustup(app);
        if self.fix {
            download_tools(app).await?;
        } else {
            report_tool_cache(app)?;
        }
        Ok(())
    }
}

fn check_cli_min_version(app: &AppContext) -> Result<()> {
    let robot_path = match crate::resolver::discover_robot_yaml(app.project.root()) {
        Ok(path) => path,
        Err(_) => {
            app.ui
                .warn("robot.yaml not found; skipping phoxal.cli_min_version check");
            return Ok(());
        }
    };
    let robot = crate::resolver::load_robot(&robot_path)?;
    let req = VersionReq::parse(&robot.phoxal.cli_min_version).with_context(|| {
        format!(
            "invalid phoxal.cli_min_version '{}'",
            robot.phoxal.cli_min_version
        )
    })?;
    let version = Version::parse(env!("CARGO_PKG_VERSION"))
        .context("installed phoxal-cli version is not semver")?;
    if !req.matches(&version) {
        bail!(
            "phoxal-cli {} does not satisfy robot.yaml phoxal.cli_min_version {}",
            version,
            robot.phoxal.cli_min_version
        );
    }
    app.ui.success(format!("phoxal-cli version {version}"));
    Ok(())
}

fn check_docker(app: &AppContext) -> Result<()> {
    shell::run_stdout("docker", ["--version"], None)
        .context("Docker is not installed or not on PATH")?;
    shell::run_stdout("docker", ["info"], None)
        .context("Docker is installed but the daemon is not running")?;
    app.ui.success("Docker daemon is running");
    Ok(())
}

fn check_rustup(app: &AppContext) {
    match shell::run_stdout("rustup", ["--version"], None) {
        Ok(_) => app.ui.success("rustup is installed"),
        Err(_) => app.ui.warn(format!(
            "rustup is missing; install from https://rustup.rs for {}",
            std::env::consts::OS
        )),
    }
}

fn report_tool_cache(app: &AppContext) -> Result<()> {
    let lock_path = app.project.root().join(LOCKFILE_NAME);
    if !lock_path.is_file() {
        app.ui
            .warn("phoxal.lock not found; run simulate once or use doctor --fix after locking");
        return Ok(());
    }
    let lockfile = Lockfile::read(&lock_path)?;
    for (name, tool) in lockfile.tools {
        let binary_name = tool_binary_name(&tool);
        let path = tool_cache_path(&name, &tool.resolved, binary_name)?;
        if path.is_file() {
            app.ui.success(format!("tool {name}: {}", path.display()));
        } else {
            app.ui
                .warn(format!("tool {name}: missing {}", path.display()));
        }
    }
    Ok(())
}

async fn download_tools(app: &AppContext) -> Result<()> {
    let lock_path = app.project.root().join(LOCKFILE_NAME);
    let lockfile = Lockfile::read(&lock_path).with_context(|| {
        format!(
            "doctor --fix needs a resolved {}; run simulate first",
            LOCKFILE_NAME
        )
    })?;
    for (name, tool) in lockfile.tools {
        let binary_name = tool_binary_name(&tool);
        let destination = tool_cache_path(&name, &tool.resolved, binary_name)?;
        if destination.is_file() {
            app.ui.success(format!("tool {name}: already cached"));
            continue;
        }
        if tool.sha256.is_empty() {
            app.ui.warn(format!(
                "tool {name}: {} is unpublished; skipping placeholder",
                tool.asset
            ));
            continue;
        }
        if tool.repo.is_empty() || tool.binary_name.is_empty() {
            app.ui.warn(format!(
                "tool {name}: lockfile is missing repo/binary metadata; run update --pin-digests"
            ));
            continue;
        }
        let url = format!(
            "https://github.com/{}/releases/download/v{}/{}",
            tool.repo, tool.resolved, tool.asset
        );
        let download = download_cache_path(&tool.repo, &tool.resolved, &tool.asset)?;
        let bytes = read_or_download(app, &download, &url).await?;
        let actual = sha256_hex(&bytes);
        if actual != tool.sha256 {
            bail!(
                "downloaded tool {name} checksum mismatch for {}: expected {}, got {}",
                tool.asset,
                tool.sha256,
                actual
            );
        }
        extract_binary(&bytes, &tool.binary_name, &destination).with_context(|| {
            format!("failed to extract {} from {}", tool.binary_name, tool.asset)
        })?;
        app.ui
            .success(format!("tool {name}: {}", destination.display()));
    }
    Ok(())
}

async fn read_or_download(app: &AppContext, path: &Path, url: &str) -> Result<Vec<u8>> {
    if path.is_file() {
        return fs::read(path).with_context(|| format!("failed to read {}", path.display()));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    app.ui.info(format!("downloading {url}"));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent("phoxal-cli")
        .build()?;
    let mut req = client.get(url);
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        req = req.bearer_auth(token);
    }
    let response = req.send().await?;
    if !response.status().is_success() {
        bail!("tool download returned {}", response.status());
    }
    let bytes = response.bytes().await?.to_vec();
    atomic_write(path, &bytes).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(bytes)
}

fn extract_binary(bytes: &[u8], binary_name: &str, destination: &Path) -> Result<()> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries().context("failed to read tar archive")? {
        let mut entry = entry.context("failed to read tar entry")?;
        let path = entry.path().context("failed to read tar entry path")?;
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if file_name != binary_name {
            continue;
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let mut extracted = Vec::new();
        entry
            .read_to_end(&mut extracted)
            .context("failed to read binary from tar entry")?;
        atomic_write(destination, &extracted)
            .with_context(|| format!("failed to write {}", destination.display()))?;
        make_executable(destination)?;
        return Ok(());
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

fn tool_cache_path(name: &str, version: &str, binary_name: &str) -> Result<PathBuf> {
    Ok(host_paths::cache_dir()?
        .join("tools")
        .join(name)
        .join(version)
        .join(binary_name))
}

fn download_cache_path(repo: &str, version: &str, asset: &str) -> Result<PathBuf> {
    Ok(host_paths::cache_dir()?
        .join("downloads")
        .join(repo)
        .join(format!("v{version}"))
        .join(asset))
}

fn tool_binary_name(tool: &LockedTool) -> &str {
    if tool.binary_name.is_empty() {
        &tool.asset
    } else {
        &tool.binary_name
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("cache path did not have a parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create temp file in {}", parent.display()))?;
    tmp.write_all(bytes)
        .with_context(|| format!("failed to write temp file in {}", parent.display()))?;
    tmp.persist(path)
        .map(|_| ())
        .with_context(|| format!("failed to persist {}", path.display()))
}
