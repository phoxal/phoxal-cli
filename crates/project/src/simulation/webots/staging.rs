//! Filesystem staging for generated simulation worlds and meshes.

use crate::simulation::prepare::ComponentTypeToStage;
use crate::simulation::prepare::ControllerLaunch;
use crate::simulation::prepare::RobotToStage;
use crate::simulation::prepare::StagedSimulationWorld;
use crate::simulation::prepare::stage_simulation_world;
use crate::simulation::webots::root;
use anyhow::Context;
use anyhow::Result;
use phoxal_bundle::RuntimeBundle;
use phoxal_runtime_contract::identity::{ExecutionId, ParticipantId};
use std::path::Path;

/// Stage a resolved robot and authored world into the Webots filesystem view.
pub(crate) fn stage_simulation_for_robot(
    project_root: &Path,
    world_source_path: &Path,
    bundle: &RuntimeBundle,
    execution: ExecutionId,
    participant: ParticipantId,
    connect_endpoint: &str,
) -> Result<StagedSimulationWorld> {
    // Prepare every generated file in the task-local tree before reconciling
    // it into the ordinary Webots project.
    let base_world_text = std::fs::read_to_string(world_source_path)
        .with_context(|| format!("failed to read {}", world_source_path.display()))?;
    let world_name = world_source_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .context("world source path has no file stem")?;

    let robot = bundle.robot();
    let robot_id = robot.id();
    anyhow::ensure!(
        !connect_endpoint.trim().is_empty(),
        "Webots controller requires the supervisor router endpoint"
    );
    let controller_launch = ControllerLaunch {
        execution,
        participant,
        bundle_root: bundle.root().to_path_buf(),
        connect_endpoint: connect_endpoint.to_string(),
    };

    let component_types = robot
        .components()
        .filter(|instance| {
            robot
                .simulation_for_component_type(instance.component_type().as_str())
                .is_some()
        })
        .map(|instance| instance.component_type())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(|component_type| ComponentTypeToStage {
            component_type: component_type.as_str(),
        })
        .collect::<Vec<_>>();

    let mesh_root = root::meshes_dir(project_root);
    stage_compiled_geometry_assets(robot, bundle.assets(), &mesh_root)?;
    stage_simulation_world(
        &base_world_text,
        &root::protos_dir(project_root),
        &mesh_root,
        &root::world_path(project_root, world_name),
        &[RobotToStage {
            robot_id: robot_id.to_string(),
            bundle: robot,
            component_types,
            controller_launch,
        }],
    )
}

fn stage_compiled_geometry_assets(
    robot: &phoxal_model::Robot,
    assets: &phoxal_bundle::ParticipantAssets,
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
    let canonical_root = mesh_root
        .canonicalize()
        .context("failed to resolve staged Webots mesh root")?;
    let mut referenced = robot
        .structure()
        .asset_ids()
        .collect::<std::collections::BTreeSet<_>>();
    for instance in robot.components() {
        let component = robot
            .component_for_instance(instance.id().as_str())
            .with_context(|| {
                format!(
                    "canonical robot component instance '{}' has no component definition",
                    instance.id()
                )
            })?;
        referenced.extend(component.structure().asset_ids());
    }
    validate_unique_geometry_destinations(referenced.iter().copied())?;
    for id in referenced {
        let bytes = assets
            .read(id)
            .with_context(|| format!("compiled geometry asset '{}' is missing", id.as_str()))?;
        anyhow::ensure!(
            Path::new(id.as_str())
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_))),
            "compiled geometry asset '{}' has an unsafe path",
            id.as_str()
        );
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
        let canonical = destination.canonicalize().with_context(|| {
            format!(
                "failed to resolve staged geometry {}",
                destination.display()
            )
        })?;
        anyhow::ensure!(
            canonical.starts_with(&canonical_root),
            "compiled geometry asset '{}' escaped the Webots mesh root",
            id.as_str()
        );
    }
    Ok(())
}

fn validate_unique_geometry_destinations<'a>(
    assets: impl IntoIterator<Item = &'a phoxal_model::AssetId>,
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
        let prefixed = phoxal_model::AssetId::new("meshes/wheel/body.stl".to_string())?;
        let unprefixed = phoxal_model::AssetId::new("wheel/body.stl".to_string())?;
        let error = validate_unique_geometry_destinations([&prefixed, &unprefixed])
            .expect_err("both identifiers normalize to one Webots path");
        assert!(error.to_string().contains("same staged path"));
        Ok(())
    }
}
