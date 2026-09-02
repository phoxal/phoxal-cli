//! Filesystem staging for generated simulation worlds and meshes.

use crate::simulation::prepare::ControllerLaunch;
use crate::simulation::prepare::RobotToStage;
use crate::simulation::prepare::stage_simulation_world;
use crate::simulation::webots::root;
use anyhow::Context;
use anyhow::Result;
use phoxal::bundle::RuntimeBundle;
use std::path::Path;
use std::path::PathBuf;

/// Stage a resolved robot and authored world into the Webots filesystem view,
/// returning the staged world Webots is opened on.
pub(crate) fn stage_simulation_for_robot(
    project_root: &Path,
    world_source_path: &Path,
    bundle: &RuntimeBundle,
    connect_endpoint: &str,
) -> Result<PathBuf> {
    // Prepare every generated file in the task-local tree before reconciling
    // it into the ordinary Webots project.
    let base_world_text = std::fs::read_to_string(world_source_path)
        .with_context(|| format!("failed to read {}", world_source_path.display()))?;
    let world_name = world_source_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .context("world source path has no file stem")?;

    let robot = bundle.robot();
    let controller_launch = ControllerLaunch {
        bundle_root: bundle.root().to_path_buf(),
        connect_endpoint: connect_endpoint.to_string(),
    };

    let component_types = robot
        .components()
        .filter(|instance| instance.simulation().is_some())
        .map(|instance| instance.instance().component_type().as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();

    let mesh_root = root::meshes_dir(project_root);
    stage_compiled_geometry_assets(robot, bundle, &mesh_root)?;
    stage_simulation_world(
        &base_world_text,
        &root::protos_dir(project_root),
        &mesh_root,
        &root::world_path(project_root, world_name),
        RobotToStage {
            bundle: robot,
            component_types,
            controller_launch,
        },
    )
}

fn stage_compiled_geometry_assets(
    robot: &phoxal::model::Robot,
    bundle: &RuntimeBundle,
    mesh_root: &Path,
) -> Result<()> {
    // Geometry staging runs before `stage_simulation_world`, while the fresh
    // Webots tree contains only the parent `protos` directory.
    std::fs::create_dir_all(mesh_root).with_context(|| {
        format!(
            "failed to create staged Webots mesh root {}",
            mesh_root.display()
        )
    })?;
    let mut referenced = robot
        .structure()
        .asset_ids()
        .collect::<std::collections::BTreeSet<_>>();
    for instance in robot.components() {
        referenced.extend(instance.component_type().structure().asset_ids());
    }
    validate_unique_geometry_destinations(referenced.iter().copied())?;
    for id in referenced {
        let bytes = bundle
            .asset(id)
            .with_context(|| format!("compiled geometry asset '{}' is missing", id.as_str()))?;
        let relative = crate::simulation::webots::proto::support::paths::staged_geometry_path(id);
        let destination = mesh_root.join(relative);
        let parent = destination
            .parent()
            .context("compiled mesh destination has no parent")?;
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create staged mesh directory {}",
                parent.display()
            )
        })?;
        std::fs::write(&destination, &bytes)
            .with_context(|| format!("failed to stage compiled mesh {}", destination.display()))?;
    }
    Ok(())
}

fn validate_unique_geometry_destinations<'a>(
    assets: impl IntoIterator<Item = &'a phoxal::model::AssetId>,
) -> Result<()> {
    let mut destinations = std::collections::BTreeMap::new();
    for asset in assets {
        let relative =
            crate::simulation::webots::proto::support::paths::staged_geometry_path(asset);
        if let Some(previous) =
            destinations.insert(relative.to_string(), asset.as_str().to_string())
        {
            anyhow::bail!(
                "compiled geometry assets '{}' and '{}' map to the same staged path '{}'",
                previous,
                asset.as_str(),
                relative
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_geometry_ids_cannot_overwrite_one_staged_path() -> Result<()> {
        let prefixed = phoxal::model::AssetId::new("meshes/wheel/body.stl".to_string())?;
        let unprefixed = phoxal::model::AssetId::new("wheel/body.stl".to_string())?;
        let error = validate_unique_geometry_destinations([&prefixed, &unprefixed])
            .expect_err("both identifiers normalize to one Webots path");
        assert!(error.to_string().contains("same staged path"));
        Ok(())
    }
}
