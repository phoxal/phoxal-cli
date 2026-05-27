use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use phoxal_engine::staged::Robot;
use phoxal_utils_conventions::{COMPONENT_FILE, COMPONENTS_DIR, MODEL_FILE};
use phoxal_utils_robot::Model;
use phoxal_utils_robot::v1::{self, SourceBundle, resolve_source_bundle};
use phoxal_utils_structure::Structure;

use crate::AppContext;
use crate::Project;
use crate::unit::runtime_catalog::PLATFORM_RUNTIME_NAMES;
use crate::unit::source_graph::validate_model_components;
use crate::unit::stream_demand::validate_runtime_stream_demands;
use crate::unit::validate::component::Component;
use crate::unit::validate::validate_mesh_paths_exist;

#[derive(Debug, Clone)]
pub struct ValidatedRobot {
    pub robot_model: String,
    pub model: v1::ModelV1,
    pub resolved_facts: v1::ResolvedFacts,
    pub base_structure: Structure,
}

impl ValidatedRobot {
    pub fn load(app: &AppContext, robot_model: &str) -> Result<Self> {
        let model_dir = app.project.model_dir(robot_model);
        let model = Model::read_from_dir(&model_dir)?;
        let model = model
            .as_v1()
            .context("xtask only supports model.yaml version v1")?
            .clone();
        let base_structure = Structure::read_from_dir(&model_dir)?;
        base_structure.validate()?;

        validate_mesh_paths_exist(&model_dir, &base_structure)?;
        validate_distinct_components_once(app, robot_model, &model)?;
        let components = load_used_components(app, robot_model, &model)?;
        validate_model_components(
            &model,
            &base_structure,
            |component_type| {
                phoxal_utils_component::Component::read_from_dir(
                    app.project
                        .component_config_dir_for_model(robot_model, component_type),
                )
            },
            |component_type| {
                app.project
                    .component_package_manifest(component_type)
                    .is_file()
            },
            app.project.is_fixture_robot(robot_model),
        )?;

        let resolved_facts =
            resolve_source_bundle(SourceBundle::new(model.clone(), components.clone()))?;
        for warning in validate_runtime_stream_demands(
            &model,
            &components,
            PLATFORM_RUNTIME_NAMES,
            resolved_facts.localize_backend,
            &resolved_facts.roles,
        )? {
            app.ui.warn(warning);
        }

        Ok(Self {
            robot_model: robot_model.to_string(),
            model,
            resolved_facts,
            base_structure,
        })
    }

    pub fn stage_bundle(&self, app: &AppContext, output: Option<&Path>) -> Result<Robot> {
        app.ui.info(format!(
            "Creating bundle payload for '{}'",
            self.robot_model
        ));

        let bundle_root = output
            .map(Path::to_path_buf)
            .unwrap_or_else(|| app.project.bundle_dir(&self.robot_model));
        if bundle_root.exists() {
            fs::remove_dir_all(&bundle_root).with_context(|| {
                format!(
                    "failed to remove existing bundle staging {}",
                    bundle_root.display()
                )
            })?;
        }
        fs::create_dir_all(&bundle_root).with_context(|| {
            format!(
                "failed to create bundle directory {}",
                bundle_root.display()
            )
        })?;

        copy_model_config(&app.project, &self.robot_model, &bundle_root)?;
        let mut assembled_structure = self.base_structure.clone();
        for (component_id, instance) in &self.model.components {
            let component_structure = Structure::read_from_dir(
                app.project
                    .component_structure_dir_for_model(&self.robot_model, &instance.component),
            )
            .with_context(|| {
                format!(
                    "failed to read structure.urdf for component '{}' mounted as '{}'",
                    instance.component, component_id
                )
            })?;
            assembled_structure = assembled_structure
                .with_mounted_component(component_id, &instance.mount_link, &component_structure)
                .with_context(|| {
                    format!(
                        "failed to mount component '{}' of type '{}' on link '{}'",
                        component_id, instance.component, instance.mount_link
                    )
                })?;
        }
        assembled_structure
            .validate()
            .context("assembled bundle structure.urdf is invalid")?;
        assembled_structure.write_to_dir(&bundle_root)?;
        copy_component_configs(&app.project, &self.robot_model, &self.model, &bundle_root)?;

        app.ui
            .success(format!("Bundle root: {}", bundle_root.display()));

        Robot::read_from_dir(&bundle_root)
    }
}

fn load_used_components(
    app: &AppContext,
    robot_model: &str,
    model: &v1::ModelV1,
) -> Result<BTreeMap<String, phoxal_utils_component::v1::Component>> {
    model
        .used_component_types()
        .into_iter()
        .map(|component_type| {
            let component = phoxal_utils_component::Component::read_from_dir(
                app.project
                    .component_config_dir_for_model(robot_model, component_type),
            )?
            .as_v1()
            .context("xtask only supports component.yaml version v1")?
            .clone();
            Ok((component_type.to_string(), component))
        })
        .collect()
}

fn validate_distinct_components_once(
    app: &AppContext,
    robot_model: &str,
    model: &v1::ModelV1,
) -> Result<()> {
    let mut validated_component_types = BTreeSet::new();
    for (component_id, component_instance) in &model.components {
        if validated_component_types.insert(component_instance.component.clone()) {
            let component_dir = app
                .project
                .component_config_dir_for_model(robot_model, &component_instance.component);
            app.ui
                .step(
                    format!("Validate Component {}", component_instance.component),
                    || {
                        Component::new(component_instance.component.clone())
                            .validate_dir(&component_dir)
                    },
                )
                .with_context(|| {
                    format!(
                        "component '{}' of type '{}' is invalid",
                        component_id, component_instance.component
                    )
                })?;
        }
    }
    Ok(())
}

fn copy_model_config(project: &Project, robot_model: &str, bundle_root: &Path) -> Result<()> {
    fs::copy(
        project.model_dir(robot_model).join(MODEL_FILE),
        bundle_root.join(MODEL_FILE),
    )
    .with_context(|| format!("failed to copy {MODEL_FILE} for '{robot_model}'"))?;
    Ok(())
}

fn copy_component_configs(
    project: &Project,
    robot_model: &str,
    model: &v1::ModelV1,
    bundle_root: &Path,
) -> Result<()> {
    let components_root = bundle_root.join(COMPONENTS_DIR);
    for component_type in model.used_component_types() {
        let source = project
            .component_config_dir_for_model(robot_model, component_type)
            .join(COMPONENT_FILE);
        let destination_dir = components_root.join(component_type);
        fs::create_dir_all(&destination_dir).with_context(|| {
            format!(
                "failed to create component bundle directory {}",
                destination_dir.display()
            )
        })?;
        fs::copy(&source, destination_dir.join(COMPONENT_FILE)).with_context(|| {
            format!(
                "failed to copy {COMPONENT_FILE} for '{}' from {}",
                component_type,
                source.display()
            )
        })?;
    }
    Ok(())
}
