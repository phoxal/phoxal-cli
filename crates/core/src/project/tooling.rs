use std::{
    fs,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use anyhow::{Context, Result, anyhow, bail};
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

pub fn cargo_binary_name(crate_dir: &Path, preferred_name: Option<&str>) -> Result<String> {
    let manifest_path = crate_dir.join("Cargo.toml");
    let contents = fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let manifest = toml::from_str::<TomlValue>(&contents)
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
    let bin_names = manifest
        .get("bin")
        .and_then(TomlValue::as_array)
        .map(|bins| {
            bins.iter()
                .filter_map(|bin| {
                    bin.as_table()
                        .and_then(|table| table.get("name"))
                        .and_then(TomlValue::as_str)
                        .map(str::to_string)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if let Some(preferred_name) = preferred_name
        && bin_names.iter().any(|bin_name| bin_name == preferred_name)
    {
        return Ok(preferred_name.to_string());
    }
    if bin_names.len() == 1 {
        return Ok(bin_names[0].clone());
    }
    if !bin_names.is_empty() {
        if let Some(preferred_name) = preferred_name {
            bail!(
                "{} declares multiple [[bin]] targets ({}) but none named '{preferred_name}'",
                manifest_path.display(),
                bin_names.join(", ")
            );
        }
        bail!(
            "{} declares multiple [[bin]] targets ({}); declare exactly one [[bin]] or use package.name as the binary",
            manifest_path.display(),
            bin_names.join(", ")
        );
    }

    manifest
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(TomlValue::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("{} does not declare package.name", manifest_path.display()))
}
