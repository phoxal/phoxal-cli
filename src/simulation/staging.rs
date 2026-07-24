//! Filesystem staging for generated simulation worlds and meshes.

use super::require_absolute_symlink_target;
use crate::component_driver::component_assets_dir;
use crate::simulate_staging::ComponentTypeToStage;
use crate::simulate_staging::RobotToStage;
use crate::simulate_staging::StagedSimulationWorld;
use crate::simulate_staging::stage_simulation_world;
use crate::webots_stage_root;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::launch_plan::SIMULATOR_SUPERVISOR_PROVIDER_ID;
use phoxal_cli_core::project::launch_plan::simulator_controller_provider_id;
use phoxal_cli_core::project::resolver::ResolvedRobot;
use std::collections::BTreeMap;
use std::path::Path;

/// Stage a resolved robot and authored world into the Webots filesystem view.
pub(crate) fn stage_simulation_for_robot(
    project_root: &Path,
    world_source_path: &Path,
    resolved: &ResolvedRobot,
    launch_plan: &LaunchPlan,
) -> Result<StagedSimulationWorld> {
    // Wipe-and-restage per play: the staged root is a single, home-based
    // location shared across every `simulate` invocation (not project-scoped
    // any more), and Webots only ever runs one world per play, so a previous
    // play's stale worlds/protos/meshes/controllers must never linger. This
    // must run before any of this play's own staging below writes anything.
    webots_stage_root::wipe_and_recreate()?;

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
    let controller_id = simulator_controller_provider_id(robot_id);
    let controller_launch = robot
        .participants
        .iter()
        .find(|participant| participant.launch.participant_id == controller_id)
        .map(|participant| participant.launch.clone())
        .ok_or_else(|| {
            anyhow!("sim launch plan is missing the controller participant '{controller_id}'")
        })?;
    let supervisor_launch = robot
        .participants
        .iter()
        .find(|participant| participant.launch.participant_id == SIMULATOR_SUPERVISOR_PROVIDER_ID)
        .map(|participant| participant.launch.clone())
        .ok_or_else(|| {
            anyhow!(
                "sim launch plan is missing the supervisor participant '{SIMULATOR_SUPERVISOR_PROVIDER_ID}'"
            )
        })?;

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
        let crate_dir = component_assets_dir(component, project_root)?.ok_or_else(|| {
            anyhow!(
                "component instance '{}' (type '{}') has no resolved component_assets package; \
                 simulation needs its component.yaml/structure.urdf to stage the robot model. \
                 Passive components without an official assets package need a matching \
                 components/ workspace crate.",
                component.instance,
                component.source_name
            )
        })?;
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

    // `require_native` tells the supervisor whether it must resolve native
    // (packaged) controller/component artifacts rather than accepting a local
    // dev/path-overridden build; false whenever any simulator artifact is
    // path-overridden for local simulator development.
    let require_native = resolved
        .simulators
        .iter()
        .all(|runtime| runtime.source_path().is_none());

    let mesh_root = webots_stage_root::meshes_dir()?;
    // The Phase-6 mesh-staging gap: the generated PROTOs reference mesh assets
    // relative to `mesh_root` (the robot's own meshes directly under it, each
    // component's under `<mesh_root>/<component_type>/` per
    // `component_mesh_prefix`), but nothing copied the physical mesh files
    // there before this fix - the robot spawned with no visible geometry.
    // The robot's own meshes stay a real copy directly under `mesh_root`
    // (it shares that directory with every component's symlinked subdir, so
    // it cannot itself be a symlink); each component type's own `meshes/` is
    // symlinked instead - see `stage_component_meshes`.
    stage_robot_meshes(project_root, &resolved.robot.robot.structure, &mesh_root)?;
    for (component_type, source_dir) in &component_type_dirs {
        stage_component_meshes(source_dir, component_type, &mesh_root)?;
    }
    stage_simulation_world(
        &base_world_text,
        &webots_stage_root::protos_dir()?,
        &mesh_root,
        &webots_stage_root::world_path(world_name)?,
        supervisor_launch,
        require_native,
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
/// This stays a real COPY, not a symlink: `mesh_root` also hosts every
/// mounted component type's own symlinked `<component_type>/` subdirectory
/// side by side (see `stage_component_meshes`), so `mesh_root` itself must
/// remain a real directory the robot's own files sit in directly - there is
/// no single source directory a whole-`mesh_root` symlink could point at.
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

/// Stage one component type's `meshes/` directory (if any) as a SYMLINK at
/// `<mesh_root>/<component_type>/` pointing at the component's resolved mesh
/// source directory (the unpacked cached asset bundle's `meshes/` for a
/// suite component, or the local `components/<id>/meshes/` for a
/// path-pinned one - both already absolute, see `component_assets_dir`) - the
/// cache/path-pin stays the single source of truth instead of a copy. The
/// prefix `WebotsSceneDescription::from_component`'s `component_mesh_prefix`
/// embeds into the component's own mesh URDF references (`meshes/<file>` ->
/// `<component_type>/<file>`, see `staged_mesh_path_from_urdf_filename`), so
/// the generated PROTOs resolve through the symlinked directory exactly as
/// they would a copied one. `mesh_root` itself must already exist (see
/// `webots_stage_root::wipe_and_recreate`) - not every robot has its own
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
    require_absolute_symlink_target("component mesh source directory", &source)?;
    let dest = mesh_root.join(component_type);
    std::os::unix::fs::symlink(&source, &dest).with_context(|| {
        format!(
            "failed to symlink component meshes {} to staged path {}",
            source.display(),
            dest.display()
        )
    })
}

pub(crate) fn copy_dir_recursive(source: &Path, dest: &Path) -> Result<()> {
    for entry in std::fs::read_dir(source)
        .with_context(|| format!("failed to read mesh source directory {}", source.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let source_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if file_type.is_dir() {
            std::fs::create_dir_all(&dest_path).with_context(|| {
                format!(
                    "failed to create staged mesh directory {}",
                    dest_path.display()
                )
            })?;
            copy_dir_recursive(&source_path, &dest_path)?;
        } else if file_type.is_file() {
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
