//! Authored-project Cargo metadata and path resolution.

use std::{fs, path::Path, path::PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use anyhow::{Context, Result, anyhow};
use toml::Value as TomlValue;

pub fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)?;
    }

    Ok(())
}

pub fn resolve_project_path(project_root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    }
}

pub fn cargo_package_name(crate_dir: &Path) -> Result<String> {
    let manifest_path = crate_dir.join("Cargo.toml");
    let contents = fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let manifest = toml::from_str::<TomlValue>(&contents)
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
    manifest
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(TomlValue::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("{} does not declare package.name", manifest_path.display()))
}
