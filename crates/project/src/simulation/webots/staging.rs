//! Filesystem staging for generated simulation worlds and meshes.

use crate::resolve::component_driver::component_assets_dir;
use crate::simulation::prepare::ComponentTypeToStage;
use crate::simulation::prepare::ControllerLaunch;
use crate::simulation::prepare::RobotToStage;
use crate::simulation::prepare::StagedSimulationWorld;
use crate::simulation::prepare::stage_simulation_world;
use crate::simulation::webots::root;
use anyhow::Context;
use anyhow::Result;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::resolver::ResolvedRobot;
use std::collections::BTreeMap;
use std::path::Path;

/// Stage a resolved robot and authored world into the Webots filesystem view.
pub(crate) fn stage_simulation_for_robot(
    project_root: &Path,
    world_source_path: &Path,
    resolved: &ResolvedRobot,
    launch_plan: &LaunchPlan,
    connect_endpoints: &[String],
    runtime_root: &Path,
) -> Result<StagedSimulationWorld> {
    // Prepare every generated file in the task-local tree before reconciling
    // it into the ordinary Webots project.
    let base_world_text = std::fs::read_to_string(world_source_path)
        .with_context(|| format!("failed to read {}", world_source_path.display()))?;
    let world_name = world_source_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .context("world source path has no file stem")?;

    let robot = launch_plan
        .robots
        .first()
        .context("sim launch plan has no robot")?;
    let robot_id = &resolved.robot.robot.id;
    anyhow::ensure!(
        !connect_endpoints.is_empty(),
        "Webots controller requires the resident router endpoint"
    );
    let controller_launch = ControllerLaunch {
        namespace: robot.namespace.clone(),
        robot_id: robot_id.clone(),
        robot_root: Some(runtime_root.to_path_buf()),
        connect_endpoints: connect_endpoints.to_vec(),
    };

    let structure_path = project_root.join(&resolved.robot.robot.structure);
    let structure = phoxal::model::structure::Structure::read_from_file(&structure_path)
        .with_context(|| {
            format!(
                "failed to read robot structure declared by robot.yaml structure: {}",
                resolved.robot.robot.structure.display()
            )
        })?;
    structure
        .validate()
        .context("robot structure failed validation")?;

    let mut components = BTreeMap::new();
    let mut component_type_dirs = BTreeMap::new();
    for component in &resolved.components {
        if component_type_dirs.contains_key(&component.source_name) {
            continue;
        }
        let crate_dir = component_assets_dir(component, project_root)?;
        phoxal_cli_core::schema::ensure_supported_revision(
            &crate_dir.join("component.yaml"),
            phoxal_cli_core::schema::DocumentKind::Component,
        )?;
        let component_model = phoxal::model::component::Component::read_from_dir(&crate_dir)
            .with_context(|| {
                format!(
                    "failed to read component.yaml for component type '{}' from {}",
                    component.source_name,
                    crate_dir.display()
                )
            })?
            .as_v0()
            .context("Webots staging only supports component.yaml version v0")?
            .clone();
        components.insert(component.source_name.clone(), component_model);
        component_type_dirs.insert(component.source_name.clone(), crate_dir);
    }

    let bundle = phoxal::model::v0::Robot {
        manifest: resolved.robot.clone(),
        components,
        structure,
    };
    // Only stage a PROTO for component types that actually carry Webots
    // simulation data - a component with no `simulation.yaml` has nothing for
    // `generate_component_proto` to render and is not expected to be staged.
    let component_types = component_type_dirs
        .iter()
        .filter(|(_, source_dir)| {
            source_dir.join("simulation.yaml").is_file()
                || source_dir.join("simulation.yml").is_file()
        })
        .map(|(component_type, source_dir)| ComponentTypeToStage {
            component_type,
            source_dir,
        })
        .collect::<Vec<_>>();

    let mesh_root = root::meshes_dir()?;
    // The Phase-6 mesh-staging gap: the generated PROTOs reference mesh assets
    // relative to `mesh_root` (the robot's own meshes directly under it, each
    // component's under `<mesh_root>/<component_type>/` per
    // `component_mesh_prefix`), but nothing copied the physical mesh files
    // there before this fix - the robot spawned with no visible geometry.
    // The robot's own meshes and each component type's meshes are copied into
    // the task-local generation tree.
    stage_robot_meshes(project_root, &resolved.robot.robot.structure, &mesh_root)?;
    for (component_type, source_dir) in &component_type_dirs {
        stage_component_meshes(source_dir, component_type, &mesh_root)?;
    }
    stage_simulation_world(
        &base_world_text,
        &root::protos_dir()?,
        &mesh_root,
        &root::world_path(world_name)?,
        &[RobotToStage {
            robot_id: robot_id.clone(),
            bundle: &bundle,
            component_types,
            controller_launch,
        }],
    )
}

/// The mesh source directory convention: a `meshes/` sibling of the file a
/// URDF/robot document is anchored at (the robot's own `structure.urdf` at
/// the project root, or a component's `structure.urdf` in its source dir).
const MESHES_DIR: &str = "meshes";

/// Stage the robot's own `meshes/` directory (if any) directly under
/// `mesh_root` - `WebotsSceneDescription::from_robot` renders with
/// `component_mesh_prefix: None`, so the robot's own mesh URDF references
/// (`meshes/<file>`) resolve unprefixed, one level under `mesh_root` itself.
///
/// These copied assets keep the generated project independent of source paths.
pub(crate) fn stage_robot_meshes(
    project_root: &Path,
    structure_path: &Path,
    mesh_root: &Path,
) -> Result<()> {
    let structure_dir = project_root
        .join(structure_path)
        .parent()
        .map_or_else(|| project_root.to_path_buf(), std::path::Path::to_path_buf);
    let source = structure_dir.join(MESHES_DIR);
    if !source.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(mesh_root).with_context(|| {
        format!(
            "failed to create staged mesh directory {}",
            mesh_root.display()
        )
    })?;
    copy_dir_recursive(&source, mesh_root)
}

/// Stage one component type's `meshes/` directory as copied content. The
/// prefix `WebotsSceneDescription::from_component`'s `component_mesh_prefix`
/// embeds into the component's own mesh URDF references (`meshes/<file>` ->
/// `<component_type>/<file>`, see `staged_mesh_path_from_urdf_filename`), so
/// the generated PROTOs resolve through the copied directory. `mesh_root`
/// itself must already exist (see
/// `root::wipe_and_recreate`) - not every robot has its own
/// meshes to trigger `stage_robot_meshes`' `create_dir_all`.
pub(crate) fn stage_component_meshes(
    source_dir: &Path,
    component_type: &str,
    mesh_root: &Path,
) -> Result<()> {
    let source = source_dir.join(MESHES_DIR);
    if !source.is_dir() {
        return Ok(());
    }
    let dest = mesh_root.join(component_type);
    std::fs::create_dir_all(&dest)?;
    copy_dir_recursive(&source, &dest)
}

pub(crate) fn copy_dir_recursive(source: &Path, dest: &Path) -> Result<()> {
    for entry in std::fs::read_dir(source)
        .with_context(|| format!("failed to read mesh source directory {}", source.display()))?
    {
        let entry = entry?;
        let source_path = entry.path();
        let metadata = std::fs::symlink_metadata(&source_path)?;
        anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "mesh content must not contain symlinks: {}",
            source_path.display()
        );
        let dest_path = dest.join(entry.file_name());
        if metadata.is_dir() {
            std::fs::create_dir_all(&dest_path).with_context(|| {
                format!(
                    "failed to create staged mesh directory {}",
                    dest_path.display()
                )
            })?;
            copy_dir_recursive(&source_path, &dest_path)?;
        } else if metadata.is_file() {
            std::fs::copy(&source_path, &dest_path).with_context(|| {
                format!(
                    "failed to stage mesh file {} to {}",
                    source_path.display(),
                    dest_path.display()
                )
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn mesh_staging_rejects_symlinks_with_the_source_path() -> Result<()> {
        let source = tempfile::tempdir()?;
        let destination = tempfile::tempdir()?;
        let mesh = source.path().join("body.stl");
        let linked = source.path().join("linked.stl");
        std::fs::write(&mesh, b"mesh")?;
        std::os::unix::fs::symlink(&mesh, &linked)?;
        let error = copy_dir_recursive(source.path(), destination.path())
            .expect_err("symlinked mesh must fail explicitly");
        assert!(error.to_string().contains(&linked.display().to_string()));
        Ok(())
    }
}
