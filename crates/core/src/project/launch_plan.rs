//! Checked launch-plan construction for run and simulation sessions.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use phoxal::check as graph_check;
use phoxal::participant::launch::{
    BusProfile, ClockMode, DEFAULT_SHUTDOWN_GRACE_MS, ParticipantLaunch,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::resolver::{ResolvedRobot, ResolvedTool, RobotManifestExtras};
use crate::check::source::{SourceParticipant, SourceParticipantKind};

pub const DEFAULT_ROUTER_CONNECT: &str = "tcp/localhost:7447";
pub const SITE_INFRASTRUCTURE_ROUTER: &str = "infrastructure-router";
pub const SITE_TOOL_BUS: &str = "tool-bus";
pub const SITE_TOOL_JOYPAD: &str = "tool-joypad";
pub const ROBOT_TOOL_LOG: &str = "tool-log";
/// The host-resource-meter tool (CLI-UX Phase 3/4): a standard, hard-required
/// bus participant exactly like `tool-joypad`, published in every mode (Run,
/// Deploy, Webots) - a host meter is useful everywhere, including a deployed
/// robot, unlike the joypad peripheral. A catalog snapshot that cannot
/// resolve it is outdated and fails resolution (product decision 9); there
/// is no graceful-degrade path.
pub const SITE_TOOL_TELEMETRY: &str = "tool-telemetry";
pub const RUNTIME_ROBOT_ROOT_RELATIVE: &str = ".phoxal/run/robot";
pub const SIMULATOR_SUPERVISOR_PROVIDER_ID: &str = "simulator-webots-supervisor";
pub const SIMULATOR_SUPERVISOR_ARTIFACT_NAME: &str = "webots-supervisor";
pub const SIMULATOR_CONTROLLER_ARTIFACT_NAME: &str = "webots-controller";

#[must_use]
pub fn simulator_controller_provider_id(robot_id: &str) -> String {
    format!("simulator-webots-controller-{robot_id}")
}

/// The full standard, hard-required site-tool id set (product decision 9) -
/// derived ONCE here and used consistently everywhere a caller needs to ask
/// "is this a standard site tool": [`build_site_launches`] (launch), and
/// simulation launch resolution's graph-proof
/// filtering (resolution/validation). Resolution itself
/// (`catalog::OFFICIAL_TOOLS`) and readiness (`supervisor`'s per-`Tool`-kind
/// handling) already treat every standard tool uniformly by kind rather than
/// naming router/joypad/telemetry individually, so this is the one list the
/// id-based call sites needed to share (finding A6).
///
/// Finding A6's regression: `simulate`'s metadata/contract filter used to
/// hardcode only `SITE_INFRASTRUCTURE_ROUTER`/`SITE_TOOL_JOYPAD` at three separate call
/// sites, silently excluding telemetry's declared graph contracts from
/// validation even though telemetry is started and readiness-waited exactly
/// like the other two standard tools.
pub const STANDARD_SITE_TOOLS: &[&str] = &[SITE_TOOL_JOYPAD, SITE_TOOL_TELEMETRY];
pub const STANDARD_ROBOT_TOOLS: &[&str] = &[SITE_TOOL_BUS, ROBOT_TOOL_LOG];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LaunchMode {
    Run,
    Deploy,
    /// Simulate under Webots, carrying the resolved `.wbt` world path the
    /// plan was built for. Replaces the old data-less `Sim` variant plus the
    /// `SimulatePlan::world_path` field it used to take a detour through -
    /// the plan's own mode now carries the world directly.
    Webots {
        world: PathBuf,
    },
}

/// The shared context a `LaunchPlan` is built from and launched alongside:
/// which `robot.yaml` it came from, the project root, the full resolved
/// robot, and its source participants. Not part of the plan itself (the plan
/// is the launch descriptor; this is where it came from), and - like
/// `LaunchPlan` - never persisted to disk. Replaces the fields the old
/// `SimulatePlan` wrapper re-declared next to its own `LaunchPlan`
/// (`resolved`/`project_root`/`source_participants`/`robot_path`), and the
/// matching re-declarations in `run`'s `PreparedRun`, `deploy`'s
/// `RenderPayloadInput`, and the `watch` configs.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanContext {
    pub robot_path: PathBuf,
    pub project_root: PathBuf,
    pub resolved: ResolvedRobot,
    pub source_participants: Vec<SourceParticipant>,
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
    #[serde(default)]
    pub launch_ownership: LaunchOwnership,
}

/// Who owns a participant's process lifecycle. Orthogonal to `participant_kind`:
/// most participants are `CliManaged` (the CLI supervisor spawns, restarts, and
/// tears them down). A `SimulationManaged` participant still satisfies the
/// graph proof and appears on the board via bus presence/logs (D23), but the
/// CLI supervisor never spawns or restarts it - Webots (via the supervisor)
/// owns its lifecycle. Both the Webots supervisor and each robot's controller
/// are `SimulationManaged` in `Webots` mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchOwnership {
    #[default]
    CliManaged,
    SimulationManaged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "execution", rename_all = "snake_case")]
pub enum ParticipantExecution {
    OfficialArtifact { artifact_ref: String },
    OfficialTool { artifact_ref: String },
    UserService { crate_dir: PathBuf },
    SourceArtifact { kind: String, crate_dir: PathBuf },
    ComponentDriver { crate_dir: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubstitutionRecord {
    pub component_instance: String,
    pub provider_participant_id: String,
    pub provider_artifact_id: String,
    pub provider_kind: String,
    pub contracts: Vec<SubstitutedContract>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubstitutedContract {
    pub family: String,
}

#[derive(Debug, Clone, Copy)]
pub struct CheckedRobotLaunchInput<'a> {
    pub project_root: &'a Path,
    pub resolved: &'a ResolvedRobot,
    pub manifest_extras: &'a RobotManifestExtras,
    pub checked_participants: &'a [graph_check::ParticipantApis],
    pub substitutions: &'a [SubstitutionRecord],
    pub source_participants: &'a [SourceParticipant],
}

pub fn build_launch_plan(
    mode: LaunchMode,
    robots: &[CheckedRobotLaunchInput<'_>],
) -> Result<LaunchPlan> {
    if robots.is_empty() {
        bail!("LaunchPlan requires at least one robot");
    }
    if matches!(mode, LaunchMode::Deploy) && robots.len() != 1 {
        bail!("deploy LaunchPlan must contain exactly one robot");
    }

    let site = build_site_launches(&mode, robots)?;
    let robots = robots
        .iter()
        .map(|robot| build_robot_launch(&mode, robot))
        .collect::<Result<Vec<_>>>()?;

    Ok(LaunchPlan { mode, site, robots })
}

/// Builds launch entries for standard site tools. Deploy also receives the
/// infrastructure router entry used to render its dedicated systemd unit.
fn build_site_launches(
    mode: &LaunchMode,
    robots: &[CheckedRobotLaunchInput<'_>],
) -> Result<Vec<SiteLaunch>> {
    let mut site = Vec::with_capacity(STANDARD_SITE_TOOLS.len() + 1);
    if matches!(mode, LaunchMode::Deploy) {
        if robots.iter().any(|robot| {
            robot.resolved.tools.iter().any(|artifact| {
                artifact.kind == super::catalog::ArtifactKind::Infrastructure
                    && artifact.path_override.is_some()
            })
        }) {
            bail!(
                "deploy does not yet build a path-pinned infrastructure router; use the published artifact"
            );
        }
        if robots
            .iter()
            .any(|robot| robot.resolved.robot.router.config.is_some())
        {
            bail!(
                "deploy does not yet stage router.config; remove it for the default loopback router or run locally"
            );
        }
        site.push(SiteLaunch {
            id: SITE_INFRASTRUCTURE_ROUTER.to_string(),
            artifact_ref: merge_site_tool_artifact(SITE_INFRASTRUCTURE_ROUTER, robots)?,
            phoxal_config: Value::Null,
        });
    }
    for &tool_name in STANDARD_SITE_TOOLS {
        let artifact_ref = merge_site_tool_artifact(tool_name, robots)?;
        let phoxal_config = Value::Null;
        site.push(SiteLaunch {
            id: tool_name.to_string(),
            artifact_ref,
            phoxal_config,
        });
    }
    Ok(site)
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
    resolved.tools.iter().find(|tool| tool.name == tool_name).ok_or_else(|| {
        anyhow!(
            "resolved robot {} is missing standard site tool {tool_name}; the active catalog snapshot is outdated for this standard set - run `phoxal update`",
            resolved.robot.robot.id
        )
    })
}

fn tool_artifact_ref(tool: &ResolvedTool) -> String {
    format!("{}@{}:{}", tool.repo, tool.resolved, tool.asset)
}

fn build_robot_launch(
    mode: &LaunchMode,
    input: &CheckedRobotLaunchInput<'_>,
) -> Result<RobotLaunch> {
    ensure_launch_set_parity(mode, input)?;
    let source_participants = input
        .source_participants
        .iter()
        .map(|participant| (participant.name.as_str(), participant))
        .collect::<BTreeMap<_, _>>();
    let mut official_artifacts = input
        .resolved
        .platform_runtimes
        .iter()
        .chain(input.resolved.simulators.iter())
        .map(|runtime| (runtime.name.as_str(), runtime.artifact_ref().to_string()))
        .collect::<BTreeMap<_, _>>();
    // A Catalog-sourced component driver is a first-class catalog artifact
    // too (docs #21): its `catalog_runtime` projects onto the identical
    // `ResolvedPlatformRuntime` shape a service/simulator resolves to, keyed
    // here by the component id (`checked.artifact_id`) exactly like a
    // service is keyed by its own name.
    official_artifacts.extend(
        input
            .resolved
            .components
            .iter()
            .filter_map(|component| component.driver.as_ref())
            .filter_map(|driver| driver.catalog_runtime.as_ref())
            .map(|runtime| (runtime.name.as_str(), runtime.artifact_ref().to_string())),
    );

    let mut participants = Vec::new();
    for checked in input
        .checked_participants
        .iter()
        .filter(|participant| is_robot_launch_participant(mode, participant))
    {
        let execution = participant_execution(checked, &source_participants, &official_artifacts)?;
        let launch = participant_launch(mode, input, checked);
        let launch_ownership = launch_ownership(mode, checked);
        participants.push(ParticipantLaunchRecord {
            artifact_id: checked.artifact_id.clone(),
            execution,
            launch,
            launch_ownership,
        });
    }
    for &tool_name in STANDARD_ROBOT_TOOLS {
        let tool = resolved_tool(input.resolved, tool_name)?;
        participants.push(ParticipantLaunchRecord {
            artifact_id: tool.name.clone(),
            execution: tool.path_override.as_ref().map_or_else(
                || ParticipantExecution::OfficialTool {
                    artifact_ref: tool_artifact_ref(tool),
                },
                |crate_dir| ParticipantExecution::SourceArtifact {
                    kind: "tool".to_string(),
                    crate_dir: crate_dir.clone(),
                },
            ),
            launch: ParticipantLaunch {
                participant_id: tool.name.clone(),
                namespace: input.resolved.robot.robot.namespace.clone(),
                robot_id: input.resolved.robot.robot.id.clone(),
                bus: BusProfile {
                    connect_endpoints: vec![DEFAULT_ROUTER_CONNECT.to_string()],
                },
                clock: ClockMode::Real,
                config: None,
                robot_root: Some(robot_root_for_mode(mode, input.project_root)),
                component_instance: None,
                shutdown_grace_ms: DEFAULT_SHUTDOWN_GRACE_MS,
            },
            launch_ownership: LaunchOwnership::CliManaged,
        });
    }
    participants.sort_by(|left, right| {
        left.launch
            .participant_id
            .cmp(&right.launch.participant_id)
            .then_with(|| left.artifact_id.cmp(&right.artifact_id))
    });

    Ok(RobotLaunch {
        id: input.resolved.robot.robot.id.clone(),
        namespace: input.resolved.robot.robot.namespace.clone(),
        participants,
        substitutions: input.substitutions.to_vec(),
    })
}

fn is_robot_launch_participant(
    mode: &LaunchMode,
    participant: &graph_check::ParticipantApis,
) -> bool {
    if !participant.participant_class.is_checked() {
        return false;
    }
    if participant.participant_kind == graph_check::ParticipantKind::Tool {
        return false;
    }
    if participant.participant_kind == graph_check::ParticipantKind::Simulator {
        // Simulator participants (the Webots supervisor + each robot's
        // controller) are launched by Webots itself, never by the CLI
        // supervisor - but in Webots mode they still need a launch record for
        // board presence and controllerArgs/spawn-descriptor rendering (Part
        // 3/4). Outside Webots mode a simulator participant never appears in
        // the checked set at all (substitutions are sim-only), so this only
        // takes effect for Webots.
        return matches!(mode, LaunchMode::Webots { .. });
    }
    if matches!(mode, LaunchMode::Webots { .. })
        && matches!(
            participant.participant_kind,
            graph_check::ParticipantKind::Driver
        )
    {
        return false;
    }
    true
}

/// Which launch-ownership a checked participant gets in this plan. Simulator
/// participants (the Webots supervisor and each robot's controller) are
/// `SimulationManaged` in `Webots` mode - the CLI supervisor never spawns or
/// restarts them, Webots does. Every other participant (services, user
/// runtimes, component drivers) is `CliManaged`.
fn launch_ownership(
    mode: &LaunchMode,
    participant: &graph_check::ParticipantApis,
) -> LaunchOwnership {
    if matches!(mode, LaunchMode::Webots { .. })
        && participant.participant_kind == graph_check::ParticipantKind::Simulator
    {
        LaunchOwnership::SimulationManaged
    } else {
        LaunchOwnership::CliManaged
    }
}

fn participant_execution(
    checked: &graph_check::ParticipantApis,
    source_participants: &BTreeMap<&str, &SourceParticipant>,
    official_artifacts: &BTreeMap<&str, String>,
) -> Result<ParticipantExecution> {
    let source = source_participants
        .get(checked.participant_id.as_str())
        .or_else(|| {
            if checked.participant_kind == graph_check::ParticipantKind::Simulator {
                source_participants.get(checked.artifact_id.as_str())
            } else {
                None
            }
        });
    if let Some(source) = source {
        return Ok(match source.kind {
            SourceParticipantKind::UserService => ParticipantExecution::UserService {
                crate_dir: source.crate_dir.clone(),
            },
            SourceParticipantKind::OfficialService => ParticipantExecution::SourceArtifact {
                kind: "service".to_string(),
                crate_dir: source.crate_dir.clone(),
            },
            SourceParticipantKind::ComponentDriver => ParticipantExecution::ComponentDriver {
                crate_dir: source.crate_dir.clone(),
            },
            SourceParticipantKind::Tool => ParticipantExecution::SourceArtifact {
                kind: "tool".to_string(),
                crate_dir: source.crate_dir.clone(),
            },
            SourceParticipantKind::Simulator => ParticipantExecution::SourceArtifact {
                kind: "simulator".to_string(),
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
    mode: &LaunchMode,
    input: &CheckedRobotLaunchInput<'_>,
    checked: &graph_check::ParticipantApis,
) -> ParticipantLaunch {
    let component_instance = match &checked.scope {
        graph_check::ParticipantScope::ComponentInstance(instance) => Some(instance.clone()),
        graph_check::ParticipantScope::Graph => None,
    };
    ParticipantLaunch {
        participant_id: checked.participant_id.clone(),
        namespace: input.resolved.robot.robot.namespace.clone(),
        robot_id: input.resolved.robot.robot.id.clone(),
        bus: BusProfile {
            connect_endpoints: vec![DEFAULT_ROUTER_CONNECT.to_string()],
        },
        // This field selects scheduling only for clocked service/driver launch
        // policies. Simulator binaries have a clockless, fixed host/Webots
        // policy and their controllerArgs never render this value, so launch
        // planning needs no simulator-kind exception.
        clock: match mode {
            LaunchMode::Run | LaunchMode::Deploy => ClockMode::Real,
            LaunchMode::Webots { .. } => ClockMode::Simulation,
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

fn robot_root_for_mode(mode: &LaunchMode, project_root: &Path) -> PathBuf {
    match mode {
        LaunchMode::Run | LaunchMode::Webots { .. } => {
            project_root.join(RUNTIME_ROBOT_ROOT_RELATIVE)
        }
        // The deployed robot root is the active generation symlink, not the
        // flat `/opt/phoxal` - robot.yaml, structure.urdf, and phoxal-release.json
        // are staged per-generation under `/opt/phoxal/active/` (see deploy's
        // ACTIVE_ROOT), so a participant reading `$PHOXAL_ROBOT_ROOT/robot.yaml`
        // must resolve through `active`. Pointing at `/opt/phoxal` makes every
        // participant fail with "failed to read robot file /opt/phoxal/robot.yaml".
        LaunchMode::Deploy => PathBuf::from("/opt/phoxal/active"),
    }
}

fn ensure_launch_set_parity(mode: &LaunchMode, input: &CheckedRobotLaunchInput<'_>) -> Result<()> {
    let expected = expected_checked_participant_ids(mode, input.resolved);
    let checked = input
        .checked_participants
        .iter()
        .filter(|participant| is_robot_launch_participant(mode, participant))
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

fn expected_checked_participant_ids(
    mode: &LaunchMode,
    resolved: &ResolvedRobot,
) -> BTreeSet<String> {
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
    if matches!(mode, LaunchMode::Webots { .. }) {
        expected.extend(expected_simulator_participant_ids(resolved));
    } else {
        expected.extend(
            resolved
                .components
                .iter()
                .filter(|component| component.has_driver)
                .map(|component| component.instance.clone()),
        );
    }
    expected
}

/// The participant ids the Webots launch set must carry for the resolved
/// simulator artifacts (the Webots supervisor plus this robot's controller),
/// using the same world-scoped/robot-scoped id scheme
/// the simulation participant projection assigns.
fn expected_simulator_participant_ids(resolved: &ResolvedRobot) -> BTreeSet<String> {
    resolved
        .simulators
        .iter()
        .filter_map(|runtime| simulator_participant_id(&runtime.name, &resolved.robot.robot.id))
        .collect()
}

fn simulator_participant_id(artifact_name: &str, robot_id: &str) -> Option<String> {
    match artifact_name {
        SIMULATOR_SUPERVISOR_ARTIFACT_NAME => Some(SIMULATOR_SUPERVISOR_PROVIDER_ID.to_string()),
        SIMULATOR_CONTROLLER_ARTIFACT_NAME => Some(simulator_controller_provider_id(robot_id)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::project::catalog::{ArtifactKind, SelectionChannel};
    use crate::project::resolver::{ResolvedRobot, ResolvedUserRuntime};

    use super::*;

    #[test]
    fn launch_plan_covers_site_singletons_and_user_service_config() -> anyhow::Result<()> {
        let mut resolved = empty_resolved_robot("robot_v1")?;
        add_site_tools(&mut resolved);
        resolved.user_runtimes.push(ResolvedUserRuntime {
            name: "mission".to_string(),
            path: PathBuf::from("runtimes/mission"),
            source_hash: "hash".to_string(),
        });
        let mut extras = RobotManifestExtras::default();
        extras.user_runtimes.insert(
            "mission".to_string(),
            crate::project::resolver::UserRuntimeManifestExtras {
                config: Some(serde_json::json!({"message": "line\nquoted \"value\""})),
            },
        );
        let sources = vec![SourceParticipant::user_service(
            "mission",
            PathBuf::from("/tmp/mission"),
        )];
        let checked = vec![participant(
            "mission",
            "mission",
            graph_check::ParticipantScope::Graph,
        )];
        let plan = build_launch_plan(
            LaunchMode::Run,
            &[CheckedRobotLaunchInput {
                project_root: Path::new("/tmp/robot"),
                resolved: &resolved,
                manifest_extras: &extras,
                checked_participants: &checked,
                substitutions: &[],
                source_participants: &sources,
            }],
        )?;

        assert_eq!(
            plan.site
                .iter()
                .map(|site| site.id.as_str())
                .collect::<Vec<_>>(),
            vec![SITE_TOOL_JOYPAD, SITE_TOOL_TELEMETRY]
        );
        assert_eq!(
            plan.robots[0]
                .participants
                .iter()
                .filter_map(|participant| match participant.execution {
                    ParticipantExecution::OfficialTool { .. } => {
                        Some(participant.launch.participant_id.as_str())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec![SITE_TOOL_BUS, ROBOT_TOOL_LOG]
        );
        let mission = plan.robots[0]
            .participants
            .iter()
            .find(|participant| participant.launch.participant_id == "mission")
            .expect("mission participant");
        let encoded = crate::session::launch_env::encode_participant_env(&mission.launch)?;
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
    fn deploy_robot_root_is_the_active_generation() {
        // Regression: deployed participants read robot.yaml/structure.urdf via
        // `$PHOXAL_ROBOT_ROOT`, and those files are staged per-generation under
        // `/opt/phoxal/active/` (the transactional-release symlink). Pointing the
        // root at the flat `/opt/phoxal` made every participant on a real robot
        // die with "failed to read robot file /opt/phoxal/robot.yaml". It must
        // resolve through the active generation, matching deploy's ACTIVE_ROOT.
        assert_eq!(
            robot_root_for_mode(&LaunchMode::Deploy, Path::new("/tmp/project")),
            PathBuf::from("/opt/phoxal/active"),
        );
    }

    #[test]
    fn webots_launch_records_need_no_simulator_clock_exception() -> anyhow::Result<()> {
        let resolved = empty_resolved_robot("robot_v1")?;
        let extras = RobotManifestExtras::default();
        let input = CheckedRobotLaunchInput {
            project_root: Path::new("/tmp/robot"),
            resolved: &resolved,
            manifest_extras: &extras,
            checked_participants: &[],
            substitutions: &[],
            source_participants: &[],
        };
        let mode = LaunchMode::Webots {
            world: PathBuf::from("worlds/default.wbt"),
        };
        let service = participant("mission", "mission", graph_check::ParticipantScope::Graph);

        for (participant_id, artifact_id) in [
            ("simulator-webots-supervisor", "webots-supervisor"),
            ("simulator-webots-controller-robot_v1", "webots-controller"),
        ] {
            let mut simulator = participant(
                participant_id,
                artifact_id,
                graph_check::ParticipantScope::Graph,
            );
            simulator.participant_kind = graph_check::ParticipantKind::Simulator;
            assert_eq!(
                participant_launch(&mode, &input, &simulator).clock,
                ClockMode::Simulation,
                "{participant_id} uses the mode-wide record; its clockless binary policy ignores it"
            );
        }
        assert_eq!(
            participant_launch(&mode, &input, &service).clock,
            ClockMode::Simulation,
            "services in simulation must advance from published world steps"
        );
        Ok(())
    }

    #[test]
    fn parity_rejects_missing_and_extra_checked_metadata() -> anyhow::Result<()> {
        let mut resolved = empty_resolved_robot("robot_v1")?;
        add_site_tools(&mut resolved);
        resolved.user_runtimes.push(ResolvedUserRuntime {
            name: "mission".to_string(),
            path: PathBuf::from("runtimes/mission"),
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
                substitutions: &[],
                source_participants: &sources,
            }],
        )
        .expect_err("parity should fail");
        let message = error.to_string();
        assert!(message.contains("mission"), "{message}");
        assert!(message.contains("other"), "{message}");
        Ok(())
    }

    /// Product decision 9: router, joypad, and telemetry are STANDARD site
    /// tools, so a resolved robot missing any of them means the active
    /// catalog snapshot is outdated for the current standard set - this
    /// fails the whole launch plan with a direct remediation message rather
    /// than silently degrading (the old optional-tool behavior telemetry
    /// used to have).
    #[test]
    fn a_catalog_missing_a_standard_site_tool_fails_with_a_remediation_message()
    -> anyhow::Result<()> {
        let mut resolved = empty_resolved_robot("robot_v1")?;
        resolved.tools.push(tool(SITE_TOOL_BUS));
        resolved.tools.push(tool(SITE_TOOL_JOYPAD));
        // Telemetry deliberately left unresolved, as if the pinned catalog
        // snapshot predates it.
        let extras = RobotManifestExtras::default();
        let error = build_launch_plan(
            LaunchMode::Run,
            &[empty_checked_input(
                Path::new("/tmp/robot"),
                &resolved,
                &extras,
            )],
        )
        .expect_err("a missing standard site tool must fail the launch plan");
        let message = error.to_string();
        assert!(message.contains(SITE_TOOL_TELEMETRY), "{message}");
        assert!(message.contains("phoxal update"), "{message}");
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
            participant_kind: graph_check::ParticipantKind::Service,
            participant_class: graph_check::ParticipantClass::Checked,
            api_version: "v1".to_string(),
            config_schema: None,
            scope,
            contracts: Vec::new(),
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
            substitutions: &[],
            source_participants: &[],
        }
    }

    fn empty_resolved_robot(id: &str) -> anyhow::Result<ResolvedRobot> {
        let yaml = format!(
            r#"schema: robot/v0
robot:
  id: {id}
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
  components: {{}}
"#
        );
        let robot = phoxal::model::robot::v0::Robot::parse_from_string(&yaml)?;
        Ok(ResolvedRobot {
            robot,
            channel: SelectionChannel::Stable,
            target: host_target_triple_for_tests(),
            catalog_snapshot: None,
            platform_runtimes: Vec::new(),
            simulators: Vec::new(),
            user_runtimes: Vec::new(),
            components: Vec::new(),
            tools: Vec::new(),
            path_overrides: Vec::new(),
        })
    }

    fn add_site_tools(resolved: &mut ResolvedRobot) {
        resolved.tools.push(tool(SITE_TOOL_BUS));
        resolved.tools.push(tool(SITE_TOOL_JOYPAD));
        resolved.tools.push(tool(ROBOT_TOOL_LOG));
        resolved.tools.push(tool(SITE_TOOL_TELEMETRY));
    }

    fn tool(name: &str) -> ResolvedTool {
        ResolvedTool {
            kind: ArtifactKind::Tool,
            name: name.to_string(),
            package: format!("phoxal/{name}"),
            requested: "0.1.0".to_string(),
            resolved: "0.1.0".to_string(),
            repo: "phoxal/framework".to_string(),
            asset: format!("{name}-0.1.0-{}.tar.gz", host_target_triple_for_tests()),
            binary_name: name.to_string(),
            sha256: "0".repeat(64),
            url: None,
            size: None,
            published: false,
            path_override: None,
            channel: SelectionChannel::Stable,
            target: host_target_triple_for_tests(),
        }
    }

    fn host_target_triple_for_tests() -> String {
        crate::project::catalog::host_target_triple()
    }
}
