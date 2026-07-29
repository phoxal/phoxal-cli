use std::path::Path;

use anyhow::{Context, Result, anyhow};

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

pub fn staged_mesh_path_from_urdf_filename(
    filename: &str,
    component_mesh_prefix: Option<&str>,
) -> String {
    if let Some(path) = filename.strip_prefix("package://meshes/") {
        return normalize_component_mesh_path(path, component_mesh_prefix);
    }
    if let Some(path) = filename.strip_prefix("package://robot/meshes/") {
        return path.to_string();
    }
    if let Some(path) = filename.strip_prefix("model://robot/meshes/") {
        return path.to_string();
    }
    if let Some(path) = filename.strip_prefix("meshes/") {
        return normalize_component_mesh_path(path, component_mesh_prefix);
    }
    filename.to_string()
}

fn normalize_component_mesh_path(path: &str, component_mesh_prefix: Option<&str>) -> String {
    let Some(component_mesh_prefix) = component_mesh_prefix else {
        return path.to_string();
    };

    if path
        .strip_prefix(component_mesh_prefix)
        .is_some_and(|rest| rest.starts_with('/'))
    {
        path.to_string()
    } else {
        format!("{component_mesh_prefix}/{path}")
    }
}

#[cfg(test)]
mod tests {
    use super::staged_mesh_path_from_urdf_filename;

    #[test]
    fn component_mesh_paths_are_prefixed_even_when_nested() {
        assert_eq!(
            staged_mesh_path_from_urdf_filename("meshes/wheel.stl", Some("ddsm115")),
            "ddsm115/wheel.stl"
        );
        assert_eq!(
            staged_mesh_path_from_urdf_filename("meshes/visual/wheel.stl", Some("ddsm115")),
            "ddsm115/visual/wheel.stl"
        );
        assert_eq!(
            staged_mesh_path_from_urdf_filename(
                "package://meshes/visual/wheel.stl",
                Some("ddsm115")
            ),
            "ddsm115/visual/wheel.stl"
        );
    }

    #[test]
    fn robot_mesh_paths_and_already_prefixed_component_paths_are_stable() {
        assert_eq!(
            staged_mesh_path_from_urdf_filename("meshes/base.stl", None),
            "base.stl"
        );
        assert_eq!(
            staged_mesh_path_from_urdf_filename("package://robot/meshes/base.stl", Some("ddsm115")),
            "base.stl"
        );
        assert_eq!(
            staged_mesh_path_from_urdf_filename("model://robot/meshes/base.stl", Some("ddsm115")),
            "base.stl"
        );
        assert_eq!(
            staged_mesh_path_from_urdf_filename("meshes/ddsm115/visual/wheel.stl", Some("ddsm115")),
            "ddsm115/visual/wheel.stl"
        );
    }
}
