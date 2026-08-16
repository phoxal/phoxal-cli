//! Static Webots world staging.
//!
//! `simulation::webots::proto` is a real, tested render engine (robot PROTO
//! generation, instance-node rendering, world staging) with no caller before
//! this module. This module wires it into the `simulate` launch flow: given
//! the resolved robot + the base world + each robot's controller launch
//! inputs, it stages a copy of the world that:
//!
//! - Declares an `EXTERNPROTO` for each robot's generated PROTO.
//! - Adds exactly one static robot instance.
//! - Assigns that robot the `phoxal-simulator-webots-controller` controller.
//! - Passes only the bundle root and the router endpoint. Execution identity,
//!   producer identity and the world timeline are controller-owned.
//!
//! The generated PROTO body lives in the same Webots project and the authored
//! world references it relatively.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use phoxal_model::Robot as RobotBundle;

use crate::simulation::webots::proto::{
    self, RobotInstance, WebotsController, externproto_for_generated_proto, generate_robot_proto,
    proto_name_for_robot, relative_mesh_url_prefix, render_robot_instance_node, stage_world,
};

/// The controller's complete launch contract.
///
/// There is no execution id and no participant id. A router's session id IS the
/// execution, so the controller asks the endpoint it dials; and it is not a
/// participant at all - it declares one liveliness token per component instance
/// it simulates. Both absences are why the world can be staged before the
/// supervisor is started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControllerLaunch {
    pub bundle_root: PathBuf,
    pub connect_endpoint: String,
}

/// The staged PROTO subdirectory the generated robot PROTO's own
/// `EXTERNPROTO` declarations point at for its mounted components
/// (`webots_staging/render/proto.rs` hardcodes `"components/{proto_name}.proto"`
/// relative to the robot PROTO file - so component PROTOs must physically
/// live in a `components/` subdirectory of the robot's own PROTO directory).
const COMPONENT_PROTOS_SUBDIR: &str = "components";

/// One distinct simulated component type compiled into the canonical robot.
pub struct ComponentTypeToStage<'a> {
    pub component_type: &'a str,
}

/// One robot to stage into the simulation world: its bundle (manifest +
/// component specs + structure), the distinct component types it mounts, and
/// the stable launch inputs for the controller that substitutes its
/// component-driver contracts. `component_types` may be empty for a robot
/// with no mounted components (or none with compiled simulation semantics).
pub struct RobotToStage<'a> {
    pub robot_id: String,
    pub bundle: &'a RobotBundle,
    pub component_types: Vec<ComponentTypeToStage<'a>>,
    pub controller_launch: ControllerLaunch,
}

/// Everything the caller needs to launch the staged world.
pub struct StagedSimulationWorld {
    pub staged_world_path: PathBuf,
}

/// Stage a simulation world for exactly one robot: generate its PROTO (plus
/// one PROTO per distinct component type), render one static controller-bearing
/// instance node, and stage the augmented world text.
///
/// `staged_protos_dir` is where each generated robot `.proto` file is
/// written; component PROTOs go under its `components/` subdirectory (the
/// relative path the robot PROTO's own `EXTERNPROTO` declarations point at,
/// `proto::render::proto::component_externprotos`). `mesh_root` is
/// the staged mesh directory (`root::meshes_dir`) each
/// generated PROTO's mesh asset references are resolved relative to.
/// `staged_world_path` is where the staged `.wbt` text is written.
pub fn stage_simulation_world(
    base_world_text: &str,
    staged_protos_dir: &Path,
    mesh_root: &Path,
    staged_world_path: &Path,
    robots: &[RobotToStage<'_>],
) -> Result<StagedSimulationWorld> {
    std::fs::create_dir_all(staged_protos_dir).with_context(|| {
        format!(
            "failed to create staged protos directory {}",
            staged_protos_dir.display()
        )
    })?;
    // `relative_mesh_url_prefix` canonicalizes `mesh_root`, so it must exist
    // before any PROTO's mesh URL prefix is computed - a first `simulate` run
    // may not have created it yet.
    std::fs::create_dir_all(mesh_root).with_context(|| {
        format!(
            "failed to create staged meshes directory {}",
            mesh_root.display()
        )
    })?;
    if let Some(parent) = staged_world_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create staged world directory {}",
                parent.display()
            )
        })?;
    }

    let mut extern_protos = Vec::with_capacity(robots.len());
    anyhow::ensure!(
        robots.len() == 1,
        "a Webots simulation world must contain exactly one robot"
    );
    let mut static_robot_nodes = Vec::with_capacity(1);

    let component_protos_dir = staged_protos_dir.join(COMPONENT_PROTOS_SUBDIR);
    std::fs::create_dir_all(&component_protos_dir).with_context(|| {
        format!(
            "failed to create staged component protos directory {}",
            component_protos_dir.display()
        )
    })?;

    for robot in robots {
        let proto_name = proto_name_for_robot(robot.bundle.id().as_str())?;
        let proto_path = staged_protos_dir.join(format!("{proto_name}.proto"));
        let mesh_url_prefix = relative_mesh_url_prefix(mesh_root, &proto_path)?;
        let component_solid_links = stage_component_protos(
            &component_protos_dir,
            mesh_root,
            robot.bundle,
            &robot.component_types,
        )
        .with_context(|| {
            format!(
                "failed to stage component PROTOs for robot '{}'",
                robot.robot_id
            )
        })?;

        let proto_text = generate_robot_proto(
            robot.bundle,
            robot.bundle.structure(),
            &component_solid_links,
            &proto_name,
            &mesh_url_prefix,
        )
        .with_context(|| format!("failed to generate PROTO for robot '{}'", robot.robot_id))?;
        std::fs::write(&proto_path, proto_text)
            .with_context(|| format!("failed to write staged PROTO {}", proto_path.display()))?;

        let extern_proto = externproto_for_generated_proto(&proto_path, staged_world_path)
            .with_context(|| {
                format!(
                    "failed to compute EXTERNPROTO reference for robot '{}'",
                    robot.robot_id
                )
            })?;
        extern_protos.push(extern_proto);

        let controller_args = controller_args(&robot.controller_launch)?;
        let instance = RobotInstance {
            proto_name: proto_name.clone(),
            def_name: proto_name.clone(),
            robot_id: robot.robot_id.clone(),
            controller: Some(WebotsController {
                controller_name: phoxal_cli_catalog::WEBOTS_CONTROLLER_PACKAGE.to_string(),
                controller_args,
            }),
            supervisor: Some(false),
            synchronization: None,
        };
        let instance_node = render_robot_instance_node(
            robot.bundle,
            robot.bundle.structure(),
            &component_solid_links,
            &instance,
        )
        .with_context(|| {
            format!(
                "failed to render instance node for robot '{}'",
                robot.robot_id
            )
        })?;
        static_robot_nodes.push(instance_node);
    }

    let staged_text = stage_world(base_world_text, &extern_protos, &static_robot_nodes)
        .context("failed to stage simulation world")?;
    std::fs::write(staged_world_path, staged_text).with_context(|| {
        format!(
            "failed to write staged world {}",
            staged_world_path.display()
        )
    })?;

    Ok(StagedSimulationWorld {
        staged_world_path: staged_world_path.to_path_buf(),
    })
}

fn controller_args(launch: &ControllerLaunch) -> Result<Vec<String>> {
    anyhow::ensure!(
        !launch.connect_endpoint.trim().is_empty(),
        "Webots controller requires the supervisor router endpoint"
    );
    Ok(vec![
        "--bundle-root".to_string(),
        launch.bundle_root.display().to_string(),
        "--connect".to_string(),
        launch.connect_endpoint.clone(),
    ])
}

/// Generate one PROTO per distinct component type the robot mounts, writing
/// each to `component_protos_dir` (the `components/` subdirectory the robot's
/// own generated PROTO's `EXTERNPROTO` declarations reference by relative
/// path). Returns the `component_solid_links` map
/// `generate_robot_proto`/`render_robot_instance_node` need, keyed by
/// component type (matching `WebotsSceneDescription::from_robot`'s
/// `component_solid_links.get(&model_component.component_type)` lookup).
fn stage_component_protos(
    component_protos_dir: &Path,
    mesh_root: &Path,
    bundle: &RobotBundle,
    component_types: &[ComponentTypeToStage<'_>],
) -> Result<BTreeMap<String, Vec<String>>> {
    let mut component_solid_links = BTreeMap::new();

    for component_type in component_types {
        // One mounted instance of the type is enough: the type's definition and
        // its simulation are the type's, not the instance's.
        let mounted = bundle
            .components()
            .find(|instance| {
                instance.instance().component_type().as_str() == component_type.component_type
            })
            .ok_or_else(|| {
                anyhow!(
                    "component type '{}' is not loaded in the robot bundle",
                    component_type.component_type
                )
            })?;
        let component_model = mounted.component_type();
        let comp_simulation = mounted.simulation().ok_or_else(|| {
            anyhow!(
                "component type '{}' has no compiled simulation semantics",
                component_type.component_type
            )
        })?;

        let comp_proto_name = proto_name_for_robot(component_type.component_type)?;
        let comp_proto_path = component_protos_dir.join(format!("{comp_proto_name}.proto"));
        let comp_mesh_url_prefix = relative_mesh_url_prefix(mesh_root, &comp_proto_path)?;

        let artifact = proto::generate_component_proto(
            component_type.component_type,
            component_model,
            component_model.structure(),
            comp_simulation,
            &comp_mesh_url_prefix,
        )
        .with_context(|| {
            format!(
                "failed to generate PROTO for component type '{}'",
                component_type.component_type
            )
        })?;
        std::fs::write(&comp_proto_path, artifact.proto_text).with_context(|| {
            format!(
                "failed to write staged component PROTO {}",
                comp_proto_path.display()
            )
        })?;
        component_solid_links.insert(
            component_type.component_type.to_string(),
            artifact.solid_link_ids,
        );
    }

    Ok(component_solid_links)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The controller's whole launch contract is the bundle and the endpoint.
    /// No identity is passed in: it learns the execution from the router, and
    /// it is not a participant, so there is no participant id to give it.
    #[test]
    fn controller_arguments_are_the_bundle_and_the_endpoint_only() -> Result<()> {
        let launch = ControllerLaunch {
            bundle_root: PathBuf::from("/runtime"),
            connect_endpoint: "tcp/127.0.0.1:7447".to_string(),
        };
        assert_eq!(
            controller_args(&launch)?,
            vec![
                "--bundle-root",
                "/runtime",
                "--connect",
                "tcp/127.0.0.1:7447",
            ]
        );
        Ok(())
    }
}
