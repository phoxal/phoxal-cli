//! Project-local Cargo metadata, path resolution, and content hashing.

use std::{
    fs,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use anyhow::{Context, Result, anyhow};
use sha2::{Digest, Sha256};
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

pub fn hash_tree(path: &Path) -> Result<String> {
    let mut files = Vec::new();
    collect_hash_files(path, path, &mut files)?;
    files.sort();
    let mut hasher = Sha256::new();
    for file in files {
        hasher.update(file.to_string_lossy().as_bytes());
        hasher.update(fs::read(path.join(&file))?);
    }
    Ok(hex::encode(hasher.finalize())[..16].to_string())
}

fn collect_hash_files(root: &Path, path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        if name == ".git" || name == "target" {
            continue;
        }
        if path.is_dir() {
            collect_hash_files(root, &path, files)?;
        } else if path.is_file() {
            files.push(path.strip_prefix(root)?.to_path_buf());
        }
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
