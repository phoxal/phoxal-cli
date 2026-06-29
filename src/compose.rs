use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::catalog::PlatformRuntimeCatalog;
use crate::local_zenoh::{LOCAL_ZENOH_NETWORK, LOCAL_ZENOH_PORT};
use crate::resolver::{ResolvedRobot, RobotManifestExtras};

const ROBOT_MOUNT: &str = "/robot";
const ROUTER_SERVICE: &str = "router";
const ROUTER_ENDPOINT: &str = "tcp/router:7447";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchClock {
    Real,
    Simulation,
}

impl LaunchClock {
    fn as_env(self) -> &'static str {
        match self {
            Self::Real => "real",
            Self::Simulation => "simulation",
        }
    }
}

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
    manifest_extras: &RobotManifestExtras,
    clock: LaunchClock,
) -> Result<String> {
    let run_mount = format!("{}:{ROBOT_MOUNT}:ro", run_dir.display());
    // The router's `listen` binding is deploy infra (a CLI-side default), not a
    // manifest profile field; only `connect` is materialized from the profile.
    let router_listen = format!("tcp/0.0.0.0:{LOCAL_ZENOH_PORT}");
    let bus_profile = manifest_extras.materialized_bus_profile(ROUTER_ENDPOINT);
    let mut services = BTreeMap::new();
    let runtime_environment = |connect: &str| {
        BTreeMap::from([
            (
                "PHOXAL_NAMESPACE".to_string(),
                resolved.robot.identity.namespace.to_string(),
            ),
            (
                "PHOXAL_ROBOT_ID".to_string(),
                resolved.robot.identity.id.to_string(),
            ),
            ("PHOXAL_BUNDLE_ROOT".to_string(), ROBOT_MOUNT.to_string()),
            ("PHOXAL_CONNECT".to_string(), connect.to_string()),
            ("PHOXAL_CLOCK".to_string(), clock.as_env().to_string()),
        ])
    };
    let user_runtime_environment = |name: &str| {
        let mut environment = runtime_environment(&bus_profile.connect);
        environment.insert("PHOXAL_PARTICIPANT_ID".to_string(), name.to_string());
        environment.insert(
            "PHOXAL_CONFIG".to_string(),
            manifest_extras
                .user_runtime_config(name)
                .map_or_else(|| "{}".to_string(), serde_json::Value::to_string),
        );
        environment
    };
    for runtime in &resolved.platform_runtimes {
        let Some(entry) = catalog.lookup(&runtime.name) else {
            continue;
        };
        let connect = if entry.wires_to_router {
            bus_profile.connect.as_str()
        } else {
            ""
        };
        let service = ComposeService {
            image: runtime.deploy_ref(),
            command: if entry.wires_to_router {
                vec!["run".to_string()]
            } else {
                Vec::new()
            },
            volumes: vec![run_mount.clone()],
            environment: runtime_environment(connect),
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

    services.insert(ROUTER_SERVICE.to_string(), router_service(&router_listen));

    for runtime in &resolved.user_runtimes {
        let image = user_runtime_images
            .get(&runtime.name)
            .cloned()
            .unwrap_or_else(|| runtime.image.clone());
        services.insert(
            format!("user-{}", runtime.name),
            ComposeService {
                image,
                command: vec!["run".to_string()],
                volumes: vec![run_mount.clone()],
                environment: user_runtime_environment(&runtime.name),
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

fn router_service(listen: &str) -> ComposeService {
    ComposeService {
        image: crate::local_zenoh::zenoh_image(),
        command: vec![
            "-l".to_string(),
            listen.to_string(),
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

    use phoxal::model::robot::RobotV1 as Robot;
    use phoxal::model::robot::v1::Channel;
    use serde_yaml::{Mapping, Value};

    use super::*;
    use crate::catalog::{PlatformRuntimeCatalog, PlatformRuntimeEntry};
    use crate::local_zenoh;
    use crate::resolver::{
        BusManifestExtras, BusProfileConfig, ImagePin, ResolvedPlatformRuntime, ResolvedRobot,
        ResolvedUserRuntime, RobotManifestExtras, UserRuntimeManifestExtras,
    };

    static TEST_CATALOG_ENTRIES: &[PlatformRuntimeEntry] = &[
        PlatformRuntimeEntry {
            name: "asset",
            api_versions: &["y2026_1"],
            uses_supervisor_api: false,
            wires_to_router: true,
        },
        PlatformRuntimeEntry {
            name: "diagnostics",
            api_versions: &["y2026_1"],
            uses_supervisor_api: false,
            wires_to_router: false,
        },
    ];

    static TEST_CATALOG: PlatformRuntimeCatalog = PlatformRuntimeCatalog {
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
            &manifest_extras(),
            LaunchClock::Simulation,
        )?;
        let compose: Value = serde_yaml::from_str(&compose)?;
        let root = mapping(&compose, "compose")?;
        assert_eq!(root.get(key("name")), Some(&key("testbot")));
        let services = mapping(root.get(key("services")).expect("services"), "services")?;
        let router = mapping(service(services, "router")?, "router")?;
        let asset = mapping(service(services, "asset")?, "asset")?;
        let diagnostics = mapping(service(services, "diagnostics")?, "diagnostics")?;
        let user = mapping(service(services, "user-avoid")?, "user-avoid")?;

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
        assert_eq!(
            environment.get(key("PHOXAL_BUNDLE_ROOT")),
            Some(&key("/robot"))
        );
        assert_eq!(
            environment.get(key("PHOXAL_CONNECT")),
            Some(&key("tcp/router:7447"))
        );
        assert_eq!(
            environment.get(key("PHOXAL_ROBOT_ID")),
            Some(&key("testbot"))
        );
        assert_eq!(environment.get(key("PHOXAL_NAMESPACE")), Some(&key("test")));
        assert_eq!(
            environment.get(key("PHOXAL_CLOCK")),
            Some(&key("simulation"))
        );
        assert!(environment.get(key("ROBOT_CONFIG")).is_none());
        assert!(environment.get(key("ROBOT_ID")).is_none());
        assert!(environment.get(key("ROBOT_NAMESPACE")).is_none());
        assert!(environment.get(key("ROBOT_ROUTER_ENDPOINT")).is_none());
        assert!(environment.get(key("ROBOT_SIMULATION")).is_none());

        let diagnostics_environment = mapping(
            diagnostics
                .get(key("environment"))
                .expect("diagnostics environment"),
            "diagnostics environment",
        )?;
        assert_eq!(
            diagnostics_environment.get(key("PHOXAL_CONNECT")),
            Some(&key(""))
        );

        let user_environment = mapping(
            user.get(key("environment")).expect("user environment"),
            "user environment",
        )?;
        assert_eq!(
            user_environment.get(key("PHOXAL_PARTICIPANT_ID")),
            Some(&key("avoid"))
        );
        assert_eq!(
            user_environment.get(key("PHOXAL_CONFIG")),
            Some(&key(r#"{"gain":0.7}"#))
        );
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

    #[test]
    fn selected_bus_profile_materializes_connect_and_keeps_default_router_listen()
    -> anyhow::Result<()> {
        let resolved = resolved_robot()?;
        let mut extras = manifest_extras();
        // A profile is just `{ connect }`; the router-binding `listen` endpoint
        // is deploy infra (the CLI default), never a manifest profile field.
        extras.bus = BusManifestExtras {
            selected_profile: Some("lab".to_string()),
            profiles: BTreeMap::from([(
                "lab".to_string(),
                BusProfileConfig {
                    connect: Some("tcp/lab-router:7447".to_string()),
                },
            )]),
        };
        let compose = generate(
            &resolved,
            &TEST_CATALOG,
            &PathBuf::from("/tmp/phoxal/run"),
            &BTreeMap::new(),
            &[],
            &extras,
            LaunchClock::Simulation,
        )?;
        let compose: Value = serde_yaml::from_str(&compose)?;
        let root = mapping(&compose, "compose")?;
        let services = mapping(root.get(key("services")).expect("services"), "services")?;
        let router = mapping(service(services, "router")?, "router")?;
        let asset = mapping(service(services, "asset")?, "asset")?;
        let user = mapping(service(services, "user-avoid")?, "user-avoid")?;

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
        let asset_environment = mapping(
            asset.get(key("environment")).expect("asset environment"),
            "asset environment",
        )?;
        assert_eq!(
            asset_environment.get(key("PHOXAL_CONNECT")),
            Some(&key("tcp/lab-router:7447"))
        );
        let user_environment = mapping(
            user.get(key("environment")).expect("user environment"),
            "user environment",
        )?;
        assert_eq!(
            user_environment.get(key("PHOXAL_CONNECT")),
            Some(&key("tcp/lab-router:7447"))
        );
        Ok(())
    }

    fn resolved_robot() -> anyhow::Result<ResolvedRobot> {
        Ok(ResolvedRobot {
            robot: Robot::parse_from_string(MINIMAL_ROBOT)?,
            api_version: "y2026_1".to_string(),
            channel: Channel::Stable,
            platform_runtimes: vec![
                resolved_platform_runtime("asset", "ghcr.io/phoxal/runtime-asset:y2026_1-stable"),
                resolved_platform_runtime(
                    "diagnostics",
                    "ghcr.io/phoxal/runtime-diagnostics:y2026_1-stable",
                ),
            ],
            user_runtimes: vec![ResolvedUserRuntime {
                name: "avoid".to_string(),
                path: PathBuf::from("runtimes/avoid"),
                framework: "y2026_1".to_string(),
                build: None,
                source_hash: "abc123".to_string(),
                image: "phoxal.local/testbot/user-runtime/avoid:dev".to_string(),
            }],
            components: Vec::new(),
            tools: Vec::new(),
        })
    }

    fn resolved_platform_runtime(name: &str, image_ref: &str) -> ResolvedPlatformRuntime {
        ResolvedPlatformRuntime {
            name: name.to_string(),
            image_ref: image_ref.to_string(),
            pin: ImagePin::Digest(format!("sha256:{name}")),
        }
    }

    fn manifest_extras() -> RobotManifestExtras {
        RobotManifestExtras {
            user_runtimes: BTreeMap::from([(
                "avoid".to_string(),
                UserRuntimeManifestExtras {
                    image: None,
                    config: Some(serde_json::json!({ "gain": 0.7 })),
                },
            )]),
            ..RobotManifestExtras::default()
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

    const MINIMAL_ROBOT: &str = r#"schema: v0
api_version: y2026_1

identity:
  id: testbot
  namespace: test

structure: structure.urdf

phoxal_runtimes:
  channel: stable

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
