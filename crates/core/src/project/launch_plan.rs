//! Checked launch-plan construction for run and simulation sessions.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use phoxal::check as graph_check;
use phoxal::participant::launch::{
    BusProfile, ClockMode, DEFAULT_SHUTDOWN_GRACE_MS, ExecutionDeviceId, ParticipantLaunch,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::resolver::{ResolvedRobot, ResolvedTool};
use super::suite::ArtifactKind;
use crate::check::source::{SourceParticipant, SourceParticipantKind};
use crate::session::{RuntimeFailurePolicy, StartupRequirement};

pub const DEFAULT_ROUTER_CONNECT: &str = "tcp/localhost:7447";
pub const INFRASTRUCTURE_ROUTER: &str = "infrastructure-router";
pub const ROBOT_TOOL_JOYPAD: &str = "tool-joypad";
pub const ROBOT_TOOL_DEVICE: &str = "tool-device";
/// Base directory holding one staged runtime layout per target triple.
pub const RUNTIME_BUILD_ROOT_RELATIVE: &str = ".phoxal/build";
pub const SIMULATOR_SUPERVISOR_PROVIDER_ID: &str = "simulator-webots-supervisor";
pub const SIMULATOR_SUPERVISOR_ARTIFACT_NAME: &str = "webots-supervisor";
pub const SIMULATOR_CONTROLLER_ARTIFACT_NAME: &str = "webots-controller";

#[must_use]
pub fn simulator_controller_provider_id(robot_id: &str) -> String {
    format!("simulator-webots-controller-{robot_id}")
}

/// The staged runtime layout directory for `triple` under `project_root`:
/// `.phoxal/build/<triple>/`. `run` and live simulation stage and execute the
/// host triple; `build` stages any target into the same shape. This is the one
/// runtime-root the participant launch records point at.
#[must_use]
pub fn runtime_layout_dir(project_root: &Path, triple: &str) -> PathBuf {
    project_root.join(RUNTIME_BUILD_ROOT_RELATIVE).join(triple)
}

/// Mint the bounded observation identity shared by every per-robot
/// `tool-device` launched by one canonical project supervisor.
pub fn execution_device_id(project_root: &Path) -> Result<ExecutionDeviceId> {
    let canonical = project_root.canonicalize().unwrap_or_else(|_| {
        if project_root.is_absolute() {
            project_root.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(project_root))
                .unwrap_or_else(|_| project_root.to_path_buf())
        }
    });
    let digest = Sha256::digest(canonical.as_os_str().as_encoded_bytes());
    ExecutionDeviceId::new(format!("project-{}", &hex::encode(digest)[..24]))
        .map_err(|error| anyhow!("failed to mint execution-device identity: {error}"))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LaunchMode {
    Run,
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
/// matching re-declarations in `run`'s `PreparedRun` and the `watch` configs.
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
    pub robots: Vec<RobotLaunch>,
}

/// An immutable, content-identified compilation of the complete launch graph.
/// Watch rebuilds create a new value; a running revision is never edited.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanRevision {
    pub number: u64,
    pub digest: String,
    pub plan: LaunchPlan,
}

impl PlanRevision {
    pub fn compile(number: u64, plan: LaunchPlan) -> Result<Self> {
        anyhow::ensure!(number > 0, "plan revision numbers start at one");
        let process_count = plan
            .robots
            .iter()
            .map(|robot| robot.participants.len())
            .sum::<usize>()
            // Infrastructure router plus bounded supervisor-owned helpers.
            .saturating_add(4);
        crate::session::protocol::validate_snapshot_capacity(process_count)?;
        validate_supervisor_identity_bounds(&plan)?;
        let canonical = serde_json::to_vec(&plan)?;
        let digest = hex::encode(Sha256::digest(canonical));
        Ok(Self {
            number,
            digest,
            plan,
        })
    }

    #[must_use]
    pub fn content_path(&self, root: &Path, name: &str) -> PathBuf {
        root.join(".phoxal")
            .join("plans")
            .join("content")
            .join(name)
    }

    /// Publish content-addressed bytes without ever overwriting an existing
    /// artifact. The shared content store deliberately does not include the
    /// whole-plan digest: an unchanged binary keeps the same executable path
    /// across revisions, so reconciliation does not restart unrelated
    /// participants. A repeated identical write is idempotent; different
    /// bytes at the same content address are corruption and fail closed.
    pub fn publish_content(&self, root: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf> {
        anyhow::ensure!(
            !name.is_empty() && Path::new(name).components().count() == 1,
            "plan content name must be one path component"
        );
        let path = self.content_path(root, name);
        let parent = path.parent().expect("content path has parent");
        std::fs::create_dir_all(parent)?;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                use std::io::Write;
                file.write_all(bytes)?;
                file.sync_all()?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                anyhow::ensure!(
                    std::fs::read(&path)? == bytes,
                    "immutable plan content collision at {}",
                    path.display()
                );
            }
            Err(error) => return Err(error.into()),
        }
        Ok(path)
    }
}

fn validate_supervisor_identity_bounds(plan: &LaunchPlan) -> Result<()> {
    use crate::session::protocol::{MAX_ARTIFACT_ID_BYTES, MAX_SNAPSHOT_TEXT_BYTES};
    let bounded = |label: &str, value: &str, maximum: usize| -> Result<()> {
        anyhow::ensure!(
            value.len() <= maximum,
            "{label} is {} bytes; supervisor protocol v0 limit is {maximum}",
            value.len()
        );
        Ok(())
    };
    for robot in &plan.robots {
        bounded("robot id", &robot.id, MAX_SNAPSHOT_TEXT_BYTES)?;
        bounded("robot namespace", &robot.namespace, MAX_SNAPSHOT_TEXT_BYTES)?;
        for participant in &robot.participants {
            bounded(
                "participant process id",
                &participant.launch.participant_id,
                MAX_ARTIFACT_ID_BYTES,
            )?;
            bounded(
                "participant artifact id",
                &participant.artifact_id,
                MAX_ARTIFACT_ID_BYTES,
            )?;
            bounded(
                "participant robot id",
                &participant.launch.robot_id,
                MAX_SNAPSHOT_TEXT_BYTES,
            )?;
            bounded(
                "participant namespace",
                &participant.launch.namespace,
                MAX_SNAPSHOT_TEXT_BYTES,
            )?;
        }
    }
    Ok(())
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
    pub startup_requirement: StartupRequirement,
    pub runtime_failure: RuntimeFailurePolicy,
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
    let robots = robots
        .iter()
        .map(|robot| build_robot_launch(&mode, robot))
        .collect::<Result<Vec<_>>>()?;

    Ok(LaunchPlan { mode, robots })
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
    // A Suite-sourced component driver is a first-class suite artifact
    // too (docs #21): its `suite_runtime` projects onto the identical
    // `ResolvedPlatformRuntime` shape a service/simulator resolves to, keyed
    // here by the component id (`checked.artifact_id`) exactly like a
    // service is keyed by its own name.
    official_artifacts.extend(
        input
            .resolved
            .components
            .iter()
            .filter_map(|component| component.driver.as_ref())
            .filter_map(|driver| driver.suite_runtime.as_ref())
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
            startup_requirement: StartupRequirement::Required,
            runtime_failure: RuntimeFailurePolicy::StopProject,
        });
    }
    for tool in input
        .resolved
        .tools
        .iter()
        .filter(|tool| tool.kind == ArtifactKind::Tool)
    {
        let startup_requirement = StartupRequirement::Required;
        let runtime_failure = RuntimeFailurePolicy::StopProject;
        let execution_device_id = (tool.name == ROBOT_TOOL_DEVICE)
            .then(|| execution_device_id(input.project_root))
            .transpose()?;
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
                participant_id: format!("{}-{}", tool.name, input.resolved.robot.robot.id),
                incarnation: 0,
                namespace: input.resolved.robot.robot.namespace.clone(),
                robot_id: input.resolved.robot.robot.id.clone(),
                bus: BusProfile {
                    connect_endpoints: vec![DEFAULT_ROUTER_CONNECT.to_string()],
                },
                clock: ClockMode::Real,
                config: None,
                robot_root: Some(runtime_layout_dir(
                    input.project_root,
                    &input.resolved.target,
                )),
                component_instance: None,
                execution_device_id,
                shutdown_grace_ms: DEFAULT_SHUTDOWN_GRACE_MS,
            },
            launch_ownership: LaunchOwnership::CliManaged,
            startup_requirement,
            runtime_failure,
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
        incarnation: 0,
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
            LaunchMode::Run => ClockMode::Real,
            LaunchMode::Webots { .. } => ClockMode::Simulation,
        },
        config: input
            .resolved
            .robot
            .services
            .get(&checked.participant_id)
            .and_then(|service| service.config.clone()),
        robot_root: Some(runtime_layout_dir(
            input.project_root,
            &input.resolved.target,
        )),
        component_instance,
        execution_device_id: None,
        shutdown_grace_ms: DEFAULT_SHUTDOWN_GRACE_MS,
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

    use crate::project::resolver::{ResolvedRobot, ResolvedUserRuntime};
    use crate::project::suite::ArtifactKind;

    use super::*;

    #[test]
    fn plan_revisions_are_content_identified_and_never_overwritten() -> anyhow::Result<()> {
        let plan = LaunchPlan {
            mode: LaunchMode::Run,
            robots: Vec::new(),
        };
        let first = PlanRevision::compile(1, plan.clone())?;
        let second = PlanRevision::compile(2, plan)?;
        assert_eq!(first.digest, second.digest);
        let changed_plan = PlanRevision::compile(
            3,
            LaunchPlan {
                mode: LaunchMode::Webots {
                    world: PathBuf::from("/tmp/world.wbt"),
                },
                robots: Vec::new(),
            },
        )?;
        assert_ne!(first.digest, changed_plan.digest);
        assert_eq!(
            first.content_path(Path::new("/tmp/project"), "participant"),
            changed_plan.content_path(Path::new("/tmp/project"), "participant")
        );
        let temp = tempfile::tempdir()?;
        let path = first.publish_content(temp.path(), "participant", b"revision-one")?;
        assert_eq!(std::fs::read(&path)?, b"revision-one");
        first.publish_content(temp.path(), "participant", b"revision-one")?;
        assert!(
            first
                .publish_content(temp.path(), "participant", b"mutated")
                .is_err()
        );
        assert_eq!(std::fs::read(path)?, b"revision-one");
        Ok(())
    }

    #[test]
    fn launch_plan_covers_per_robot_tools_and_user_service_config() -> anyhow::Result<()> {
        let mut resolved = empty_resolved_robot("robot_v1")?;
        add_robot_tools(&mut resolved);
        resolved.user_runtimes.push(ResolvedUserRuntime {
            name: "mission".to_string(),
            path: PathBuf::from("runtimes/mission"),
            source_hash: "hash".to_string(),
        });
        resolved.robot.services.insert(
            "mission".to_string(),
            phoxal::model::robot::v0::UserService {
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
                checked_participants: &checked,
                substitutions: &[],
                source_participants: &sources,
            }],
        )?;

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
            vec![
                "tool-bus-robot_v1",
                "tool-device-robot_v1",
                "tool-joypad-robot_v1",
                "tool-log-robot_v1",
                "tool-telemetry-robot_v1",
            ]
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
    fn run_robot_tools_have_unique_participant_ids_per_robot() -> anyhow::Result<()> {
        let mut robot_a = empty_resolved_robot("robot_a")?;
        let mut robot_b = empty_resolved_robot("robot_b")?;
        add_robot_tools(&mut robot_a);
        add_robot_tools(&mut robot_b);
        let inputs = [
            empty_checked_input(Path::new("/tmp/project"), &robot_a),
            empty_checked_input(Path::new("/tmp/project"), &robot_b),
        ];

        let plan = build_launch_plan(LaunchMode::Run, &inputs)?;
        let ids = plan
            .robots
            .iter()
            .flat_map(|robot| {
                robot
                    .participants
                    .iter()
                    .map(|participant| participant.launch.participant_id.as_str())
            })
            .collect::<Vec<_>>();
        assert!(ids.contains(&"tool-bus-robot_a"));
        assert!(ids.contains(&"tool-log-robot_a"));
        assert!(ids.contains(&"tool-joypad-robot_a"));
        assert!(ids.contains(&"tool-bus-robot_b"));
        assert!(ids.contains(&"tool-log-robot_b"));
        assert!(ids.contains(&"tool-joypad-robot_b"));
        let devices = plan
            .robots
            .iter()
            .map(|robot| {
                robot
                    .participants
                    .iter()
                    .find(|participant| participant.artifact_id == ROBOT_TOOL_DEVICE)
                    .expect("per-robot device activation")
            })
            .collect::<Vec<_>>();
        assert_eq!(devices.len(), 2);
        assert_eq!(
            devices[0].launch.execution_device_id, devices[1].launch.execution_device_id,
            "co-hosted robot samplers must expose one project device identity"
        );
        assert_eq!(devices[0].startup_requirement, StartupRequirement::Required);
        assert_eq!(
            devices[0].runtime_failure,
            RuntimeFailurePolicy::StopProject
        );
        Ok(())
    }

    #[test]
    fn execution_device_identity_is_project_scoped_stable_and_bounded() -> anyhow::Result<()> {
        let first = tempfile::tempdir()?;
        let second = tempfile::tempdir()?;
        let first_identity = execution_device_id(first.path())?;

        assert_eq!(first_identity, execution_device_id(first.path())?);
        assert_ne!(first_identity, execution_device_id(second.path())?);
        assert!(
            first_identity.to_string().len()
                <= phoxal::participant::launch::MAX_EXECUTION_DEVICE_ID_BYTES
        );
        Ok(())
    }

    #[test]
    fn webots_launch_records_need_no_simulator_clock_exception() -> anyhow::Result<()> {
        let resolved = empty_resolved_robot("robot_v1")?;
        let input = CheckedRobotLaunchInput {
            project_root: Path::new("/tmp/robot"),
            resolved: &resolved,
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
        add_robot_tools(&mut resolved);
        resolved.user_runtimes.push(ResolvedUserRuntime {
            name: "mission".to_string(),
            path: PathBuf::from("runtimes/mission"),
            source_hash: "hash".to_string(),
        });
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
    ) -> CheckedRobotLaunchInput<'a> {
        CheckedRobotLaunchInput {
            project_root,
            resolved,
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
            train: "0.36.0".to_string(),
            target: host_target_triple_for_tests(),
            platform_runtimes: Vec::new(),
            simulators: Vec::new(),
            user_runtimes: Vec::new(),
            components: Vec::new(),
            tools: Vec::new(),
            path_overrides: Vec::new(),
        })
    }

    fn add_robot_tools(resolved: &mut ResolvedRobot) {
        resolved.tools.push(tool("tool-bus"));
        resolved.tools.push(tool("tool-joypad"));
        resolved.tools.push(tool("tool-log"));
        resolved.tools.push(tool("tool-telemetry"));
        resolved.tools.push(tool(ROBOT_TOOL_DEVICE));
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
            train: "0.36.0".to_string(),
            target: host_target_triple_for_tests(),
        }
    }

    fn host_target_triple_for_tests() -> String {
        crate::project::suite::host_target_triple()
    }
}
