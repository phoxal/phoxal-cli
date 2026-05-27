use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, bail};
use derive_new::new;
use phoxal_utils_component::Component as ComponentConfig;
use phoxal_utils_component::v1::capability::StructuralTarget;
use phoxal_utils_simulation::Simulation;
use phoxal_utils_structure::Structure;

use crate::AppContext;
use crate::unit::Unit;

use super::validate_mesh_paths_exist;

#[derive(Debug, Clone, new)]
pub struct Component {
    #[new(into)]
    component_type: String,
}

impl Unit for Component {
    type Output = ();

    fn name(&self) -> &'static str {
        "Validate Component"
    }

    fn execute(&self, app: &AppContext) -> anyhow::Result<Self::Output> {
        let component_dir = app.project.component_dir(&self.component_type);
        self.validate_dir(&component_dir)
    }

    fn run(&self, app: &AppContext) -> anyhow::Result<Self::Output> {
        app.ui.step(
            format!("Validate Component {}", self.component_type),
            || self.execute(app),
        )
    }
}

impl Component {
    pub fn validate_dir(&self, component_dir: &Path) -> anyhow::Result<()> {
        let component = ComponentConfig::read_from_dir(component_dir)?;
        let component = component
            .as_v1()
            .context("xtask validate only supports component.yaml version v1")?;

        let simulation = Simulation::read_from_dir(component_dir)?;
        let simulation = simulation
            .as_v1()
            .context("xtask validate only supports simulation.yaml version v1")?;
        let structure = Structure::read_from_dir(component_dir)?;

        structure.validate_fragment()?;
        component.validate()?;
        simulation.validate()?;

        let structure_link_ids = structure
            .links
            .iter()
            .map(|link| link.name.as_str())
            .collect::<BTreeSet<_>>();
        let structure_joint_ids = structure
            .joints
            .iter()
            .map(|joint| joint.name.as_str())
            .collect::<BTreeSet<_>>();

        for (capability_id, capability) in &component.capabilities {
            let target: &StructuralTarget = capability.target();
            match target {
                StructuralTarget::Joint { id } => {
                    if !structure_joint_ids.contains(id.as_str()) {
                        bail!(
                            "component '{}' capability '{}' targets missing joint '{}'",
                            self.component_type,
                            capability_id,
                            id
                        );
                    }
                }
                StructuralTarget::Link { id } => {
                    if !structure_link_ids.contains(id.as_str()) {
                        bail!(
                            "component '{}' capability '{}' targets missing link '{}'",
                            self.component_type,
                            capability_id,
                            id
                        );
                    }
                }
            }
        }

        for capability_id in simulation.capabilities.keys() {
            if !component.capabilities.contains_key(capability_id) {
                bail!(
                    "component '{}' simulation capability '{}' does not exist in component.yaml",
                    self.component_type,
                    capability_id
                );
            }
        }

        for link_id in simulation.links.keys() {
            if !structure_link_ids.contains(link_id.as_str()) {
                bail!(
                    "component '{}' simulation link '{}' does not exist in structure.urdf",
                    self.component_type,
                    link_id
                );
            }
        }

        validate_mesh_paths_exist(component_dir, &structure)?;

        Ok(())
    }
}
