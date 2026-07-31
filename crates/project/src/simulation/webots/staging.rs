//! Filesystem staging for generated simulation worlds and meshes.

use crate::simulation::prepare::ComponentTypeToStage;
use crate::simulation::prepare::ControllerLaunch;
use crate::simulation::prepare::RobotToStage;
use crate::simulation::prepare::StagedSimulationWorld;
use crate::simulation::prepare::stage_simulation_world;
use crate::simulation::webots::root;
use anyhow::Context;
use anyhow::Result;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::resolver::BundlePlan;
use std::path::Path;

/// Stage a resolved robot and authored world into the Webots filesystem view.
pub(crate) fn stage_simulation_for_robot(
    world_source_path: &Path,
    resolved: &BundlePlan,
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
    let bundle = resolved
        .compiled
        .decode_robot()
        .context("failed to decode the canonical robot for Webots staging")?;
    let robot_id = bundle.robot_id();
    anyhow::ensure!(
        !connect_endpoints.is_empty(),
        "Webots controller requires the resident router endpoint"
    );
    let controller_launch = ControllerLaunch {
        namespace: robot.namespace.clone(),
        robot_id: robot_id.to_string(),
        bundle_root: Some(runtime_root.to_path_buf()),
        connect_endpoints: connect_endpoints.to_vec(),
    };

    let component_types = bundle
        .components()
        .filter(|instance| {
            bundle
                .simulation_for_component_type(instance.component_type())
                .is_some()
        })
        .map(|instance| instance.component_type())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(|component_type| ComponentTypeToStage { component_type })
        .collect::<Vec<_>>();

    let mesh_root = root::meshes_dir()?;
    stage_compiled_geometry_assets(&bundle, &resolved.compiled.assets, &mesh_root)?;
    stage_simulation_world(
        &base_world_text,
        &root::protos_dir()?,
        &mesh_root,
        &root::world_path(world_name)?,
        &[RobotToStage {
            robot_id: robot_id.to_string(),
            bundle: &bundle,
            component_types,
            controller_launch,
        }],
    )
}

fn stage_compiled_geometry_assets(
    robot: &phoxal_model::Robot,
    assets: &std::collections::BTreeMap<phoxal_manifest::AssetId, Vec<u8>>,
    mesh_root: &Path,
) -> Result<()> {
    let canonical_root = mesh_root
        .canonicalize()
        .context("failed to resolve staged Webots mesh root")?;
    let mut referenced = robot
        .structure()
        .asset_ids()
        .collect::<std::collections::BTreeSet<_>>();
    for instance in robot.components() {
        let component = robot
            .component_for_instance(instance.id())
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
            .get(id)
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
        std::fs::write(&destination, bytes)
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
    assets: impl IntoIterator<Item = &'a phoxal_manifest::AssetId>,
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
    use phoxal_cli_core::project::resolver::CompiledBundle;

    #[test]
    fn stages_referenced_geometry_outside_the_legacy_mesh_prefix() -> Result<()> {
        let source = tempfile::tempdir()?;
        let component = source.path().join("components/wheel");
        std::fs::create_dir_all(source.path().join("meshes/geometry"))?;
        std::fs::create_dir_all(&component)?;
        std::fs::write(
            source.path().join("robot.yaml"),
            r#"schema: robot/v0
robot:
  id: geometry-bot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  structure: structure.urdf
  kinematic:
    kind: omnidirectional
    actuators: [wheel.motor]
    encoders: []
  components:
    wheel:
      component: wheel
      mount_link: base
"#,
        )?;
        std::fs::write(
            source.path().join("structure.urdf"),
            r#"<robot name="geometry-bot"><link name="base_footprint"/><link name="base_link"/><link name="base"><visual><geometry><mesh filename="package://robot/geometry/body.stl"/></geometry></visual></link><joint name="root" type="fixed"><parent link="base_footprint"/><child link="base_link"/></joint><joint name="mount" type="fixed"><parent link="base_link"/><child link="base"/></joint></robot>"#,
        )?;
        std::fs::write(
            source.path().join("meshes/geometry/body.stl"),
            b"solid body\nendsolid body\n",
        )?;
        std::fs::write(
            component.join("component.yaml"),
            "schema: component/v0\ncapabilities:\n  motor:\n    kind: motor\n    command: velocity\n    target:\n      kind: joint\n      id: wheel_joint\n",
        )?;
        std::fs::write(
            component.join("structure.urdf"),
            r#"<robot name="wheel"><link name="base"/><link name="wheel"/><joint name="wheel_joint" type="continuous"><parent link="base"/><child link="wheel"/></joint></robot>"#,
        )?;
        let mut compiled =
            CompiledBundle::from_project(phoxal_manifest::compile(phoxal_manifest::SourceSet {
                project_root: source.path().to_path_buf(),
                robot_manifest: source.path().join("robot.yaml"),
                component_roots: std::collections::BTreeMap::from([(
                    "wheel".to_string(),
                    component,
                )]),
            })?)?;
        // Exercise the staging rule independently of today's compiler naming
        // convention: a future canonical model may reference a normalized
        // logical asset outside `meshes/`, and staging must follow the model
        // reference rather than filter by a hard-coded prefix.
        let old_id = phoxal_manifest::AssetId::new("meshes/robot/geometry/body.stl".to_string())?;
        let bytes = compiled
            .assets
            .remove(&old_id)
            .context("fixture compiler omitted geometry asset")?;
        compiled.assets.insert(
            phoxal_manifest::AssetId::new("geometry/body.stl".to_string())?,
            bytes,
        );
        let canonical = String::from_utf8(compiled.robot)?
            .replace("meshes/robot/geometry/body.stl", "geometry/body.stl");
        compiled.robot = canonical.into_bytes();
        let robot = compiled.decode_robot()?;
        let mesh_root = tempfile::tempdir()?;
        stage_compiled_geometry_assets(&robot, &compiled.assets, mesh_root.path())?;
        assert_eq!(
            std::fs::read(mesh_root.path().join("geometry/body.stl"))?,
            b"solid body\nendsolid body\n"
        );
        Ok(())
    }

    #[test]
    fn distinct_geometry_ids_cannot_overwrite_one_staged_path() -> Result<()> {
        let prefixed = phoxal_manifest::AssetId::new("meshes/wheel/body.stl".to_string())?;
        let unprefixed = phoxal_manifest::AssetId::new("wheel/body.stl".to_string())?;
        let error = validate_unique_geometry_destinations([&prefixed, &unprefixed])
            .expect_err("non-injective geometry paths must fail before staging")
            .to_string();
        assert!(error.contains(prefixed.as_str()), "{error}");
        assert!(error.contains(unprefixed.as_str()), "{error}");
        assert!(error.contains("same staged path"), "{error}");
        Ok(())
    }
}
