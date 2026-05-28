use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::catalog::PlatformRuntimeCatalog;
use crate::resolver::ResolvedRobot;

const ROBOT_MOUNT: &str = "/robot";
const ROUTER_PORT: &str = "127.0.0.1:7447:7447";

#[derive(Debug, Clone, Serialize)]
struct ComposeFile {
    services: BTreeMap<String, ComposeService>,
    #[serde(
        rename = "x-phoxal-native-tools",
        skip_serializing_if = "Vec::is_empty"
    )]
    native_tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ComposeService {
    image: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    volumes: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    environment: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    depends_on: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    ports: Vec<String>,
    #[serde(skip_serializing_if = "is_false")]
    init: bool,
    restart: String,
}

pub fn generate(
    resolved: &ResolvedRobot,
    catalog: &PlatformRuntimeCatalog,
    run_dir: &Path,
    user_runtime_images: &BTreeMap<String, String>,
    native_tools: &[String],
) -> Result<String> {
    let run_mount = format!("{}:{ROBOT_MOUNT}:ro", run_dir.display());
    let mut services = BTreeMap::new();
    for runtime in &resolved.platform_runtimes {
        let Some(entry) = catalog.lookup(&runtime.name) else {
            continue;
        };
        let mut environment = entry
            .default_env
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect::<BTreeMap<_, _>>();
        environment.insert("PHOXAL_RUNTIME_NAME".to_string(), runtime.name.clone());
        environment.insert(
            "PHOXAL_BUS_ROUTER".to_string(),
            "tcp/localhost:7447".to_string(),
        );
        let mut service = ComposeService {
            image: runtime.pinned_image(),
            volumes: vec![run_mount.clone()],
            environment,
            depends_on: if runtime.name == "router" {
                Vec::new()
            } else {
                vec!["router".to_string()]
            },
            ports: Vec::new(),
            init: true,
            restart: "unless-stopped".to_string(),
        };
        if runtime.name == "router" {
            service.ports.push(ROUTER_PORT.to_string());
        }
        services.insert(runtime.name.clone(), service);
    }

    for runtime in &resolved.user_runtimes {
        let image = user_runtime_images
            .get(&runtime.name)
            .cloned()
            .unwrap_or_else(|| runtime.image_tag.clone());
        services.insert(
            format!("user-{}", runtime.name),
            ComposeService {
                image,
                volumes: vec![run_mount.clone()],
                environment: BTreeMap::from([
                    ("PHOXAL_ROBOT_DIR".to_string(), ROBOT_MOUNT.to_string()),
                    ("PHOXAL_RUNTIME_NAME".to_string(), runtime.name.clone()),
                    (
                        "PHOXAL_BUS_ROUTER".to_string(),
                        "tcp/host.docker.internal:7447".to_string(),
                    ),
                ]),
                depends_on: vec!["router".to_string()],
                ports: Vec::new(),
                init: true,
                restart: "unless-stopped".to_string(),
            },
        );
    }

    Ok(serde_yaml::to_string(&ComposeFile {
        services,
        native_tools: native_tools.to_vec(),
    })?)
}

fn is_false(value: &bool) -> bool {
    !*value
}
