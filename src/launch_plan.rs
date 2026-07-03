use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use phoxal::check as graph_check;
use phoxal::participant::launch::{
    BusProfile, ClockMode, DEFAULT_SHUTDOWN_GRACE_MS, ParticipantLaunch,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::commands::check::{SourceParticipant, SourceParticipantKind};
use crate::resolver::{ResolvedRobot, ResolvedTool, RobotManifestExtras};

pub const DEFAULT_ROUTER_CONNECT: &str = "tcp/localhost:7447";
pub const SITE_TOOL_ROUTER: &str = "tool-router";
pub const SITE_TOOL_JOYPAD: &str = "tool-joypad";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LaunchMode {
    Run,
    Sim,
    Deploy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaunchPlan {
    pub mode: LaunchMode,
    pub site: Vec<SiteLaunch>,
    pub robots: Vec<RobotLaunch>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SiteLaunch {
    pub id: String,
    pub artifact_ref: String,
    #[serde(rename = "PHOXAL_CONFIG")]
    pub phoxal_config: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RobotLaunch {
    pub id: String,
    pub namespace: String,
    pub participants: Vec<ParticipantLaunchRecord>,
    pub substitutions: Vec<SubstitutionRecord>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParticipantLaunchRecord {
    pub artifact_id: String,
    pub execution: ParticipantExecution,
    pub launch: ParticipantLaunch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ParticipantExecution {
    OfficialArtifact { artifact_ref: String },
    UserService { crate_dir: PathBuf },
    ComponentDriver { crate_dir: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubstitutionRecord {
    pub component_instance: String,
    pub provider_participant_id: String,
    pub contracts: Vec<SubstitutedContract>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubstitutedContract {
    pub family: String,
    pub topic: String,
    pub direction: String,
    pub schema_id: String,
}

#[derive(Debug, Clone, Copy)]
pub struct CheckedRobotLaunchInput<'a> {
    pub project_root: &'a Path,
    pub resolved: &'a ResolvedRobot,
    pub manifest_extras: &'a RobotManifestExtras,
    pub checked_participants: &'a [graph_check::ParticipantApis],
    pub source_participants: &'a [SourceParticipant],
}

pub fn build_launch_plan(
    mode: LaunchMode,
    robots: &[CheckedRobotLaunchInput<'_>],
) -> Result<LaunchPlan> {
    if robots.is_empty() {
        bail!("LaunchPlan requires at least one robot");
    }
    if mode == LaunchMode::Deploy && robots.len() != 1 {
        bail!("deploy LaunchPlan must contain exactly one robot");
    }

    let site = build_site_launches(mode, robots)?;
    let robots = robots
        .iter()
        .map(|robot| build_robot_launch(mode, robot))
        .collect::<Result<Vec<_>>>()?;

    Ok(LaunchPlan { mode, site, robots })
}

fn build_site_launches(
    mode: LaunchMode,
    robots: &[CheckedRobotLaunchInput<'_>],
) -> Result<Vec<SiteLaunch>> {
    let router = merge_site_tool_artifact(SITE_TOOL_ROUTER, robots)?;
    let joypad = merge_site_tool_artifact(SITE_TOOL_JOYPAD, robots)?;
    let router_config = router_config(mode, robots)?;
    Ok(vec![
        SiteLaunch {
            id: SITE_TOOL_ROUTER.to_string(),
            artifact_ref: router,
            phoxal_config: router_config,
        },
        SiteLaunch {
            id: SITE_TOOL_JOYPAD.to_string(),
            artifact_ref: joypad,
            phoxal_config: serde_json::json!({}),
        },
    ])
}

fn merge_site_tool_artifact(
    tool_name: &str,
    robots: &[CheckedRobotLaunchInput<'_>],
) -> Result<String> {
    let mut artifact_ref = None;
    for robot in robots {
        let tool = resolved_tool(robot.resolved, tool_name)?;
        let current = tool_artifact_ref(tool);
        if let Some(existing) = &artifact_ref {
            if existing != &current {
                bail!(
                    "site tool {tool_name} resolves to conflicting artifacts: {existing} and {current}"
                );
            }
        } else {
            artifact_ref = Some(current);
        }
    }
    artifact_ref.ok_or_else(|| anyhow!("site tool {tool_name} was not resolved"))
}

fn resolved_tool<'a>(resolved: &'a ResolvedRobot, tool_name: &str) -> Result<&'a ResolvedTool> {
    resolved
        .tools
        .iter()
        .find(|tool| tool.name == tool_name)
        .ok_or_else(|| {
            anyhow!(
                "resolved robot {} is missing required site tool {tool_name}",
                resolved.robot.identity.id
            )
        })
}

fn tool_artifact_ref(tool: &ResolvedTool) -> String {
    format!("{}@{}:{}", tool.repo, tool.resolved, tool.asset)
}

fn router_config(mode: LaunchMode, robots: &[CheckedRobotLaunchInput<'_>]) -> Result<Value> {
    let mut listen = BTreeSet::<String>::new();
    let mut device_claims = BTreeMap::<String, String>::new();
    for robot in robots {
        for endpoint in &robot.resolved.robot.bus.listen {
            let endpoint = endpoint.trim();
            if endpoint.is_empty() {
                continue;
            }
            if let Some(device) = listen_device_claim(endpoint) {
                if let Some(existing) = device_claims.get(&device) {
                    if existing != endpoint {
                        bail!(
                            "conflicting router listen claims for device {device}: {existing} and {endpoint}"
                        );
                    }
                } else {
                    device_claims.insert(device, endpoint.to_string());
                }
            }
            listen.insert(endpoint.to_string());
        }
    }

    let mut config = Map::new();
    if !listen.is_empty() {
        config.insert(
            "listen".to_string(),
            Value::Array(listen.into_iter().map(Value::String).collect()),
        );
    }
    if mode == LaunchMode::Deploy
        && let Some(uplink) = &robots[0].resolved.robot.bus.uplink
    {
        config.insert(
            "uplink".to_string(),
            serde_json::to_value(uplink).context("failed to encode router uplink config")?,
        );
    }
    Ok(Value::Object(config))
}

fn listen_device_claim(endpoint: &str) -> Option<String> {
    let rest = endpoint.strip_prefix("serial/")?;
    let query = rest.find('?').unwrap_or(rest.len());
    let fragment = rest.find('#').unwrap_or(rest.len());
    let device = &rest[..query.min(fragment)];
    Some(device.to_string())
}

fn build_robot_launch(
    mode: LaunchMode,
    input: &CheckedRobotLaunchInput<'_>,
) -> Result<RobotLaunch> {
    ensure_launch_set_parity(input)?;
    let source_participants = input
        .source_participants
        .iter()
        .map(|participant| (participant.name.as_str(), participant))
        .collect::<BTreeMap<_, _>>();
    let official_artifacts = input
        .resolved
        .platform_runtimes
        .iter()
        .map(|runtime| (runtime.name.as_str(), runtime.artifact_ref().to_string()))
        .collect::<BTreeMap<_, _>>();

    let mut participants = Vec::new();
    for checked in input
        .checked_participants
        .iter()
        .filter(|participant| participant.participant_class.is_checked())
    {
        let execution = participant_execution(checked, &source_participants, &official_artifacts)?;
        let launch = participant_launch(mode, input, checked);
        participants.push(ParticipantLaunchRecord {
            artifact_id: checked.artifact_id.clone(),
            execution,
            launch,
        });
    }
    participants.sort_by(|left, right| {
        left.launch
            .participant_id
            .cmp(&right.launch.participant_id)
            .then_with(|| left.artifact_id.cmp(&right.artifact_id))
    });

    Ok(RobotLaunch {
        id: input.resolved.robot.identity.id.clone(),
        namespace: input.resolved.robot.identity.namespace.clone(),
        participants,
        substitutions: Vec::new(),
    })
}

fn participant_execution(
    checked: &graph_check::ParticipantApis,
    source_participants: &BTreeMap<&str, &SourceParticipant>,
    official_artifacts: &BTreeMap<&str, String>,
) -> Result<ParticipantExecution> {
    if let Some(source) = source_participants.get(checked.participant_id.as_str()) {
        return Ok(match source.kind {
            SourceParticipantKind::UserService => ParticipantExecution::UserService {
                crate_dir: source.crate_dir.clone(),
            },
            SourceParticipantKind::ComponentDriver => ParticipantExecution::ComponentDriver {
                crate_dir: source.crate_dir.clone(),
            },
        });
    }
    if let Some(artifact_ref) = official_artifacts.get(checked.artifact_id.as_str()) {
        return Ok(ParticipantExecution::OfficialArtifact {
            artifact_ref: artifact_ref.clone(),
        });
    }
    bail!(
        "checked participant {} has no resolved execution source",
        checked.participant_id
    )
}

fn participant_launch(
    mode: LaunchMode,
    input: &CheckedRobotLaunchInput<'_>,
    checked: &graph_check::ParticipantApis,
) -> ParticipantLaunch {
    let component_instance = match &checked.scope {
        graph_check::ParticipantScope::ComponentInstance(instance) => Some(instance.clone()),
        graph_check::ParticipantScope::Graph => None,
    };
    ParticipantLaunch {
        participant_id: checked.participant_id.clone(),
        namespace: input.resolved.robot.identity.namespace.clone(),
        robot_id: input.resolved.robot.identity.id.clone(),
        bus: BusProfile {
            connect_endpoints: vec![DEFAULT_ROUTER_CONNECT.to_string()],
        },
        clock: match mode {
            LaunchMode::Run | LaunchMode::Deploy => ClockMode::Real,
            LaunchMode::Sim => ClockMode::Simulation,
        },
        config: input
            .manifest_extras
            .user_runtime_config(&checked.participant_id)
            .cloned(),
        robot_root: Some(robot_root_for_mode(mode, input.project_root)),
        component_instance,
        shutdown_grace_ms: DEFAULT_SHUTDOWN_GRACE_MS,
    }
}

fn robot_root_for_mode(mode: LaunchMode, project_root: &Path) -> PathBuf {
    match mode {
        LaunchMode::Run | LaunchMode::Sim => project_root.to_path_buf(),
        LaunchMode::Deploy => PathBuf::from("manifest"),
    }
}

fn ensure_launch_set_parity(input: &CheckedRobotLaunchInput<'_>) -> Result<()> {
    let expected = expected_checked_participant_ids(input.resolved);
    let checked = input
        .checked_participants
        .iter()
        .filter(|participant| participant.participant_class.is_checked())
        .map(|participant| participant.participant_id.clone())
        .collect::<BTreeSet<_>>();

    let missing = expected
        .difference(&checked)
        .cloned()
        .collect::<Vec<String>>();
    let extra = checked
        .difference(&expected)
        .cloned()
        .collect::<Vec<String>>();

    if !missing.is_empty() || !extra.is_empty() {
        let mut message = String::from("checked participant set does not match the LaunchPlan set");
        if !missing.is_empty() {
            message.push_str("; missing from checked metadata: ");
            message.push_str(&missing.join(", "));
        }
        if !extra.is_empty() {
            message.push_str("; checked metadata has no resolved participant: ");
            message.push_str(&extra.join(", "));
        }
        bail!("{message}");
    }
    Ok(())
}

fn expected_checked_participant_ids(resolved: &ResolvedRobot) -> BTreeSet<String> {
    let mut expected = BTreeSet::new();
    expected.extend(
        resolved
            .platform_runtimes
            .iter()
            .map(|runtime| runtime.name.clone()),
    );
    expected.extend(
        resolved
            .user_runtimes
            .iter()
            .map(|runtime| runtime.name.clone()),
    );
    expected.extend(
        resolved
            .components
            .iter()
            .filter(|component| component.has_driver)
            .map(|component| component.instance.clone()),
    );
    expected
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use anyhow::bail;

    use crate::catalog::{
        ArtifactStatus, Channel as CatalogChannel, fixture_catalog_for_tests,
        fixture_contract_for_tests, fixture_service_entry_for_tests,
    };
    use crate::commands::check::{
        CheckGraphContext, RawArtifact, RawEmitApis, SourceParticipant,
        platform_artifact_refs_from_resolved, robot_graph_from_resolved, run_check_with_context,
    };
    use crate::resolver::{
        ResolveOptions, ResolvedRobot, host_target_triple, resolve, target_generation_for_robot,
    };

    use super::*;

    #[test]
    fn launch_plan_covers_site_singletons_services_and_component_instances() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        std::fs::create_dir_all(temp.path().join("runtimes/mission"))?;
        std::fs::write(
            temp.path().join("runtimes/mission/Cargo.toml"),
            "[package]\nname = \"mission\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )?;
        std::fs::write(temp.path().join("runtimes/mission/src.txt"), "source")?;
        let robot = phoxal::model::robot::v1::Robot::parse_from_string(FIXTURE_ROBOT)?;
        let catalog = fixture_catalog_for_tests(vec![fixture_service_entry_for_tests(
            "drive",
            "y2026_1",
            "0.1.0",
            CatalogChannel::Stable,
            &host_target_triple(),
            ArtifactStatus::Released,
            vec![fixture_contract_for_tests(
                "drive::Target",
                "drive/target",
                "publish",
                "schema-drive",
            )],
        )]);
        let mut resolved = resolve(
            &robot,
            temp.path(),
            Some(&catalog),
            ResolveOptions {
                resolve_external_artifacts: false,
                resolve_source_commits: false,
            },
        )?;
        add_site_tools(&mut resolved);
        let mut extras = RobotManifestExtras::default();
        extras.user_runtimes.insert(
            "mission".to_string(),
            crate::resolver::UserRuntimeManifestExtras {
                config: Some(serde_json::json!({
                    "message": "line\nquoted \"value\"",
                })),
            },
        );
        let source_participants = vec![
            SourceParticipant::user_service("mission", temp.path().join("runtimes/mission")),
            SourceParticipant::component_driver_with_artifact_id(
                "left_drive",
                "ddsm115",
                temp.path().join("components/ddsm115"),
            ),
            SourceParticipant::component_driver_with_artifact_id(
                "right_drive",
                "ddsm115",
                temp.path().join("components/ddsm115"),
            ),
        ];
        let robot_graph = robot_graph_from_resolved(&resolved);
        let platform_refs = platform_artifact_refs_from_resolved(&resolved);
        let outcome = run_check_with_context(
            &platform_refs,
            &[],
            &source_participants,
            CheckGraphContext {
                robot_graph: &robot_graph,
                manifest_extras: &extras,
            },
            |artifact_ref| {
                if artifact_ref.contains("service-drive") {
                    Ok(raw_emit_apis("service", "drive"))
                } else {
                    bail!("unexpected platform artifact {artifact_ref}")
                }
            },
            |_| bail!("no tools in this check fixture"),
            |source| match source.kind {
                SourceParticipantKind::UserService => Ok(raw_emit_apis("service", &source.name)),
                SourceParticipantKind::ComponentDriver => {
                    Ok(raw_emit_apis("driver", &source.expected_artifact_id))
                }
            },
        )?;
        assert!(outcome.is_ok(), "fixture check should pass: {outcome:?}");
        let plan = build_launch_plan(
            LaunchMode::Run,
            &[CheckedRobotLaunchInput {
                project_root: temp.path(),
                resolved: &resolved,
                manifest_extras: &extras,
                checked_participants: &outcome.checked_participants,
                source_participants: &source_participants,
            }],
        )?;

        assert_eq!(plan.mode, LaunchMode::Run);
        assert_eq!(plan.site[0].id, SITE_TOOL_ROUTER);
        assert_eq!(
            plan.site[0].phoxal_config,
            serde_json::json!({"listen": ["serial//dev/ttyUSB0?baudrate=115200"]})
        );
        assert_eq!(plan.site[1].id, SITE_TOOL_JOYPAD);
        let robot = &plan.robots[0];
        assert_eq!(robot.id, "robot_v1");
        assert_eq!(robot.substitutions, Vec::<SubstitutionRecord>::new());
        let participant_ids = robot
            .participants
            .iter()
            .map(|participant| participant.launch.participant_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            participant_ids,
            vec!["drive", "left_drive", "mission", "right_drive"]
        );
        let left_drive = robot
            .participants
            .iter()
            .find(|participant| participant.launch.participant_id == "left_drive")
            .expect("left_drive participant");
        assert_eq!(left_drive.artifact_id, "ddsm115");
        assert_eq!(
            left_drive.launch.component_instance.as_deref(),
            Some("left_drive")
        );
        let mission = robot
            .participants
            .iter()
            .find(|participant| participant.launch.participant_id == "mission")
            .expect("mission participant");
        assert_eq!(
            mission.launch.config,
            Some(serde_json::json!({"message": "line\nquoted \"value\""}))
        );
        let encoded = crate::launch_env::encode_participant_env(&mission.launch)?;
        assert_eq!(
            encoded
                .variables()
                .get(phoxal::participant::launch::env::CONFIG)
                .map(String::as_str),
            Some(r#"{"message":"line\nquoted \"value\""}"#)
        );
        Ok(())
    }

    #[test]
    fn deploy_plan_rejects_multiple_robots() -> anyhow::Result<()> {
        let robot = empty_resolved_robot("robot_a")?;
        let extras = RobotManifestExtras::default();
        let inputs = [
            empty_checked_input(Path::new("/tmp/a"), &robot, &extras),
            empty_checked_input(Path::new("/tmp/b"), &robot, &extras),
        ];
        let error =
            build_launch_plan(LaunchMode::Deploy, &inputs).expect_err("deploy is one robot");
        assert!(error.to_string().contains("exactly one robot"), "{error:#}");
        Ok(())
    }

    #[test]
    fn multi_robot_listen_endpoints_collapse_and_conflicts_error() -> anyhow::Result<()> {
        let mut left = empty_resolved_robot("left")?;
        add_site_tools(&mut left);
        left.robot.bus.listen = vec![
            "serial//dev/ttyUSB0?baudrate=115200".to_string(),
            "tcp/127.0.0.1:7448".to_string(),
        ];
        let mut right = empty_resolved_robot("right")?;
        add_site_tools(&mut right);
        right.robot.bus.listen = vec![
            "serial//dev/ttyUSB0?baudrate=115200".to_string(),
            "tcp/127.0.0.1:7448".to_string(),
        ];
        let extras = RobotManifestExtras::default();
        let inputs = [
            empty_checked_input(Path::new("/tmp/left"), &left, &extras),
            empty_checked_input(Path::new("/tmp/right"), &right, &extras),
        ];
        let plan = build_launch_plan(LaunchMode::Sim, &inputs)?;
        assert_eq!(
            plan.site[0].phoxal_config,
            serde_json::json!({
                "listen": [
                    "serial//dev/ttyUSB0?baudrate=115200",
                    "tcp/127.0.0.1:7448"
                ]
            })
        );

        right.robot.bus.listen = vec!["serial//dev/ttyUSB0?baudrate=9600".to_string()];
        let inputs = [
            empty_checked_input(Path::new("/tmp/left"), &left, &extras),
            empty_checked_input(Path::new("/tmp/right"), &right, &extras),
        ];
        let error = build_launch_plan(LaunchMode::Sim, &inputs)
            .expect_err("same serial device with different options conflicts");
        assert!(
            error
                .to_string()
                .contains("conflicting router listen claims"),
            "{error:#}"
        );
        Ok(())
    }

    #[test]
    fn parity_rejects_missing_and_extra_checked_metadata() -> anyhow::Result<()> {
        let mut resolved = empty_resolved_robot("robot_v1")?;
        add_site_tools(&mut resolved);
        resolved
            .user_runtimes
            .push(crate::resolver::ResolvedUserRuntime {
                name: "mission".to_string(),
                path: PathBuf::from("runtimes/mission"),
                framework: "y2026_1".to_string(),
                source_hash: "hash".to_string(),
            });
        let extras = RobotManifestExtras::default();
        let sources = vec![SourceParticipant::user_service(
            "mission",
            PathBuf::from("/tmp/mission"),
        )];
        let checked = vec![participant(
            "other",
            "other",
            graph_check::ParticipantScope::Graph,
        )];
        let error = build_launch_plan(
            LaunchMode::Run,
            &[CheckedRobotLaunchInput {
                project_root: Path::new("/tmp/robot"),
                resolved: &resolved,
                manifest_extras: &extras,
                checked_participants: &checked,
                source_participants: &sources,
            }],
        )
        .expect_err("parity should fail");
        let message = error.to_string();
        assert!(message.contains("mission"), "{message}");
        assert!(message.contains("other"), "{message}");
        Ok(())
    }

    fn participant(
        participant_id: &str,
        artifact_id: &str,
        scope: graph_check::ParticipantScope,
    ) -> graph_check::ParticipantApis {
        graph_check::ParticipantApis {
            participant_id: participant_id.to_string(),
            artifact_id: artifact_id.to_string(),
            participant_class: graph_check::ParticipantClass::Checked,
            api_version: "y2026_1".to_string(),
            bus_abi: None,
            config_schema: None,
            scope,
            contracts: Vec::new(),
        }
    }

    fn raw_emit_apis(kind: &str, id: &str) -> RawEmitApis {
        RawEmitApis {
            artifact: RawArtifact {
                kind: kind.to_string(),
                id: id.to_string(),
            },
            participant_class: "checked".to_string(),
            api_version: "y2026_1".to_string(),
            bus_abi: None,
            required_contracts: Vec::new(),
            config_schema: None,
        }
    }

    fn empty_checked_input<'a>(
        project_root: &'a Path,
        resolved: &'a ResolvedRobot,
        manifest_extras: &'a RobotManifestExtras,
    ) -> CheckedRobotLaunchInput<'a> {
        CheckedRobotLaunchInput {
            project_root,
            resolved,
            manifest_extras,
            checked_participants: &[],
            source_participants: &[],
        }
    }

    fn empty_resolved_robot(id: &str) -> anyhow::Result<ResolvedRobot> {
        let yaml = format!(
            r#"schema: v0
api_version: y2026_1
identity:
  id: {id}
  namespace: dev
structure: structure.urdf
phoxal_participants: {{}}
motion:
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
components:
  sources: {{}}
  instances: {{}}
"#
        );
        let robot = phoxal::model::robot::v1::Robot::parse_from_string(&yaml)?;
        let generation = target_generation_for_robot(&robot, None)?;
        Ok(ResolvedRobot {
            robot,
            target_generation: generation,
            channel: phoxal::model::robot::v1::Channel::Stable,
            target: host_target_triple(),
            catalog_revision: None,
            platform_runtimes: Vec::new(),
            user_runtimes: Vec::new(),
            components: Vec::new(),
            tools: Vec::new(),
        })
    }

    fn add_site_tools(resolved: &mut ResolvedRobot) {
        resolved.tools.push(tool(SITE_TOOL_ROUTER));
        resolved.tools.push(tool(SITE_TOOL_JOYPAD));
    }

    fn tool(name: &str) -> ResolvedTool {
        ResolvedTool {
            name: name.to_string(),
            requested: "0.1.0".to_string(),
            resolved: "0.1.0".to_string(),
            repo: "phoxal/framework".to_string(),
            asset: format!("{name}-0.1.0-{}.tar.gz", host_target_triple()),
            binary_name: name.to_string(),
            sha256: "0".repeat(64),
        }
    }

    const FIXTURE_ROBOT: &str = r#"schema: v0
api_version: y2026_1
identity:
  id: robot_v1
  namespace: dev
structure: structure.urdf
bus:
  listen:
    - serial//dev/ttyUSB0?baudrate=115200
phoxal_participants: {}
user_participants:
  mission:
    path: runtimes/mission
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
  sources:
    ddsm115:
      path: components/ddsm115
  instances:
    left_drive:
      component: ddsm115
      mount_link: left_wheel
      driver:
        connection: { type: can, bus: 0, node_id: 1 }
    right_drive:
      component: ddsm115
      mount_link: right_wheel
      driver:
        connection: { type: can, bus: 0, node_id: 2 }
"#;
}
