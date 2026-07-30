use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use phoxal::model::Robot;
use phoxal::model::component::capability::{Capability as PhysicalCapability, StructuralTarget};
use phoxal::model::simulation::Simulation;
use phoxal::model::simulation::capability::Capability as SimulationCapability;
use phoxal::model::structure::Structure;

use crate::simulation::webots::proto::support::urdf::convert_joint_type;
use crate::simulation::webots::proto::{metadata, proto_name_for_robot};

#[derive(Debug, Clone)]
pub struct WebotsSceneDescription {
    pub robot_name: String,
    pub root_link_id: String,
    pub links: BTreeMap<String, urdf_rs::Link>,
    pub contact_materials: BTreeMap<String, String>,
    pub joints: BTreeMap<String, urdf_rs::Joint>,
    pub runtime_components_for_joint: BTreeMap<String, Vec<RuntimeComponentBinding>>,
    pub runtime_components_for_link: BTreeMap<String, Vec<RuntimeComponentBinding>>,
    pub mounted_components_for_link: BTreeMap<String, Vec<ComponentProtoInstance>>,
    pub component_mesh_prefix: Option<String>,
}

impl WebotsSceneDescription {
    pub fn from_robot(
        configuration: &Robot,
        structure: &Structure,
        component_solid_links: &BTreeMap<String, Vec<String>>,
    ) -> Result<Self> {
        let root_link_id = structure.root_link_name()?.to_string();
        let links = structure
            .links
            .iter()
            .cloned()
            .map(|link| (link.name.clone(), link))
            .collect::<BTreeMap<_, _>>();
        let joints = structure
            .joints
            .iter()
            .cloned()
            .map(|joint| {
                convert_joint_type(&joint.joint_type)?;
                Ok((joint.name.clone(), joint))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;

        let mounted_components_for_link = configuration
            .components()
            .iter()
            .map(|(component_id, model_component)| {
                let component = configuration.component_for_instance(component_id)?;
                let capability_names = component
                    .capabilities
                    .iter()
                    .flat_map(|(capability_id, capability)| {
                        let mut names = vec![(
                            capability_id.clone(),
                            format!("{component_id}.{capability_id}"),
                        )];
                        if matches!(capability, PhysicalCapability::Imu(_)) {
                            for device_id in Self::imu_device_capability_ids(capability_id) {
                                names.push((
                                    device_id.clone(),
                                    format!("{component_id}.{device_id}"),
                                ));
                            }
                        }
                        names
                    })
                    .collect::<BTreeMap<_, _>>();
                Ok((
                    model_component.mount_link.clone(),
                    ComponentProtoInstance {
                        proto_name: proto_name_for_robot(&model_component.component_type)?,
                        capability_names,
                        solid_names: component_solid_links
                            .get(&model_component.component_type)
                            .into_iter()
                            .flat_map(|link_ids| link_ids.iter())
                            .map(|link_id| (link_id.clone(), format!("{component_id}__{link_id}")))
                            .collect(),
                    },
                ))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .fold(
                BTreeMap::<String, Vec<ComponentProtoInstance>>::new(),
                |mut acc, item| {
                    acc.entry(item.0).or_default().push(item.1);
                    acc
                },
            );

        Ok(Self {
            robot_name: configuration.robot_id().to_string(),
            root_link_id,
            links,
            contact_materials: BTreeMap::new(),
            joints,
            runtime_components_for_joint: BTreeMap::new(),
            runtime_components_for_link: BTreeMap::new(),
            mounted_components_for_link,
            component_mesh_prefix: None,
        })
    }

    pub fn from_component(
        component_type: &str,
        structure: &Structure,
        component: &phoxal::model::component::Component,
        simulation: &Simulation,
    ) -> Result<Self> {
        let root_link_id = structure.root_link_name()?.to_string();
        let links = structure
            .links
            .iter()
            .cloned()
            .map(|link| (link.name.clone(), link))
            .collect::<BTreeMap<_, _>>();
        let contact_materials = simulation
            .links
            .iter()
            .filter_map(|(link_name, link)| {
                link.contact_material
                    .as_ref()
                    .map(|material| (link_name.clone(), material.clone()))
            })
            .collect::<BTreeMap<_, _>>();
        let joints = structure
            .joints
            .iter()
            .cloned()
            .map(|joint| {
                convert_joint_type(&joint.joint_type)?;
                Ok((joint.name.clone(), joint))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;

        let mut runtime_components_for_joint = BTreeMap::new();
        let mut runtime_components_for_link = BTreeMap::new();

        for (capability_id, capability) in &component.capabilities {
            let binding = RuntimeComponentBinding {
                capability_id: capability_id.clone(),
                physical: capability.clone(),
                simulation: simulation.capabilities.get(capability_id).cloned(),
            };
            match capability.target() {
                StructuralTarget::Joint { id } => {
                    runtime_components_for_joint
                        .entry(id.clone())
                        .or_insert_with(Vec::new)
                        .push(binding);
                }
                StructuralTarget::Link { id } => {
                    runtime_components_for_link
                        .entry(id.clone())
                        .or_insert_with(Vec::new)
                        .push(binding);
                }
            }
        }

        Ok(Self {
            robot_name: component_type.to_string(),
            root_link_id,
            links,
            contact_materials,
            joints,
            runtime_components_for_joint,
            runtime_components_for_link,
            mounted_components_for_link: BTreeMap::new(),
            component_mesh_prefix: Some(component_type.to_string()),
        })
    }

    pub fn runtime_metadata_comments(&self) -> String {
        metadata::runtime_metadata_comments(self)
    }

    pub fn capability_name_field_name(capability_id: &str) -> String {
        format!("capability_name__{capability_id}")
    }

    pub fn imu_accelerometer_capability_id(capability_id: &str) -> String {
        format!("{capability_id}__accel")
    }

    pub fn imu_gyroscope_capability_id(capability_id: &str) -> String {
        format!("{capability_id}__gyro")
    }

    pub fn imu_device_capability_ids(capability_id: &str) -> [String; 2] {
        [
            Self::imu_accelerometer_capability_id(capability_id),
            Self::imu_gyroscope_capability_id(capability_id),
        ]
    }

    pub fn imu_accelerometer_name_field_name(capability_id: &str) -> String {
        Self::capability_name_field_name(&Self::imu_accelerometer_capability_id(capability_id))
    }

    pub fn imu_gyroscope_name_field_name(capability_id: &str) -> String {
        Self::capability_name_field_name(&Self::imu_gyroscope_capability_id(capability_id))
    }

    pub fn solid_name_field_name(link_id: &str) -> String {
        format!("solid_name__{link_id}")
    }

    pub fn child_joints<'a>(
        &'a self,
        parent_link_id: &'a str,
    ) -> impl Iterator<Item = &'a urdf_rs::Joint> {
        self.joints
            .values()
            .filter(move |joint| joint.parent.link == parent_link_id)
    }

    pub fn runtime_components_for_joint<'a>(
        &'a self,
        joint_id: &'a str,
    ) -> impl Iterator<Item = &'a RuntimeComponentBinding> {
        self.runtime_components_for_joint
            .get(joint_id)
            .into_iter()
            .flat_map(|bindings| bindings.iter())
    }

    pub fn runtime_components_for_link<'a>(
        &'a self,
        link_id: &'a str,
    ) -> impl Iterator<Item = &'a RuntimeComponentBinding> {
        self.runtime_components_for_link
            .get(link_id)
            .into_iter()
            .flat_map(|bindings| bindings.iter())
    }

    pub fn joint_bindings<'a>(&'a self, joint_id: &'a str) -> Vec<&'a RuntimeComponentBinding> {
        self.runtime_components_for_joint(joint_id).collect()
    }

    pub fn link_bindings<'a>(&'a self, link_id: &'a str) -> Vec<&'a RuntimeComponentBinding> {
        self.runtime_components_for_link(link_id).collect()
    }

    pub fn capability_name_field_ids(&self) -> BTreeSet<String> {
        self.runtime_components_for_joint
            .values()
            .chain(self.runtime_components_for_link.values())
            .flat_map(|bindings| {
                bindings.iter().flat_map(|binding| {
                    let mut ids = vec![binding.capability_id.clone()];
                    if matches!(&binding.physical, PhysicalCapability::Imu(_)) {
                        ids.extend(Self::imu_device_capability_ids(&binding.capability_id));
                    }
                    ids
                })
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeComponentBinding {
    pub capability_id: String,
    pub physical: phoxal::model::component::capability::Capability,
    pub simulation: Option<SimulationCapability>,
}

#[derive(Debug, Clone)]
pub struct ComponentProtoInstance {
    pub proto_name: String,
    pub capability_names: BTreeMap<String, String>,
    pub solid_names: BTreeMap<String, String>,
}
