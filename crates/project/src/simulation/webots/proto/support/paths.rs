use std::path::Path;

use anyhow::{Context, Result, anyhow};

#[must_use]
pub fn staged_geometry_path(asset: &phoxal::model::AssetId) -> &str {
    asset
        .as_str()
        .strip_prefix("meshes/")
        .unwrap_or_else(|| asset.as_str())
}

pub fn relative_path_for_world(asset_path: &Path, world_target: &Path) -> Result<String> {
    relative_path_for_asset(asset_path, world_target)
}

pub fn relative_path_for_asset(asset_path: &Path, reference_path: &Path) -> Result<String> {
    let asset_path = asset_path
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", asset_path.display()))?;
    let reference_parent = reference_path
        .parent()
        .ok_or_else(|| anyhow!("reference path has no parent directory"))?;
    let reference_parent = reference_parent.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize reference directory {}",
            reference_parent.display()
        )
    })?;

    let relative_path =
        pathdiff::diff_paths(&asset_path, &reference_parent).unwrap_or_else(|| asset_path.clone());
    Ok(relative_path.to_string_lossy().replace('\\', "/"))
}
