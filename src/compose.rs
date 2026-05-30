use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::catalog::PlatformRuntimeCatalog;
use crate::local_zenoh::{LOCAL_ZENOH_NETWORK, LOCAL_ZENOH_PORT};
use crate::resolver::ResolvedRobot;

const ROBOT_MOUNT: &str = "/robot";
const ROUTER_SERVICE: &str = "router";

#[derive(Debug, Clone, Serialize)]
struct ComposeFile {
    name: String,
    services: BTreeMap<String, ComposeService>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    networks: BTreeMap<String, NetworkSpec>,
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
    command: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    volumes: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    environment: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "DependsOn::is_empty")]
    depends_on: DependsOn,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    ports: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    networks: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    healthcheck: Option<Healthcheck>,
    #[serde(skip_serializing_if = "is_false")]
    init: bool,
    restart: String,
}

#[derive(Debug, Clone, Serialize)]
struct Healthcheck {
    test: Vec<String>,
    interval: String,
    timeout: String,
    retries: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum DependsOn {
    Services(Vec<String>),
    Conditions(BTreeMap<String, DependsOnCondition>),
}

impl DependsOn {
    fn is_empty(&self) -> bool {
        match self {
            Self::Services(services) => services.is_empty(),
            Self::Conditions(conditions) => conditions.is_empty(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct DependsOnCondition {
    condition: String,
}

#[derive(Debug, Clone, Serialize)]
struct NetworkSpec {
    external: bool,
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
    let runtime_environment = || {
        BTreeMap::from([
            ("ROBOT_CONFIG".to_string(), ROBOT_MOUNT.to_string()),
            (
                "ROBOT_ID".to_string(),
                resolved.robot.identity.id.to_string(),
            ),
            (
                "ROBOT_NAMESPACE".to_string(),
                resolved.robot.identity.namespace.to_string(),
            ),
            (
                "ROBOT_ROUTER_ENDPOINT".to_string(),
                "tcp/router:7447".to_string(),
            ),
            ("ROBOT_SIMULATION".to_string(), "true".to_string()),
        ])
    };
    for runtime in &resolved.platform_runtimes {
        let Some(entry) = catalog.lookup(&runtime.name) else {
            continue;
        };
        let service = ComposeService {
            image: runtime.deploy_ref(),
            command: if entry.wires_to_router {
                vec!["run".to_string()]
            } else {
                Vec::new()
            },
            volumes: vec![run_mount.clone()],
            environment: if entry.wires_to_router {
                runtime_environment()
            } else {
                BTreeMap::new()
            },
            depends_on: if entry.wires_to_router {
                router_healthy_depends_on()
            } else {
                DependsOn::Services(Vec::new())
            },
            ports: Vec::new(),
            networks: Vec::new(),
            healthcheck: None,
            init: true,
            restart: "unless-stopped".to_string(),
        };
        services.insert(runtime.name.clone(), service);
    }

    services.insert(ROUTER_SERVICE.to_string(), router_service());

    for runtime in &resolved.user_runtimes {
        let image = user_runtime_images
            .get(&runtime.name)
            .cloned()
            .unwrap_or_else(|| runtime.image_tag.clone());
        services.insert(
            format!("user-{}", runtime.name),
            ComposeService {
                image,
                command: vec!["run".to_string()],
                volumes: vec![run_mount.clone()],
                environment: runtime_environment(),
                depends_on: router_healthy_depends_on(),
                ports: Vec::new(),
                networks: Vec::new(),
                healthcheck: None,
                init: true,
                restart: "unless-stopped".to_string(),
            },
        );
    }

    Ok(serde_yaml::to_string(&ComposeFile {
        name: resolved.robot.identity.id.clone(),
        services,
        networks: BTreeMap::from([(
            LOCAL_ZENOH_NETWORK.to_string(),
            NetworkSpec { external: true },
        )]),
        native_tools: native_tools.to_vec(),
    })?)
}

fn router_service() -> ComposeService {
    ComposeService {
        image: crate::local_zenoh::zenoh_image(),
        command: vec![
            "-l".to_string(),
            format!("tcp/0.0.0.0:{LOCAL_ZENOH_PORT}"),
            "-e".to_string(),
            format!(
                "tcp/{}:{LOCAL_ZENOH_PORT}",
                crate::local_zenoh::LOCAL_ZENOH_CONTAINER
            ),
            "--no-multicast-scouting".to_string(),
            "--cfg".to_string(),
            "mode:\"router\"".to_string(),
        ],
        volumes: Vec::new(),
        environment: BTreeMap::new(),
        depends_on: DependsOn::Services(Vec::new()),
        ports: Vec::new(),
        networks: vec!["default".to_string(), LOCAL_ZENOH_NETWORK.to_string()],
        healthcheck: Some(Healthcheck {
            test: vec![
                "CMD-SHELL".to_string(),
                "grep -q ':1D17 .* 0A' /proc/net/tcp".to_string(),
            ],
            interval: "1s".to_string(),
            timeout: "2s".to_string(),
            retries: 60,
        }),
        init: true,
        restart: "unless-stopped".to_string(),
    }
}

fn router_healthy_depends_on() -> DependsOn {
    DependsOn::Conditions(BTreeMap::from([(
        ROUTER_SERVICE.to_string(),
        DependsOnCondition {
            condition: "service_healthy".to_string(),
        },
    )]))
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::SystemTime;

    use phoxal_core_robot::RobotV1 as Robot;
    use semver::Version;
    use serde_yaml::{Mapping, Value};

    use super::*;
    use crate::catalog::{PlatformRuntimeCatalog, PlatformRuntimeEntry};
    use crate::local_zenoh;
    use crate::resolver::{ImagePin, ResolvedPlatformRuntime, ResolvedRobot};

    static TEST_CATALOG_ENTRIES: &[PlatformRuntimeEntry] = &[PlatformRuntimeEntry {
        name: "asset",
        image_repo: "ghcr.io/phoxal/runtime-asset",
        uses_supervisor_api: false,
        wires_to_router: true,
    }];

    static TEST_CATALOG: PlatformRuntimeCatalog = PlatformRuntimeCatalog {
        supported_runtimes_version_req: "*",
        entries: TEST_CATALOG_ENTRIES,
    };

    #[test]
    fn platform_runtimes_use_runtime_cli_contract() -> anyhow::Result<()> {
        let resolved = resolved_robot()?;
        let compose = generate(
            &resolved,
            &TEST_CATALOG,
            &PathBuf::from("/tmp/phoxal/run"),
            &BTreeMap::new(),
            &[],
        )?;
        let compose: Value = serde_yaml::from_str(&compose)?;
        let root = mapping(&compose, "compose")?;
        assert_eq!(root.get(key("name")), Some(&key("testbot")));
        let services = mapping(root.get(key("services")).expect("services"), "services")?;
        let router = mapping(service(services, "router")?, "router")?;
        let asset = mapping(service(services, "asset")?, "asset")?;

        assert_eq!(
            router.get(key("image")),
            Some(&key(local_zenoh::DEFAULT_ZENOH_IMAGE))
        );
        assert_eq!(router.get(key("ports")), None);
        assert_eq!(
            router.get(key("networks")),
            Some(&Value::Sequence(vec![key("default"), key("phoxal-link")]))
        );
        assert_eq!(
            router.get(key("command")),
            Some(&Value::Sequence(vec![
                key("-l"),
                key("tcp/0.0.0.0:7447"),
                key("-e"),
                key("tcp/phoxal-local-zenoh:7447"),
                key("--no-multicast-scouting"),
                key("--cfg"),
                key("mode:\"router\""),
            ]))
        );
        assert_eq!(router.get(key("environment")), None);
        let healthcheck = mapping(
            router.get(key("healthcheck")).expect("router healthcheck"),
            "router healthcheck",
        )?;
        assert_eq!(
            healthcheck.get(key("test")),
            Some(&Value::Sequence(vec![
                key("CMD-SHELL"),
                key("grep -q ':1D17 .* 0A' /proc/net/tcp"),
            ]))
        );

        assert_eq!(
            asset.get(key("command")),
            Some(&Value::Sequence(vec![key("run")]))
        );
        let depends_on = mapping(
            asset.get(key("depends_on")).expect("asset depends_on"),
            "asset depends_on",
        )?;
        let router_depends_on = mapping(
            depends_on.get(key("router")).expect("router dependency"),
            "router dependency",
        )?;
        assert_eq!(
            router_depends_on.get(key("condition")),
            Some(&key("service_healthy"))
        );
        let environment = mapping(
            asset.get(key("environment")).expect("asset environment"),
            "asset environment",
        )?;
        assert_eq!(environment.get(key("ROBOT_CONFIG")), Some(&key("/robot")));
        assert_eq!(
            environment.get(key("ROBOT_ROUTER_ENDPOINT")),
            Some(&key("tcp/router:7447"))
        );
        assert_eq!(environment.get(key("ROBOT_ID")), Some(&key("testbot")));
        assert_eq!(environment.get(key("ROBOT_NAMESPACE")), Some(&key("test")));
        let networks = mapping(root.get(key("networks")).expect("networks"), "networks")?;
        let link_network = mapping(
            networks
                .get(key("phoxal-link"))
                .expect("phoxal-link network"),
            "phoxal-link network",
        )?;
        assert_eq!(link_network.get(key("external")), Some(&Value::Bool(true)));

        Ok(())
    }

    fn resolved_robot() -> anyhow::Result<ResolvedRobot> {
        Ok(ResolvedRobot {
            robot: Robot::parse_from_string(MINIMAL_ROBOT)?,
            runtime_set_version: Version::parse("0.0.0-dev")?,
            requested_runtime_set: "0.0.0-dev".to_string(),
            releases_fetched_at: Some(SystemTime::UNIX_EPOCH),
            platform_runtimes: vec![resolved_platform_runtime(
                "asset",
                "ghcr.io/phoxal/runtime-asset",
            )],
            user_runtimes: Vec::new(),
            components: Vec::new(),
            tools: Vec::new(),
        })
    }

    fn resolved_platform_runtime(name: &str, image_repo: &str) -> ResolvedPlatformRuntime {
        ResolvedPlatformRuntime {
            name: name.to_string(),
            image_repo: image_repo.to_string(),
            version: Version::parse("0.0.0-dev").expect("valid test version"),
            pin: ImagePin::Digest(format!("sha256:{name}")),
        }
    }

    fn service<'a>(services: &'a Mapping, name: &str) -> anyhow::Result<&'a Value> {
        services
            .get(key(name))
            .ok_or_else(|| anyhow::anyhow!("missing service {name}"))
    }

    fn mapping<'a>(value: &'a Value, label: &str) -> anyhow::Result<&'a Mapping> {
        value
            .as_mapping()
            .ok_or_else(|| anyhow::anyhow!("{label} is not a mapping"))
    }

    fn key(value: &str) -> Value {
        Value::String(value.to_string())
    }

    const MINIMAL_ROBOT: &str = r#"version: v1

phoxal:
  cli_min_version: "^0.1"

identity:
  id: testbot
  namespace: test

structure: structure.urdf

phoxal_runtimes:
  version: "latest"

motion:
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5

components:
  sources: {}
  instances: {}
"#;
}
