pub mod component;

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use phoxal_utils_conventions::{MESHES_DIR, MODEL_URI_PREFIX, PACKAGE_URI_PREFIX};
use phoxal_utils_structure::Structure;
use urdf_rs::Geometry;

pub fn validate_mesh_paths_exist(root: &Path, structure: &Structure) -> Result<()> {
    for link in &structure.links {
        for visual in &link.visual {
            validate_geometry_mesh(root, &visual.geometry)?;
        }
        for collision in &link.collision {
            validate_geometry_mesh(root, &collision.geometry)?;
        }
    }

    Ok(())
}

fn validate_geometry_mesh(root: &Path, geometry: &Geometry) -> Result<()> {
    let Geometry::Mesh { filename, .. } = geometry else {
        return Ok(());
    };

    let trimmed = filename
        .trim_start_matches(PACKAGE_URI_PREFIX)
        .trim_start_matches(MODEL_URI_PREFIX);
    let Some((_, relative_path)) = trimmed.split_once('/') else {
        bail!(
            "structure mesh '{}' must include a package/model name and relative path",
            filename
        );
    };

    let relative_path = PathBuf::from(relative_path);
    if relative_path.is_absolute() {
        bail!(
            "structure mesh '{}' must resolve to a relative path",
            filename
        );
    }

    let mesh_path = root.join(MESHES_DIR).join(relative_path);
    if !mesh_path.is_file() {
        bail!(
            "mesh '{}' resolves to missing file {}",
            filename,
            mesh_path.display()
        );
    }
    Ok(())
}
