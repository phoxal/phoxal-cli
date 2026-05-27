use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use phoxal_engine::{DEFAULT_ROBOT_NAMESPACE, component_package_name, runtime_package_name};
use phoxal_engine::{ENV_COMPONENT_ID, ENV_ROBOT_CONFIG, ENV_ROBOT_SIMULATION, staged::Robot};
use phoxal_utils_robot::v1::{ConnectionConfig, LocalizeBackendKind, resolve_localize_backend};

use crate::Project;
use crate::unit::runtime_catalog::{PLATFORM_RUNTIME_SERVICES, is_platform_runtime};

const SHARED_BUILD_DOCKERFILE_PATH: &str = "phoxal-cli/docker/build.Dockerfile";
const BUNDLE_BUILD_DOCKERFILE_PATH: &str = "phoxal-cli/docker/bundle-build.Dockerfile";
const ORB_SLAM3_LOCALIZE_DOCKERFILE_PATH: &str = "runtimes/localize/orb-slam3-sys/Dockerfile";
const DEFAULT_RELEASE_TARGET_TRIPLE: &str = "aarch64-unknown-linux-gnu";
const DEFAULT_RELEASE_DOCKER_PLATFORM: &str = "linux/arm64";
const DEFAULT_LOCAL_X86_64_TARGET_TRIPLE: &str = "x86_64-unknown-linux-gnu";
const DEFAULT_LOCAL_AARCH64_TARGET_TRIPLE: &str = "aarch64-unknown-linux-gnu";
const DEFAULT_LOCAL_X86_64_DOCKER_PLATFORM: &str = "linux/amd64";
const DEFAULT_LOCAL_AARCH64_DOCKER_PLATFORM: &str = "linux/arm64";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildPlatform {
    pub target_triple: String,
    pub docker_platform: String,
}

impl BuildPlatform {
    pub fn new(target_triple: impl Into<String>, docker_platform: impl Into<String>) -> Self {
        Self {
            target_triple: target_triple.into(),
            docker_platform: docker_platform.into(),
        }
    }

    pub fn release_default() -> Self {
        Self::new(
            DEFAULT_RELEASE_TARGET_TRIPLE,
            DEFAULT_RELEASE_DOCKER_PLATFORM,
        )
    }

    pub fn local_default() -> Self {
        match std::env::consts::ARCH {
            "aarch64" | "arm64" => Self::new(
                DEFAULT_LOCAL_AARCH64_TARGET_TRIPLE,
                DEFAULT_LOCAL_AARCH64_DOCKER_PLATFORM,
            ),
            _ => Self::new(
                DEFAULT_LOCAL_X86_64_TARGET_TRIPLE,
                DEFAULT_LOCAL_X86_64_DOCKER_PLATFORM,
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableComponentDriver {
    pub component_id: String,
    pub component_type: String,
}

pub fn available_component_drivers(
    project: &Project,
    configuration: &Robot,
) -> Result<Vec<AvailableComponentDriver>> {
    configuration
        .model
        .components
        .iter()
        .filter(|(_, component)| component.driver.is_some())
        .map(|(component_id, component)| {
            let manifest = project.component_package_manifest(&component.component);
            if !manifest.is_file() {
                bail!(
                    "component '{}' provides driver config but {} does not exist",
                    component_id,
                    manifest.display()
                );
            }
            Ok(AvailableComponentDriver {
                component_id: component_id.clone(),
                component_type: component.component.clone(),
            })
        })
        .collect()
}

#[derive(Debug, Clone, Default)]
pub struct ContainerSelectionParams {
    pub enable_component_drivers: bool,
    pub enable_component_driver: Vec<String>,
    pub enable_component_driver_id: Vec<String>,
}

impl ContainerSelectionParams {
    pub fn resolve_for_local(self) -> ContainerServiceSelection {
        ContainerServiceSelection::new(
            self.enable_component_drivers,
            self.enable_component_driver,
            self.enable_component_driver_id,
        )
    }

    pub fn resolve_for_production(
        self,
        project: &Project,
        configuration: &Robot,
    ) -> Result<ContainerServiceSelection> {
        Ok(ContainerServiceSelection::new(
            self.enable_component_drivers,
            self.enable_component_driver,
            resolve_production_component_ids(
                self.enable_component_driver_id,
                available_component_drivers(project, configuration)?
                    .into_iter()
                    .map(|driver| driver.component_id)
                    .collect(),
            ),
        ))
    }
}

fn resolve_production_component_ids(
    explicit_component_ids: Vec<String>,
    all_component_ids: Vec<String>,
) -> Vec<String> {
    let mut enabled_component_ids = explicit_component_ids;
    enabled_component_ids.extend(all_component_ids);
    enabled_component_ids.sort();
    enabled_component_ids.dedup();
    enabled_component_ids
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerServiceSelection {
    enable_all_component_drivers: bool,
    enabled_component_types: Vec<String>,
    enabled_component_ids: Vec<String>,
    docker_excluded_runtime_names: Vec<String>,
}

impl ContainerServiceSelection {
    pub fn new(
        enable_all_component_drivers: bool,
        enabled_component_types: Vec<String>,
        enabled_component_ids: Vec<String>,
    ) -> Self {
        Self {
            enable_all_component_drivers,
            enabled_component_types,
            enabled_component_ids,
            docker_excluded_runtime_names: Vec::new(),
        }
    }

    pub fn with_docker_excluded_runtimes(
        mut self,
        docker_excluded_runtime_names: Vec<String>,
    ) -> Self {
        self.docker_excluded_runtime_names = docker_excluded_runtime_names;
        self
    }

    fn validate_docker_runtime_exclusions(&self, configuration: &Robot) -> Result<Vec<String>> {
        let mut docker_excluded_runtime_names = BTreeSet::new();
        for runtime_name in &self.docker_excluded_runtime_names {
            if !is_platform_runtime(runtime_name) || runtime_name == "router" {
                bail!("runtime '{runtime_name}' is not a supported runtime service");
            }
            docker_excluded_runtime_names.insert(runtime_name.clone());
        }
        let _ = configuration;
        Ok(docker_excluded_runtime_names.into_iter().collect())
    }

    pub fn resolve(
        &self,
        configuration: &Robot,
        available_component_drivers: &[AvailableComponentDriver],
    ) -> Result<ResolvedContainerServices> {
        let mut enabled_component_ids = BTreeSet::new();

        if self.enable_all_component_drivers {
            enabled_component_ids.extend(
                available_component_drivers
                    .iter()
                    .map(|driver| driver.component_id.clone()),
            );
        }

        for component_type in &self.enabled_component_types {
            let matches = available_component_drivers
                .iter()
                .filter(|driver| driver.component_type == *component_type)
                .map(|driver| driver.component_id.clone())
                .collect::<BTreeSet<_>>();
            if matches.is_empty() {
                bail!("component driver type '{component_type}' is not configured in model.yaml");
            }
            enabled_component_ids.extend(matches);
        }

        for component_id in &self.enabled_component_ids {
            if !available_component_drivers
                .iter()
                .any(|driver| driver.component_id == *component_id)
            {
                bail!("component driver '{component_id}' is not configured in model.yaml");
            }
            enabled_component_ids.insert(component_id.clone());
        }

        Ok(ResolvedContainerServices {
            enabled_component_ids: enabled_component_ids.into_iter().collect(),
            docker_excluded_runtime_names: self
                .validate_docker_runtime_exclusions(configuration)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedContainerServices {
    pub enabled_component_ids: Vec<String>,
    pub docker_excluded_runtime_names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedImagePlan {
    pub crate_dir: PathBuf,
    pub dockerfile_path: PathBuf,
    pub image: String,
    pub source: ManagedImageSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedImageSource {
    RustBinary {
        package_name: String,
        binary_name: String,
    },
    DockerContext {
        build_context: PathBuf,
        fingerprint_paths: Vec<PathBuf>,
    },
}

impl ManagedImagePlan {
    pub fn rust_binary(
        package_name: impl Into<String>,
        binary_name: impl Into<String>,
        crate_dir: PathBuf,
        dockerfile_path: PathBuf,
        image: impl Into<String>,
    ) -> Self {
        Self {
            crate_dir,
            dockerfile_path,
            image: image.into(),
            source: ManagedImageSource::RustBinary {
                package_name: package_name.into(),
                binary_name: binary_name.into(),
            },
        }
    }

    pub fn docker_context(
        crate_dir: PathBuf,
        dockerfile_path: PathBuf,
        image: impl Into<String>,
        build_context: PathBuf,
        fingerprint_paths: Vec<PathBuf>,
    ) -> Self {
        Self {
            crate_dir,
            dockerfile_path,
            image: image.into(),
            source: ManagedImageSource::DockerContext {
                build_context,
                fingerprint_paths,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServicePlan {
    pub service_name: String,
    pub image: String,
    pub managed_image: Option<ManagedImagePlan>,
    pub uses_supervisor_api: bool,
    pub devices: Vec<String>,
    pub component_id: Option<String>,
}

impl ServicePlan {
    pub fn uses_supervisor_api(&self) -> bool {
        self.uses_supervisor_api
    }

    pub fn environment(
        &self,
        config_path: &str,
        robot_id: Option<&str>,
        robot_namespace: Option<&str>,
        robot_hostname: Option<&str>,
        simulation: bool,
    ) -> Vec<(String, String)> {
        let mut environment = vec![
            (ENV_ROBOT_CONFIG.to_string(), config_path.to_string()),
            (
                "ROBOT_ROUTER_ENDPOINT".to_string(),
                "tcp/router:7447".to_string(),
            ),
        ];

        if simulation {
            environment.push((ENV_ROBOT_SIMULATION.to_string(), true.to_string()));
        }

        if let Some(robot_id) = robot_id {
            environment.push(("ROBOT_ID".to_string(), robot_id.to_string()));
        }
        if let Some(robot_namespace) = robot_namespace {
            environment.push(("ROBOT_NAMESPACE".to_string(), robot_namespace.to_string()));
        }
        if let Some(robot_hostname) = robot_hostname {
            environment.push(("ROBOT_HOSTNAME".to_string(), robot_hostname.to_string()));
        }
        if let Some(component_id) = &self.component_id {
            environment.push((ENV_COMPONENT_ID.to_string(), component_id.clone()));
        }

        environment
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundlePlan {
    pub image: String,
    pub dockerfile_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwarePlan {
    pub image: String,
    pub dockerfile_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouterPlan {
    pub image: String,
    pub managed_image: ManagedImagePlan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopologyPlan {
    pub services: Vec<ServicePlan>,
    pub bundle: BundlePlan,
    pub firmware: Option<FirmwarePlan>,
    pub router: RouterPlan,
}

impl TopologyPlan {
    pub fn managed_images(&self) -> Vec<&ManagedImagePlan> {
        std::iter::once(&self.router.managed_image)
            .chain(
                self.services
                    .iter()
                    .filter_map(|service| service.managed_image.as_ref()),
            )
            .collect()
    }

    pub fn firmware_managed_image(&self) -> Option<ManagedImagePlan> {
        self.firmware.as_ref().map(|firmware| {
            let crate_dir = firmware
                .dockerfile_path
                .parent()
                .unwrap_or(&firmware.dockerfile_path)
                .to_path_buf();
            ManagedImagePlan::docker_context(
                crate_dir.clone(),
                firmware.dockerfile_path.clone(),
                firmware.image.clone(),
                crate_dir,
                vec![firmware.dockerfile_path.clone()],
            )
        })
    }
}

pub fn plan_local_topology(
    project: &Project,
    robot_model: &str,
    configuration: &Robot,
    selection: &ContainerServiceSelection,
) -> Result<TopologyPlan> {
    plan_topology(
        project,
        robot_model,
        configuration,
        selection,
        "local",
        DEFAULT_ROBOT_NAMESPACE,
    )
}

pub fn plan_deploy_topology(
    project: &Project,
    robot_model: &str,
    configuration: &Robot,
    selection: &ContainerServiceSelection,
    registry: &str,
    tag: &str,
) -> Result<TopologyPlan> {
    plan_topology(
        project,
        robot_model,
        configuration,
        selection,
        registry,
        tag,
    )
}

fn plan_topology(
    project: &Project,
    robot_model: &str,
    configuration: &Robot,
    selection: &ContainerServiceSelection,
    registry: &str,
    tag: &str,
) -> Result<TopologyPlan> {
    let component_drivers = available_component_drivers(project, configuration)?;
    let selection = selection.resolve(configuration, &component_drivers)?;
    let mut services = Vec::new();
    let workspace_root = project.root();
    let localize_uses_orb_slam3_image = matches!(
        resolve_localize_backend(&configuration.model, &configuration.components).kind(),
        LocalizeBackendKind::OrbSlam3Rgbd | LocalizeBackendKind::OrbSlam3RgbdInertial,
    );

    for service in PLATFORM_RUNTIME_SERVICES
        .iter()
        .filter(|service| service.name != "router")
    {
        let runtime_name = service.name;
        if selection
            .docker_excluded_runtime_names
            .iter()
            .any(|excluded| excluded == runtime_name)
        {
            continue;
        }

        let service_name = runtime_name.to_string();
        let package_name = runtime_package_name(runtime_name);
        let crate_dir = workspace_root.join("runtimes").join(runtime_name);
        let dockerfile_path = dockerfile_path(workspace_root, &crate_dir)?;
        let image = image_ref(registry, &package_name, tag);

        let managed_image = Some(runtime_managed_image(
            runtime_name,
            package_name.clone(),
            crate_dir,
            dockerfile_path,
            image.clone(),
            workspace_root,
            localize_uses_orb_slam3_image,
        ));

        services.push(ServicePlan {
            service_name,
            image,
            managed_image,
            uses_supervisor_api: service.uses_supervisor_api,
            devices: Vec::new(),
            component_id: None,
        });
    }

    for component_id in &selection.enabled_component_ids {
        let component_instance = configuration
            .model
            .components
            .get(component_id)
            .with_context(|| {
                format!("selected component driver '{component_id}' is not defined in model.yaml")
            })?;
        let driver = component_instance.driver.as_ref().with_context(|| {
            format!("selected component '{component_id}' has no driver config in model.yaml")
        })?;
        let package_name = component_package_name(&component_instance.component);
        let crate_dir = project.component_dir(&component_instance.component);
        let dockerfile_path = dockerfile_path(workspace_root, &crate_dir)?;
        let image = driver
            .image
            .clone()
            .unwrap_or_else(|| image_ref(registry, &package_name, tag));
        let devices = devices_for_connection(&driver.connection);

        let managed_image = driver.image.is_none().then(|| {
            ManagedImagePlan::rust_binary(
                package_name.clone(),
                package_name,
                crate_dir,
                dockerfile_path,
                image.clone(),
            )
        });

        services.push(ServicePlan {
            service_name: component_id.clone(),
            image,
            managed_image,
            uses_supervisor_api: false,
            devices,
            component_id: Some(component_id.clone()),
        });
    }

    let bundle = BundlePlan {
        image: image_ref(registry, &format!("robot-bundle-{robot_model}"), tag),
        dockerfile_path: bundle_dockerfile_path(workspace_root)?,
    };
    let router_name = "router";
    let router_package_name = runtime_package_name(router_name);
    let router_crate_dir = workspace_root.join("runtimes").join(router_name);
    let router_dockerfile_path = dockerfile_path(workspace_root, &router_crate_dir)?;
    let router_image = image_ref(registry, &router_package_name, tag);
    let router = RouterPlan {
        image: router_image.clone(),
        managed_image: ManagedImagePlan::rust_binary(
            router_package_name.clone(),
            router_package_name,
            router_crate_dir,
            router_dockerfile_path,
            router_image,
        ),
    };

    let firmware_dockerfile_path =
        workspace_root.join("phoxal-cli/docker/jetson-firmware.build.Dockerfile");
    let firmware = firmware_dockerfile_path.exists().then(|| FirmwarePlan {
        image: image_ref(registry, "jetson-firmware", tag),
        dockerfile_path: firmware_dockerfile_path,
    });

    Ok(TopologyPlan {
        services,
        bundle,
        firmware,
        router,
    })
}

fn runtime_managed_image(
    runtime_name: &str,
    package_name: String,
    crate_dir: PathBuf,
    dockerfile_path: PathBuf,
    image: String,
    workspace_root: &Path,
    use_orb_slam3_image: bool,
) -> ManagedImagePlan {
    if runtime_name == "localize" && use_orb_slam3_image {
        ManagedImagePlan::docker_context(
            crate_dir,
            workspace_root.join(ORB_SLAM3_LOCALIZE_DOCKERFILE_PATH),
            image,
            workspace_root.to_path_buf(),
            vec![
                workspace_root.join("runtimes/localize/orb-slam3-sys"),
                workspace_root.join("runtimes/localize/src"),
            ],
        )
    } else {
        ManagedImagePlan::rust_binary(
            package_name.clone(),
            package_name,
            crate_dir,
            dockerfile_path,
            image,
        )
    }
}

fn image_ref(registry: &str, image_name: &str, tag: &str) -> String {
    format!("{}/{image_name}:{tag}", registry.trim_end_matches('/'))
}

fn dockerfile_path(workspace_root: &Path, crate_dir: &Path) -> Result<PathBuf> {
    let specific_path = crate_dir.join("build.Dockerfile");
    if specific_path.exists() {
        return Ok(specific_path);
    }

    let shared_path = workspace_root.join(SHARED_BUILD_DOCKERFILE_PATH);
    if shared_path.exists() {
        return Ok(shared_path);
    }

    bail!(
        "missing service Dockerfile at {} and shared workspace Dockerfile at {}; selected services must map to an explicit build definition",
        specific_path.display(),
        shared_path.display()
    );
}

fn bundle_dockerfile_path(workspace_root: &Path) -> Result<PathBuf> {
    let dockerfile_path = workspace_root.join(BUNDLE_BUILD_DOCKERFILE_PATH);
    if dockerfile_path.exists() {
        return Ok(dockerfile_path);
    }

    bail!(
        "missing bundle Dockerfile at {}; bundle images must map to an explicit build definition",
        dockerfile_path.display()
    );
}

fn devices_for_connection(connection: &ConnectionConfig) -> Vec<String> {
    match connection {
        ConnectionConfig::I2c { bus, .. } => {
            let device = format!("/dev/i2c-{bus}");
            vec![format!("{device}:{device}")]
        }
        ConnectionConfig::Spi {
            bus, chip_select, ..
        } => {
            let device = format!("/dev/spidev{bus}.{chip_select}");
            vec![format!("{device}:{device}")]
        }
        ConnectionConfig::Serial { port, .. } | ConnectionConfig::Uart { port, .. } => {
            vec![format!("{port}:{port}")]
        }
        ConnectionConfig::Usb { .. } => vec!["/dev/bus/usb:/dev/bus/usb".to_string()],
        ConnectionConfig::Gpio { chip, .. } => {
            let device = if chip.starts_with("/dev/") {
                chip.clone()
            } else {
                format!("/dev/{chip}")
            };
            vec![format!("{device}:{device}")]
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn localize_plan(use_orb_slam3_image: bool) -> ManagedImagePlan {
        let workspace_root = PathBuf::from("/repo");
        runtime_managed_image(
            "localize",
            "phoxal-runtime-localize".to_string(),
            workspace_root.join("runtimes/localize"),
            workspace_root.join(SHARED_BUILD_DOCKERFILE_PATH),
            "local/phoxal-runtime-localize:dev".to_string(),
            &workspace_root,
            use_orb_slam3_image,
        )
    }

    #[test]
    fn localize_uses_orb_slam3_docker_context_when_resolved() {
        let plan = localize_plan(true);

        assert!(
            plan.dockerfile_path
                .ends_with(ORB_SLAM3_LOCALIZE_DOCKERFILE_PATH)
        );
        match plan.source {
            ManagedImageSource::DockerContext {
                build_context,
                fingerprint_paths,
            } => {
                assert_eq!(build_context, PathBuf::from("/repo"));
                assert_eq!(
                    fingerprint_paths,
                    vec![
                        PathBuf::from("/repo/runtimes/localize/orb-slam3-sys"),
                        PathBuf::from("/repo/runtimes/localize/src"),
                    ]
                );
            }
            ManagedImageSource::RustBinary { .. } => {
                panic!("resolved ORB-SLAM3 localize must use a Docker context image")
            }
        }
    }

    #[test]
    fn localize_uses_rust_binary_by_default() {
        let plan = localize_plan(false);

        match plan.source {
            ManagedImageSource::RustBinary {
                package_name,
                binary_name,
            } => {
                assert_eq!(package_name, "phoxal-runtime-localize");
                assert_eq!(binary_name, "phoxal-runtime-localize");
            }
            ManagedImageSource::DockerContext { .. } => {
                panic!("default localize image must use the shared Rust binary path")
            }
        }
    }

    #[test]
    fn fixture_robot_resolves_to_orb_slam3_image() {
        let robot = fixture_robot();

        assert!(matches!(
            resolve_localize_backend(&robot.model, &robot.components).kind(),
            LocalizeBackendKind::OrbSlam3RgbdInertial,
        ));
    }

    #[test]
    fn fixture_robot_without_imu_role_resolves_to_orb_slam3_rgbd_image() {
        let mut robot = fixture_robot();
        component_roles_mut(&mut robot, "imu").remove("imu");

        assert!(matches!(
            resolve_localize_backend(&robot.model, &robot.components).kind(),
            LocalizeBackendKind::OrbSlam3Rgbd,
        ));
    }

    #[test]
    fn fixture_robot_without_depth_role_resolves_to_dead_reckoning() {
        let mut robot = fixture_robot();
        component_roles_mut(&mut robot, "front_camera").remove("depth");

        assert!(matches!(
            resolve_localize_backend(&robot.model, &robot.components).kind(),
            LocalizeBackendKind::DeadReckoning,
        ));
    }

    fn fixture_robot() -> Robot {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = manifest_dir
            .parent()
            .unwrap_or_else(|| {
                panic!(
                    "phoxal-cli-core CARGO_MANIFEST_DIR must live one level below the workspace root: {}",
                    manifest_dir.display()
                )
            });
        let bundle_root = workspace_root
            .join("fixture")
            .join("robot")
            .join("rgbd-imu-diff-drive");

        let project = Project::new(workspace_root).unwrap_or_else(|error| {
            panic!(
                "failed to create project from {}: {error:#}",
                workspace_root.display()
            )
        });
        let model = phoxal_utils_robot::Model::read_from_dir(&bundle_root)
            .unwrap_or_else(|error| {
                panic!(
                    "failed to read fixture model from {}: {error:#}",
                    bundle_root.display()
                )
            })
            .as_v1()
            .expect("fixture model must be v1")
            .clone();
        let components = model
            .used_component_types()
            .into_iter()
            .map(|component_type| {
                let component_dir =
                    project.component_config_dir_for_model("rgbd-imu-diff-drive", component_type);
                let component = phoxal_utils_component::Component::read_from_dir(&component_dir)
                    .unwrap_or_else(|error| {
                        panic!(
                            "failed to read fixture component {component_type} from {}: {error:#}",
                            component_dir.display()
                        )
                    })
                    .as_v1()
                    .expect("fixture component must be v1")
                    .clone();
                (component_type.to_string(), component)
            })
            .collect::<BTreeMap<_, _>>();

        Robot { model, components }
    }

    fn component_roles_mut<'a>(
        robot: &'a mut Robot,
        component_id: &str,
    ) -> &'a mut std::collections::BTreeMap<String, Vec<phoxal_utils_robot::v1::Role>> {
        match robot.model.components.get_mut(component_id) {
            Some(component) => &mut component.roles,
            None => panic!("fixture missing {component_id} component instance"),
        }
    }
}
