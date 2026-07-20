//! Project-root path conventions and tooling used by CLI domain operations.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub mod tooling;

// Bundle-layout conventions. These were `pub` constants in `phoxal::runtime` up to
// framework 0.10.0; the 0.11 rewrite made them private to the framework's model
// internals, so the CLI owns its copies (as it already does for SIMULATION_FILE).
const COMPONENT_FILE: &str = "component.yaml";
const STRUCTURE_FILE: &str = "structure.urdf";
const MESHES_DIR: &str = "meshes";
const DEFAULT_ROBOT_NAMESPACE: &str = "dev";
const SIMULATION_FILE: &str = "simulation.yaml";

#[derive(Debug, Clone)]
pub struct Project {
    workspace_root: PathBuf,
}

impl Project {
    pub fn new(workspace_root: impl AsRef<Path>) -> Result<Self> {
        let workspace_root = normalize_existing_path(workspace_root.as_ref())?;
        Ok(Self { workspace_root })
    }

    pub fn root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn discover_robot_models(&self) -> Result<Vec<String>> {
        let models_dir = self.workspace_root.join("models");
        let mut models = Vec::new();
        if !models_dir.is_dir() {
            return Ok(models);
        }

        for entry in fs::read_dir(&models_dir)
            .with_context(|| format!("failed to read models directory {}", models_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if !path.join("Cargo.toml").is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            models.push(name.to_string());
        }
        models.sort();
        Ok(models)
    }

    pub fn model_dir(&self, robot_model: &str) -> PathBuf {
        let model_dir = self.workspace_root.join("models").join(robot_model);
        if model_dir.is_dir() {
            model_dir
        } else {
            self.scenario_fixture_root().join(robot_model)
        }
    }

    /// A fixture robot resolves under `fixture/robot/` rather than `models/`.
    pub fn is_fixture_robot(&self, robot_model: &str) -> bool {
        !self
            .workspace_root
            .join("models")
            .join(robot_model)
            .is_dir()
    }

    /// Framework-conformance fixture robots live outside the production model tree.
    pub fn scenario_fixture_root(&self) -> PathBuf {
        self.workspace_root.join("fixture").join("robot")
    }

    pub fn component_dir(&self, component_type: &str) -> PathBuf {
        self.workspace_root.join("components").join(component_type)
    }

    pub fn fixture_component_dir(&self, component_type: &str) -> PathBuf {
        self.workspace_root
            .join("fixture")
            .join("component")
            .join(component_type)
    }

    pub fn component_config_dir_for_model(
        &self,
        robot_model: &str,
        component_type: &str,
    ) -> PathBuf {
        let model_component_dir = self.model_component_dir(robot_model, component_type);
        if model_component_dir.join(COMPONENT_FILE).is_file() {
            model_component_dir
        } else if self
            .fixture_component_dir(component_type)
            .join(COMPONENT_FILE)
            .is_file()
        {
            self.fixture_component_dir(component_type)
        } else {
            self.component_dir(component_type)
        }
    }

    pub fn component_simulation_dir_for_model(
        &self,
        robot_model: &str,
        component_type: &str,
    ) -> PathBuf {
        let model_component_dir = self.model_component_dir(robot_model, component_type);
        if model_component_dir.join(SIMULATION_FILE).is_file() {
            model_component_dir
        } else if self
            .fixture_component_dir(component_type)
            .join(SIMULATION_FILE)
            .is_file()
        {
            self.fixture_component_dir(component_type)
        } else {
            self.component_dir(component_type)
        }
    }

    pub fn component_structure_dir_for_model(
        &self,
        robot_model: &str,
        component_type: &str,
    ) -> PathBuf {
        let model_component_dir = self.model_component_dir(robot_model, component_type);
        if model_component_dir.join(STRUCTURE_FILE).is_file() {
            model_component_dir
        } else if self
            .fixture_component_dir(component_type)
            .join(STRUCTURE_FILE)
            .is_file()
        {
            self.fixture_component_dir(component_type)
        } else {
            self.component_dir(component_type)
        }
    }

    pub fn component_mesh_dir(&self, robot_model: &str, component_type: &str) -> PathBuf {
        let model_component_dir = self.model_component_dir(robot_model, component_type);
        if model_component_dir.join(MESHES_DIR).is_dir() {
            model_component_dir.join(MESHES_DIR)
        } else if self
            .fixture_component_dir(component_type)
            .join(MESHES_DIR)
            .is_dir()
        {
            self.fixture_component_dir(component_type).join(MESHES_DIR)
        } else {
            self.component_dir(component_type).join(MESHES_DIR)
        }
    }

    pub fn component_package_manifest(&self, component_type: &str) -> PathBuf {
        self.component_dir(component_type).join("Cargo.toml")
    }

    fn model_component_dir(&self, robot_model: &str, component_type: &str) -> PathBuf {
        self.model_dir(robot_model)
            .join("components")
            .join(component_type)
    }

    pub fn webots_source_dir(&self) -> PathBuf {
        self.workspace_root.join("simulator").join("webots")
    }

    pub fn webots_openstreet_dir(&self) -> PathBuf {
        self.webots_source_dir().join("openstreet")
    }

    pub fn webots_world_source(&self, world_name: &str) -> PathBuf {
        self.webots_source_dir()
            .join("worlds")
            .join(format!("{world_name}.wbt"))
    }

    pub fn fixture_world_source(&self, world_name: &str) -> PathBuf {
        self.workspace_root
            .join("fixture")
            .join("world")
            .join(format!("{world_name}.wbt"))
    }

    pub fn webots_openstreet_map(&self, file_name: &str) -> PathBuf {
        self.webots_openstreet_dir().join(file_name)
    }

    pub fn dist_model_dir(&self, robot_model: &str) -> PathBuf {
        self.workspace_root
            .join("dist")
            .join("models")
            .join(robot_model)
    }

    pub fn dist_validation_scenario_dir(&self, selector: &str) -> PathBuf {
        self.workspace_root
            .join("dist")
            .join("validation")
            .join("scenario")
            .join(selector)
    }

    pub fn dev_robot_dir(&self, robot_hostname: &str) -> PathBuf {
        self.workspace_root
            .join("dist")
            .join(DEFAULT_ROBOT_NAMESPACE)
            .join(robot_hostname)
    }

    pub fn dev_log_dir(&self, robot_hostname: &str) -> PathBuf {
        self.dev_robot_dir(robot_hostname).join("logs")
    }

    // Webots' generated per-play staging lives under
    // `<project>/.phoxal/webots`; see `webots_stage_root`.
}

fn normalize_existing_path(path: &Path) -> Result<PathBuf> {
    std::env::current_dir()
        .context("failed to resolve current working directory")?
        .join(path)
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", path.display()))
}
