use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Args;
use toml::Value as TomlValue;

use crate::AppContext;
use crate::catalog::SUPPORTED_RUNTIME_TRAIN;
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
        check_user_runtime_deps(app)?;
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

fn check_user_runtime_deps(app: &AppContext) -> Result<()> {
    let robot_path = match crate::resolver::discover_robot_yaml(app.project.root()) {
        Ok(path) => path,
        Err(_) => {
            app.ui
                .warn("robot.yaml not found; skipping user runtime dependency check");
            return Ok(());
        }
    };
    let robot = crate::resolver::load_robot(&robot_path)?;
    let robot_root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?;
    let expected = expected_runtime_train(app, robot_root);
    let Some(expected_train) = parse_runtime_train(&expected) else {
        app.ui.warn(format!(
            "could not parse supported platform runtime train {expected}; skipping user runtime dependency check"
        ));
        return Ok(());
    };

    for (name, runtime) in &robot.user_runtimes {
        let runtime_dir = resolve_robot_path(robot_root, &runtime.path);
        let manifest_path = runtime_dir.join("Cargo.toml");
        let Ok(contents) = fs::read_to_string(&manifest_path) else {
            app.ui.warn(format!(
                "user runtime '{name}' has no readable Cargo.toml at {}; cannot check phoxal dependency",
                manifest_path.display()
            ));
            continue;
        };
        let Ok(manifest) = toml::from_str::<TomlValue>(&contents) else {
            app.ui.warn(format!(
                "user runtime '{name}' has an unparsable Cargo.toml at {}; cannot check phoxal dependency",
                manifest_path.display()
            ));
            continue;
        };
        let Some(dep) = manifest
            .get("dependencies")
            .and_then(|dependencies| dependencies.get("phoxal"))
        else {
            app.ui.warn(format!(
                "user runtime '{name}' is missing a phoxal dependency; pin it to the platform runtime train {expected}"
            ));
            continue;
        };

        match phoxal_dependency(dep) {
            PhoxalDependency::Branch(branch) => app.ui.warn(format!(
                "user runtime '{name}' floats on branch '{branch}'; pin it to the platform runtime train {expected}"
            )),
            PhoxalDependency::Pinned(version) => match parse_runtime_train(&version) {
                Some(actual_train) if actual_train == expected_train => app.ui.success(format!(
                    "user runtime '{name}' phoxal dependency matches platform runtime train {expected}"
                )),
                Some(_) => app.ui.warn(format!(
                    "user runtime '{name}' pins phoxal {version}; expected platform runtime train {expected}"
                )),
                None => app.ui.warn(format!(
                    "user runtime '{name}' has an unparsable phoxal dependency {version}; pin it to the platform runtime train {expected}"
                )),
            },
            PhoxalDependency::Unparsable => app.ui.warn(format!(
                "user runtime '{name}' has an unparsable phoxal dependency; pin it to the platform runtime train {expected}"
            )),
        }
    }

    Ok(())
}

fn expected_runtime_train(app: &AppContext, robot_root: &Path) -> String {
    let lock_path = robot_root.join(LOCKFILE_NAME);
    if !lock_path.is_file() {
        return SUPPORTED_RUNTIME_TRAIN.to_string();
    }
    match Lockfile::read(&lock_path) {
        Ok(lockfile) => lockfile.phoxal_runtimes.resolved,
        Err(err) => {
            app.ui.warn(format!(
                "failed to read {}; checking user runtime deps against {SUPPORTED_RUNTIME_TRAIN}: {err:#}",
                lock_path.display()
            ));
            SUPPORTED_RUNTIME_TRAIN.to_string()
        }
    }
}

fn resolve_robot_path(robot_root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        robot_root.join(path)
    }
}

enum PhoxalDependency {
    Branch(String),
    Pinned(String),
    Unparsable,
}

fn phoxal_dependency(dep: &TomlValue) -> PhoxalDependency {
    if let Some(version) = dep.as_str() {
        return PhoxalDependency::Pinned(version.to_string());
    }
    let Some(table) = dep.as_table() else {
        return PhoxalDependency::Unparsable;
    };
    if let Some(branch) = table.get("branch").and_then(TomlValue::as_str) {
        return PhoxalDependency::Branch(branch.to_string());
    }
    if let Some(version) = table.get("version").and_then(TomlValue::as_str) {
        return PhoxalDependency::Pinned(version.to_string());
    }
    if let Some(tag) = table.get("tag").and_then(TomlValue::as_str) {
        return PhoxalDependency::Pinned(tag.trim_start_matches('v').to_string());
    }
    PhoxalDependency::Unparsable
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RuntimeTrain {
    major: u64,
    minor: u64,
}

fn parse_runtime_train(value: &str) -> Option<RuntimeTrain> {
    let trimmed = value.trim().trim_start_matches('v');
    for token in trimmed
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '+'))
    {
        let token = token.trim_start_matches('v');
        let mut parts = token.split('.');
        let (Some(major), Some(minor)) = (parts.next(), parts.next()) else {
            continue;
        };
        if !major.chars().all(|ch| ch.is_ascii_digit())
            || !minor.chars().all(|ch| ch.is_ascii_digit())
        {
            continue;
        }
        return Some(RuntimeTrain {
            major: major.parse().ok()?,
            minor: minor.parse().ok()?,
        });
    }
    None
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
            "rustup is missing; install it from https://rustup.rs (detected OS: {})",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_version_and_requirement_trains() {
        let train = RuntimeTrain { major: 0, minor: 7 };

        assert_eq!(parse_runtime_train("0.7.0"), Some(train));
        assert_eq!(parse_runtime_train("v0.7.1"), Some(train));
        assert_eq!(parse_runtime_train("^0.7"), Some(train));
        assert_eq!(parse_runtime_train(">=0.7, <0.8"), Some(train));
        assert_eq!(parse_runtime_train("main"), None);
    }

    #[test]
    fn classifies_phoxal_dependency_forms() {
        let manifest: TomlValue = toml::from_str(
            r#"
[dependencies]
string = "0.7.0"
table = { version = "^0.7" }
tag = { git = "https://github.com/phoxal/framework", tag = "v0.7.0" }
branch = { git = "https://github.com/phoxal/framework", branch = "main" }
"#,
        )
        .expect("valid toml");
        let dependencies = manifest
            .get("dependencies")
            .expect("dependencies")
            .as_table()
            .expect("dependencies table");

        assert!(matches!(
            phoxal_dependency(dependencies.get("string").expect("string")),
            PhoxalDependency::Pinned(version) if version == "0.7.0"
        ));
        assert!(matches!(
            phoxal_dependency(dependencies.get("table").expect("table")),
            PhoxalDependency::Pinned(version) if version == "^0.7"
        ));
        assert!(matches!(
            phoxal_dependency(dependencies.get("tag").expect("tag")),
            PhoxalDependency::Pinned(version) if version == "0.7.0"
        ));
        assert!(matches!(
            phoxal_dependency(dependencies.get("branch").expect("branch")),
            PhoxalDependency::Branch(branch) if branch == "main"
        ));
    }
}
