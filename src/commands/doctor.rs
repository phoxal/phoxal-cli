use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use clap::Args;
use phoxal_cli_core::AppContext;
use phoxal_cli_core::shell;
use semver::{Version, VersionReq};

use crate::lockfile::{LOCKFILE_NAME, Lockfile};

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
        let path = tool_cache_path(app.project.root(), &name, &tool.resolved, &tool.asset);
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
        let destination = tool_cache_path(app.project.root(), &name, &tool.resolved, &tool.asset);
        if destination.is_file() {
            app.ui.success(format!("tool {name}: already cached"));
            continue;
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let url = format!(
            "https://github.com/phoxal/{name}/releases/download/v{}/{}",
            tool.resolved, tool.asset
        );
        app.ui.info(format!("downloading {url}"));
        let bytes = reqwest::get(&url).await?.bytes().await?;
        let actual = sha256_hex(&bytes);
        if actual != tool.sha256 {
            bail!(
                "downloaded tool {name} checksum mismatch: expected {}, got {}",
                tool.sha256,
                actual
            );
        }
        fs::write(&destination, bytes)
            .with_context(|| format!("failed to write {}", destination.display()))?;
        app.ui
            .success(format!("tool {name}: {}", destination.display()));
    }
    Ok(())
}

fn tool_cache_path(
    project_root: &Path,
    name: &str,
    version: &str,
    asset: &str,
) -> std::path::PathBuf {
    project_root
        .join(".phoxal")
        .join("cache")
        .join("tools")
        .join(name)
        .join(version)
        .join(asset)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}
