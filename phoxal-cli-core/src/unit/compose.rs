use std::collections::BTreeMap;

use anyhow::Result;
use phoxal_engine::RobotIdentity;
use serde::Serialize;

use crate::AppContext;
use crate::unit::Unit;
use crate::unit::container::TopologyPlan;

const CONTAINER_WORKSPACE_ROOT: &str = "/workspace";
const BUNDLE_SERVICE_NAME: &str = "bundle";
const WORKSPACE_VOLUME_NAME: &str = "workspace";
const BALENA_COMPOSE_VERSION: &str = "2.4";
const ROUTER_SERVICE_NAME: &str = "router";
const ENV_LOCALIZE_BACKEND: &str = "ROBOT_LOCALIZE_BACKEND";
const ENV_ROBOT_ID: &str = "ROBOT_ID";
const ENV_ROBOT_NAMESPACE: &str = "ROBOT_NAMESPACE";
const ENV_ROBOT_HOSTNAME: &str = "ROBOT_HOSTNAME";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposeMode {
    Local {
        identity: RobotIdentity,
        rust_log: String,
    },
    Deploy,
}

#[derive(Debug, Clone)]
pub struct GenerateCompose {
    pub topology: TopologyPlan,
    pub mode: ComposeMode,
}

impl Unit for GenerateCompose {
    type Output = String;

    fn name(&self) -> &'static str {
        "Generate Compose"
    }

    fn execute(&self, _app: &AppContext) -> Result<Self::Output> {
        Ok(serde_yaml::to_string(&ComposeFile::from_plan(
            &self.topology,
            &self.mode,
        ))?)
    }
}

#[derive(Debug, Serialize)]
struct ComposeFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    services: BTreeMap<String, ComposeService>,
    volumes: BTreeMap<String, serde_yaml::Value>,
}

impl ComposeFile {
    fn from_plan(topology: &TopologyPlan, mode: &ComposeMode) -> Self {
        let mut services = BTreeMap::new();
        let mut bundle = ComposeService::new(topology.bundle.image.clone());
        bundle.labels = supervisor_api_labels();
        bundle.volumes = vec![format!(
            "{WORKSPACE_VOLUME_NAME}:{CONTAINER_WORKSPACE_ROOT}"
        )];
        bundle.restart = "no".to_string();
        services.insert(BUNDLE_SERVICE_NAME.to_string(), bundle);

        if matches!(mode, ComposeMode::Deploy)
            && let Some(firmware) = &topology.firmware
        {
            let mut firmware_service = ComposeService::new(firmware.image.clone());
            firmware_service.network_mode = Some("host".to_string());
            firmware_service.init = true;
            firmware_service.labels = BTreeMap::from([(
                "io.balena.features.extra-firmware".to_string(),
                "1".to_string(),
            )]);
            services.insert("firmware".to_string(), firmware_service);
        }

        services.insert(
            ROUTER_SERVICE_NAME.to_string(),
            router_service(topology, mode),
        );

        for service in &topology.services {
            let environment = match mode {
                ComposeMode::Local {
                    identity, rust_log, ..
                } => {
                    let host_name = identity.host_name();
                    let mut environment = BTreeMap::from_iter(service.environment(
                        CONTAINER_WORKSPACE_ROOT,
                        Some(identity.robot_id.as_str()),
                        Some(identity.robot_namespace.as_str()),
                        Some(host_name.as_str()),
                        true,
                    ));
                    environment.insert("RUST_LOG".to_string(), rust_log.clone());
                    if let Ok(localize_backend) = std::env::var(ENV_LOCALIZE_BACKEND) {
                        environment.insert(ENV_LOCALIZE_BACKEND.to_string(), localize_backend);
                    }
                    Environment::Map(environment)
                }
                ComposeMode::Deploy => {
                    let mut environment = service
                        .environment(CONTAINER_WORKSPACE_ROOT, None, None, None, false)
                        .into_iter()
                        .map(|(name, value)| format!("{name}={value}"))
                        .collect::<Vec<_>>();
                    environment.extend([
                        ENV_ROBOT_ID.to_string(),
                        ENV_ROBOT_NAMESPACE.to_string(),
                        ENV_ROBOT_HOSTNAME.to_string(),
                    ]);
                    Environment::List(environment)
                }
            };

            let depends_on = vec![
                BUNDLE_SERVICE_NAME.to_string(),
                ROUTER_SERVICE_NAME.to_string(),
            ];

            let mut compose_service = ComposeService::new(service.image.clone());
            compose_service.labels = supervisor_api_labels();
            compose_service.init = true;
            compose_service.volumes = vec![format!(
                "{WORKSPACE_VOLUME_NAME}:{CONTAINER_WORKSPACE_ROOT}:ro"
            )];
            compose_service.devices = service.devices.clone();
            compose_service.environment = Some(environment);
            compose_service.depends_on = depends_on;
            services.insert(service.service_name.clone(), compose_service);
        }

        Self {
            version: matches!(mode, ComposeMode::Deploy)
                .then(|| BALENA_COMPOSE_VERSION.to_string()),
            services,
            volumes: BTreeMap::from([(
                WORKSPACE_VOLUME_NAME.to_string(),
                serde_yaml::Value::Mapping(Default::default()),
            )]),
        }
    }
}

#[derive(Debug, Serialize)]
struct ComposeService {
    image: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    network_mode: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    labels: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    volumes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    devices: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    environment: Option<Environment>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    extra_hosts: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    ports: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    entrypoint: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    command: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    depends_on: Vec<String>,
    restart: String,
    /// Run the service under an init (tini) as PID 1 so the runtime binary becomes a
    /// child: this gives correct signal forwarding + zombie reaping, and a real crash
    /// of the runtime exits the container (not a manual stop) so `restart: always`
    /// fires. Only emitted when true.
    #[serde(skip_serializing_if = "is_not_init")]
    init: bool,
}

fn is_not_init(value: &bool) -> bool {
    !*value
}

impl ComposeService {
    fn new(image: String) -> Self {
        Self {
            image,
            network_mode: None,
            labels: BTreeMap::new(),
            volumes: Vec::new(),
            devices: Vec::new(),
            environment: None,
            extra_hosts: Vec::new(),
            ports: Vec::new(),
            entrypoint: Vec::new(),
            command: Vec::new(),
            depends_on: Vec::new(),
            restart: "always".to_string(),
            init: false,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum Environment {
    Map(BTreeMap<String, String>),
    List(Vec<String>),
}

fn router_service(topology: &TopologyPlan, mode: &ComposeMode) -> ComposeService {
    let mut service = ComposeService::new(topology.router.image.clone());
    service.labels = supervisor_api_labels();
    service.depends_on = vec![BUNDLE_SERVICE_NAME.to_string()];

    if let ComposeMode::Local { .. } = mode {
        service.extra_hosts = vec!["host.docker.internal:host-gateway".to_string()];
        service.ports = vec!["7447:7447".to_string()];
    }

    service
}

fn supervisor_api_labels() -> BTreeMap<String, String> {
    BTreeMap::from([(
        "io.balena.features.supervisor-api".to_string(),
        "1".to_string(),
    )])
}
