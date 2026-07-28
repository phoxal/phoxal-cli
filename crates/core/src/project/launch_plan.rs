//! Checked launch-plan construction for run and simulation sessions.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::identity::{ExecutionId, ProducerId};
use anyhow::{Result, bail};
use phoxal::check as graph_check;
use phoxal::participant::ExecutionOrigin;
use phoxal::participant::launch::{
    BusProfile, ClockMode, DEFAULT_SHUTDOWN_GRACE_MS, ParticipantLaunch,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::catalog::ArtifactKind;
use super::resolver::{ResolvedRobot, official_binary_name};
use crate::check::source::{SourceParticipant, SourceParticipantKind};
use crate::session::{RuntimeFailurePolicy, StartupRequirement};

pub const DEFAULT_ROUTER_CONNECT: &str = "tcp/localhost:7447";
pub const ROBOT_TOOL_JOYPAD: &str = "tool-joypad";
pub const ROBOT_TOOL_DEVICE: &str = "tool-device";
/// The staged runtime layout / `cargo install --root` directory
/// (organization#951 WS4). No per-triple nesting: one robot targets one
/// platform at a time, so a second `--target` simply restages this same root.
pub const RUNTIME_BUNDLE_ROOT_RELATIVE: &str = ".phoxal/bundle";
/// The Webots controller's own materialization root - deliberately separate
/// from `.phoxal/bundle/`: the controller is built only when a simulation is
/// requested, and must never enter the deployed robot bundle
/// (organization#951 WS4).
pub const RUNTIME_SIMULATION_ROOT_RELATIVE: &str = ".phoxal/simulation";
pub const SIMULATOR_CONTROLLER_ARTIFACT_NAME: &str = "webots-controller";

/// The simulation materialization root - `cargo install --root`'s target for
/// the Webots controller - under `project_root`.
#[must_use]
pub fn simulation_root_dir(project_root: &Path) -> PathBuf {
    project_root.join(RUNTIME_SIMULATION_ROOT_RELATIVE)
}

#[must_use]
pub fn simulator_controller_provider_id(robot_id: &str) -> String {
    format!("simulator-webots-controller-{robot_id}")
}

/// The staged runtime layout directory under `project_root`:
/// `.phoxal/bundle/`. `run`, live simulation, and `build` all stage into this
/// one root - this is the one runtime-root the participant launch records
/// point at.
#[must_use]
pub fn runtime_layout_dir(project_root: &Path) -> PathBuf {
    project_root.join(RUNTIME_BUNDLE_ROOT_RELATIVE)
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
/// matching re-declarations in `run`'s `PreparedRun`.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanContext {
    pub robot_path: PathBuf,
    pub project_root: PathBuf,
    /// The resolved source graph and its source-participant records - present
    /// only when the plan was prepared from a source project. A layout run (an
    /// extracted `build.phoxal` or a staged `.phoxal/bundle/` root) has
    /// no source, so this is `None` there; a consumer that needs source state
    /// (such as simulation) checks this directly rather than reading a
    /// fabricated graph (#936).
    pub source: Option<PlanSource>,
}

/// The source-only half of a [`PlanContext`].
#[derive(Debug, Clone, PartialEq)]
pub struct PlanSource {
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
    /// The digest of the plan's *content* - what to launch - and deliberately
    /// not of the run identities it carries. Two builds of the same layout
    /// digest identically even though every supervised run mints a fresh
    /// `ExecutionId` (#952 section B).
    pub digest: String,
    pub plan: LaunchPlan,
}

impl PlanRevision {
    pub fn compile(number: u64, plan: LaunchPlan) -> Result<Self> {
        anyhow::ensure!(number > 0, "plan revision numbers start at one");
        validate_runtime_bounds(&plan)?;
        // The digest is the plan's *content* - what to launch - so it excludes
        // the run identities, which say *which run* rather than what (#952
        // section B). Two builds of the same layout must digest identically
        // even though every supervised run mints fresh identities.
        let canonical = serde_json::to_vec(&content_only(plan.clone()))?;
        let digest = hex::encode(Sha256::digest(canonical));
        Ok(Self {
            number,
            digest,
            plan,
        })
    }

    #[must_use]
    pub fn content_path_in(&self, content_root: &Path, name: &str) -> PathBuf {
        content_root.join(name)
    }

    /// Publish content-addressed bytes without ever overwriting an existing
    /// artifact. The shared content store deliberately does not include the
    /// whole-plan digest: an unchanged binary keeps the same executable path
    /// across revisions, so reconciliation does not restart unrelated
    /// participants. A repeated identical write is idempotent; different
    /// bytes at the same content address are corruption and fail closed.
    pub fn publish_content_in(
        &self,
        content_root: &Path,
        name: &str,
        bytes: &[u8],
    ) -> Result<PathBuf> {
        anyhow::ensure!(
            !name.is_empty() && Path::new(name).components().count() == 1,
            "plan content name must be one path component"
        );
        let path = self.content_path_in(content_root, name);
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

/// Reject a launch graph that cannot be represented by runtime state.
///
/// Preparation calls this before publishing participant rows; revision
/// compilation repeats it as the domain-level invariant at its final choke
/// point.
pub fn validate_runtime_bounds(plan: &LaunchPlan) -> Result<()> {
    let process_count = plan
        .robots
        .iter()
        .map(|robot| robot.participants.len())
        .sum::<usize>()
        // Infrastructure router plus bounded supervisor-owned helpers.
        .saturating_add(4);
    anyhow::ensure!(
        process_count <= crate::runtime::MAX_SUPERVISED_PROCESSES,
        "execution plan has {process_count} supervised processes; runtime supports at most {}",
        crate::runtime::MAX_SUPERVISED_PROCESSES
    );
    let bounded = |label: &str, value: &str, maximum: usize| -> Result<()> {
        anyhow::ensure!(
            value.len() <= maximum,
            "{label} is {} bytes; runtime limit is {maximum}",
            value.len()
        );
        Ok(())
    };
    for robot in &plan.robots {
        bounded(
            "robot id",
            &robot.id,
            crate::runtime::MAX_RUNTIME_TEXT_BYTES,
        )?;
        bounded(
            "robot namespace",
            &robot.namespace,
            crate::runtime::MAX_RUNTIME_TEXT_BYTES,
        )?;
        for participant in &robot.participants {
            bounded(
                "participant process id",
                &participant.launch.participant_id,
                crate::runtime::MAX_RUNTIME_ARTIFACT_ID_BYTES,
            )?;
            bounded(
                "participant artifact id",
                &participant.artifact_id,
                crate::runtime::MAX_RUNTIME_ARTIFACT_ID_BYTES,
            )?;
            bounded(
                "participant robot id",
                &participant.launch.robot_id,
                crate::runtime::MAX_RUNTIME_TEXT_BYTES,
            )?;
            bounded(
                "participant namespace",
                &participant.launch.namespace,
                crate::runtime::MAX_RUNTIME_TEXT_BYTES,
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParticipantLaunchRecord {
    pub artifact_id: String,
    pub execution: ParticipantExecution,
    pub launch: ParticipantLaunch,
    pub startup_requirement: StartupRequirement,
    pub runtime_failure: RuntimeFailurePolicy,
}

/// How a launched participant's binary is identified in the staged runtime
/// layout. Re-keyed on the canonical flat-`bin/` file name (#936): a plan built
/// from a staged layout carries no source path, so the same robot produces a
/// byte-identical plan whether the layout was just staged from a source project
/// or extracted from a `build.phoxal` bundle. The role classifies board kind,
/// launch env, and telemetry; `binary_name` is the flat `bin/` lookup key the
/// loader resolves.
///
/// Source-specific data - the Cargo crate directory a participant is rebuilt
/// and run from mutable source state - deliberately does NOT live here. An extracted
/// bundle has no crate directories at all, so keeping them out of the execution
/// identity is what makes source and bundle plans identical. The source-staging
/// path recovers a crate directory from its own resolved graph
/// (`ResolvedRobot`) and source-participant records when it needs to rebuild;
/// the plan only ever names the `bin/` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "execution", rename_all = "snake_case")]
pub enum ParticipantExecution {
    /// An official platform artifact - a service or a Webots simulator,
    /// vendored or built from a workspace override - resolved from
    /// `bin/<binary_name>`.
    OfficialArtifact { binary_name: String },
    /// A privileged official tool, vendored or overridden, resolved from
    /// `bin/<binary_name>`.
    OfficialTool { binary_name: String },
    /// A user service, resolved from `bin/<binary_name>`.
    UserService { binary_name: String },
    /// A declared additional user tool (`tools:` in robot.yaml, #950),
    /// resolved from `bin/<binary_name>`.
    UserTool { binary_name: String },
    /// A component driver - one binary serving every instance of a component
    /// id - resolved from `bin/<binary_name>`.
    ComponentDriver { binary_name: String },
}

impl ParticipantExecution {
    /// The canonical flat-`bin/` file name this participant's binary is
    /// resolved under. Identical across every path that produces the same
    /// robot's plan, so it is the sole execution identity a layout needs.
    #[must_use]
    pub fn binary_name(&self) -> &str {
        match self {
            Self::OfficialArtifact { binary_name }
            | Self::OfficialTool { binary_name }
            | Self::UserService { binary_name }
            | Self::UserTool { binary_name }
            | Self::ComponentDriver { binary_name } => binary_name,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CheckedRobotLaunchInput<'a> {
    pub project_root: &'a Path,
    pub resolved: &'a ResolvedRobot,
    pub checked_participants: &'a [graph_check::ParticipantApis],
    pub source_participants: &'a [SourceParticipant],
}

pub fn build_launch_plan(
    mode: LaunchMode,
    robots: &[CheckedRobotLaunchInput<'_>],
    run: RunIdentity,
) -> Result<LaunchPlan> {
    if robots.is_empty() {
        bail!("LaunchPlan requires at least one robot");
    }
    let robots = robots
        .iter()
        .map(|robot| build_robot_launch(&mode, robot, run))
        .collect::<Result<Vec<_>>>()?;

    Ok(LaunchPlan { mode, robots })
}

/// Erase the per-run identities so two plans can be compared for content.
///
/// A supervised run mints a fresh `ExecutionId`, a fresh `ExecutionOrigin`, and
/// one `ProducerId` per participant, so no two plan builds ever agree on them -
/// and none of them describes *what* the plan launches.
#[must_use]
pub fn content_only(mut plan: LaunchPlan) -> LaunchPlan {
    let fixed_execution =
        ExecutionId::parse(&"0".repeat(ExecutionId::LEN)).expect("fixed execution id");
    let fixed_producer =
        ProducerId::parse(&"0".repeat(ExecutionId::LEN)).expect("fixed producer id");
    for robot in &mut plan.robots {
        for participant in &mut robot.participants {
            participant.launch.execution = fixed_execution;
            participant.launch.producer = fixed_producer;
            participant.launch.execution_origin = None;
        }
    }
    plan
}

/// The identities one supervised run shares across every participant it
/// launches (#952 sections B and I).
///
/// The supervisor mints these once per run. Every participant in the plan
/// carries them, so the bus root is execution-scoped and traffic from a
/// previous run cannot be observed as current; each participant still gets its
/// own [`ProducerId`].
#[derive(Clone, Copy, Debug)]
pub struct RunIdentity {
    execution: ExecutionId,
    origin: ExecutionOrigin,
}

impl RunIdentity {
    /// Adopt `execution` if a launcher already minted one for this run, or mint
    /// a fresh identity. The origin of real robot time is always minted here,
    /// by the process that supervises the run.
    #[must_use]
    pub fn mint_or_adopt(execution: Option<ExecutionId>) -> Self {
        RunIdentity {
            execution: execution.unwrap_or_else(ExecutionId::mint),
            origin: ExecutionOrigin::mint(),
        }
    }

    /// The supervised run.
    #[must_use]
    pub fn execution(&self) -> ExecutionId {
        self.execution
    }

    /// The origin of real robot time for this run.
    #[must_use]
    pub fn origin(&self) -> ExecutionOrigin {
        self.origin
    }
}

impl Default for RunIdentity {
    fn default() -> Self {
        Self::mint_or_adopt(None)
    }
}

/// The kind-stripped short name of an official tool artifact id
/// (`tool-bus` -> `bus`), matching the CLI catalog's short name and so the
/// canonical `bin/` binary the loader resolves the tool under.
fn tool_short_name(artifact_id: &str) -> &str {
    artifact_id.strip_prefix("tool-").unwrap_or(artifact_id)
}

fn build_robot_launch(
    mode: &LaunchMode,
    input: &CheckedRobotLaunchInput<'_>,
    run: RunIdentity,
) -> Result<RobotLaunch> {
    ensure_launch_set_parity(mode, input)?;
    let source_participants = input
        .source_participants
        .iter()
        .map(|participant| (participant.name.as_str(), participant))
        .collect::<BTreeMap<_, _>>();
    // The kind of every non-driver official the checked set can reference, so
    // the plan can name each one's canonical `bin/` binary
    // (`official_binary_name(kind, artifact_id)`). Suite-sourced component
    // drivers are NOT included here: they carry `ComponentInstance` scope and
    // are keyed uniformly by their component id through the driver branch of
    // `participant_execution`, exactly like a workspace-built driver.
    let official_kinds = input
        .resolved
        .platform_runtimes
        .iter()
        .chain(input.resolved.simulators.iter())
        .map(|runtime| (runtime.name.as_str(), runtime.kind))
        .collect::<BTreeMap<_, _>>();

    let mut participants = Vec::new();
    for checked in input
        .checked_participants
        .iter()
        .filter(|participant| is_robot_launch_participant(mode, participant, &source_participants))
    {
        let execution = participant_execution(checked, &source_participants, &official_kinds)?;
        let launch = participant_launch(mode, input, checked, run);
        participants.push(ParticipantLaunchRecord {
            artifact_id: checked.artifact_id.clone(),
            execution,
            launch,
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
        participants.push(ParticipantLaunchRecord {
            artifact_id: tool.name.clone(),
            // Vendored or workspace-overridden, a tool resolves to the one
            // canonical `bin/` entry the loader names; whether it was rebuilt
            // from a crate is recovered from the resolved graph at staging,
            // never encoded in the source-free plan (#936).
            execution: ParticipantExecution::OfficialTool {
                binary_name: official_binary_name(ArtifactKind::Tool, tool_short_name(&tool.name)),
            },
            launch: ParticipantLaunch {
                participant_id: format!("{}-{}", tool.name, input.resolved.robot.robot.id),
                execution: run.execution(),
                producer: ProducerId::mint(),
                execution_origin: Some(run.origin()),
                namespace: input.resolved.robot.robot.namespace.clone(),
                robot_id: input.resolved.robot.robot.id.clone(),
                bus: BusProfile {
                    connect_endpoints: vec![DEFAULT_ROUTER_CONNECT.to_string()],
                },
                clock: ClockMode::Real,
                config: None,
                robot_root: Some(runtime_layout_dir(input.project_root)),
                component_instance: None,
                shutdown_grace_ms: DEFAULT_SHUTDOWN_GRACE_MS,
            },
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
    })
}

fn is_robot_launch_participant(
    mode: &LaunchMode,
    participant: &graph_check::ParticipantApis,
    source_participants: &BTreeMap<&str, &SourceParticipant>,
) -> bool {
    if !participant.participant_class.is_checked() {
        return false;
    }
    if participant.participant_kind == graph_check::ParticipantKind::Tool {
        return source_participants
            .get(participant.participant_id.as_str())
            .is_some_and(|source| source.kind == SourceParticipantKind::UserTool);
    }
    if participant.participant_kind == graph_check::ParticipantKind::Simulator {
        // Webots owns its controller process. Simulator artifacts participate
        // in compile-time graph validation and content staging, never in the
        // resident launch plan or supervisor registry.
        return false;
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

fn participant_execution(
    checked: &graph_check::ParticipantApis,
    source_participants: &BTreeMap<&str, &SourceParticipant>,
    official_kinds: &BTreeMap<&str, ArtifactKind>,
) -> Result<ParticipantExecution> {
    // A component-instance-scoped participant is a driver: one binary named by
    // its component id serves every instance, whether it is a workspace-built
    // (source) driver or a registry-materialized one. The layout cannot tell
    // the two apart - both are `bin/phoxal-component-<id>` - so neither does
    // the plan.
    if let graph_check::ParticipantScope::ComponentInstance(_) = checked.scope {
        return Ok(ParticipantExecution::ComponentDriver {
            binary_name: official_binary_name(ArtifactKind::ComponentDriver, &checked.artifact_id),
        });
    }
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
                binary_name: checked.artifact_id.clone(),
            },

            SourceParticipantKind::OfficialService => ParticipantExecution::OfficialArtifact {
                binary_name: official_binary_name(ArtifactKind::Service, &checked.artifact_id),
            },
            SourceParticipantKind::Simulator => ParticipantExecution::OfficialArtifact {
                binary_name: official_binary_name(ArtifactKind::Simulator, &checked.artifact_id),
            },
            SourceParticipantKind::UserTool => ParticipantExecution::UserTool {
                binary_name: checked.artifact_id.clone(),
            },
            // Component drivers are handled by the component-instance branch
            // above; official tools are supplied by `resolved.tools`.
            SourceParticipantKind::ComponentDriver | SourceParticipantKind::Tool => bail!(
                "source participant {} of kind {:?} is not a launchable non-driver participant",
                source.name,
                source.kind
            ),
        });
    }
    if let Some(kind) = official_kinds.get(checked.artifact_id.as_str()) {
        return Ok(ParticipantExecution::OfficialArtifact {
            binary_name: official_binary_name(*kind, &checked.artifact_id),
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
    run: RunIdentity,
) -> ParticipantLaunch {
    let component_instance = match &checked.scope {
        graph_check::ParticipantScope::ComponentInstance(instance) => Some(instance.clone()),
        graph_check::ParticipantScope::Graph => None,
    };
    ParticipantLaunch {
        participant_id: checked.participant_id.clone(),
        execution: run.execution(),
        producer: ProducerId::mint(),
        execution_origin: Some(run.origin()),
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
            .and_then(|service| service.config.clone())
            .or_else(|| {
                input
                    .resolved
                    .robot
                    .tools
                    .get(&checked.participant_id)
                    .and_then(|tool| tool.config.clone())
            }),
        robot_root: Some(runtime_layout_dir(input.project_root)),
        component_instance,
        shutdown_grace_ms: DEFAULT_SHUTDOWN_GRACE_MS,
    }
}

fn ensure_launch_set_parity(mode: &LaunchMode, input: &CheckedRobotLaunchInput<'_>) -> Result<()> {
    let expected = expected_checked_participant_ids(mode, input.resolved);
    let source_participants = input
        .source_participants
        .iter()
        .map(|participant| (participant.name.as_str(), participant))
        .collect::<BTreeMap<_, _>>();
    let checked = input
        .checked_participants
        .iter()
        .filter(|participant| is_robot_launch_participant(mode, participant, &source_participants))
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
    expected.extend(
        resolved
            .user_tools
            .iter()
            .map(|runtime| runtime.name.clone()),
    );
    if !matches!(mode, LaunchMode::Webots { .. }) {
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

#[cfg(test)]
mod tests {
    /// #952 section B, the rows the supervisor owns: a new run is a new
    /// execution and a new origin, while a resident that was handed one adopts
    /// exactly that execution rather than minting its own.
    ///
    /// The origin is always minted locally, even on adoption: it is this
    /// host's boot-clock anchor, and only the process that supervises the run
    /// can take it.
    #[test]
    fn a_new_run_mints_a_fresh_identity_and_an_adopted_one_is_kept_exactly() {
        let first = RunIdentity::mint_or_adopt(None);
        let second = RunIdentity::mint_or_adopt(None);
        assert_ne!(
            first.execution(),
            second.execution(),
            "supervisor restart, new run, and rollback are each a new execution"
        );
        assert_ne!(
            first.origin().timeline(),
            second.origin().timeline(),
            "a new run is a new world history too"
        );

        let launcher = ExecutionId::mint();
        let adopted = RunIdentity::mint_or_adopt(Some(launcher));
        assert_eq!(
            adopted.execution(),
            launcher,
            "a detached resident adopts the launcher's run, it does not start its own"
        );
    }

    use std::path::Path;

    use crate::project::catalog::ArtifactKind;
    use crate::project::resolver::{
        ResolvedComponent, ResolvedComponentPackage, ResolvedComponentSource,
        ResolvedPlatformRuntime, ResolvedRobot, ResolvedTool, ResolvedUserRuntime,
    };

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
        let content_root = Path::new("/tmp/project/content");
        assert_eq!(
            first.content_path_in(content_root, "participant"),
            changed_plan.content_path_in(content_root, "participant")
        );
        let temp = tempfile::tempdir()?;
        let path = first.publish_content_in(temp.path(), "participant", b"revision-one")?;
        assert_eq!(std::fs::read(&path)?, b"revision-one");
        first.publish_content_in(temp.path(), "participant", b"revision-one")?;
        assert!(
            first
                .publish_content_in(temp.path(), "participant", b"mutated")
                .is_err()
        );
        assert_eq!(std::fs::read(path)?, b"revision-one");
        Ok(())
    }

    #[test]
    fn reference_graph_process_count_fits_runtime_bound() {
        let participant = |index: usize| ParticipantLaunchRecord {
            artifact_id: format!("artifact-{index}"),
            execution: ParticipantExecution::UserService {
                binary_name: format!("participant-{index}"),
            },
            launch: ParticipantLaunch {
                participant_id: format!("participant-{index}"),
                execution: ExecutionId::mint(),
                producer: ProducerId::mint(),
                execution_origin: None,
                namespace: "dev".to_string(),
                robot_id: "testbot".to_string(),
                bus: BusProfile {
                    connect_endpoints: vec![DEFAULT_ROUTER_CONNECT.to_string()],
                },
                clock: ClockMode::Real,
                config: None,
                robot_root: Some(PathBuf::from("/var/phoxal")),
                component_instance: None,
                shutdown_grace_ms: DEFAULT_SHUTDOWN_GRACE_MS,
            },
            startup_requirement: StartupRequirement::Required,
            runtime_failure: RuntimeFailurePolicy::StopProject,
        };
        let plan = |participant_count: usize| LaunchPlan {
            mode: LaunchMode::Run,
            robots: vec![RobotLaunch {
                id: "testbot".to_string(),
                namespace: "dev".to_string(),
                participants: (0..participant_count).map(participant).collect(),
            }],
        };

        PlanRevision::compile(1, plan(36)).expect("40-process reference graph should fit");
        let error =
            PlanRevision::compile(1, plan(37)).expect_err("41 processes must remain bounded");
        assert!(
            error.to_string().contains("runtime supports at most 40"),
            "{error:#}"
        );
    }

    #[test]
    fn launch_plan_covers_per_robot_tools_and_user_service_config() -> anyhow::Result<()> {
        let mut resolved = empty_resolved_robot("testbot")?;
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
                source_participants: &sources,
            }],
            RunIdentity::default(),
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
                "tool-bus-testbot",
                "tool-device-testbot",
                "tool-joypad-testbot",
                "tool-log-testbot",
                "tool-telemetry-testbot",
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
    fn webots_plan_keeps_declared_user_tools_in_the_resident_graph() -> anyhow::Result<()> {
        let mut resolved = empty_resolved_robot("testbot")?;
        resolved.simulators.push(platform_runtime(
            "webots-controller",
            ArtifactKind::Simulator,
        ));
        resolved.user_tools.push(ResolvedUserRuntime {
            name: "console".to_string(),
            path: PathBuf::from("tools/console"),
            source_hash: "hash".to_string(),
        });
        resolved.robot.tools.insert(
            "console".to_string(),
            phoxal::model::robot::v0::UserTool {
                config: Some(serde_json::json!({"rate": 20})),
            },
        );
        let sources = vec![SourceParticipant::user_tool(
            "console",
            PathBuf::from("/tmp/console"),
        )];
        let mut tool = participant("console", "console", graph_check::ParticipantScope::Graph);
        tool.participant_kind = graph_check::ParticipantKind::Tool;
        let controller_id = simulator_controller_provider_id("testbot");
        let mut controller = participant(
            &controller_id,
            SIMULATOR_CONTROLLER_ARTIFACT_NAME,
            graph_check::ParticipantScope::Graph,
        );
        controller.participant_kind = graph_check::ParticipantKind::Simulator;
        let checked = [tool, controller];
        let plan = build_launch_plan(
            LaunchMode::Webots {
                world: PathBuf::from("/tmp/default.wbt"),
            },
            &[CheckedRobotLaunchInput {
                project_root: Path::new("/tmp/robot"),
                resolved: &resolved,
                checked_participants: &checked,
                source_participants: &sources,
            }],
            RunIdentity::default(),
        )?;

        assert_eq!(
            plan.robots[0].participants.len(),
            1,
            "the Webots controller is compile-time metadata, never a resident process"
        );
        let console = &plan.robots[0].participants[0];
        assert_eq!(
            console.execution,
            ParticipantExecution::UserTool {
                binary_name: "console".to_string()
            }
        );
        assert_eq!(console.launch.config, Some(serde_json::json!({"rate": 20})));
        assert!(
            !plan.robots[0]
                .participants
                .iter()
                .any(|participant| participant.launch.participant_id == controller_id)
        );
        Ok(())
    }

    #[test]
    fn webots_excludes_physical_drivers_from_expected_and_resident_sets() -> anyhow::Result<()> {
        let mut resolved = empty_resolved_robot("testbot")?;
        let component_package = || ResolvedComponentPackage {
            package: "phoxal/component-ddsm115".to_string(),
            kind: ArtifactKind::ComponentAssets,
            source: ResolvedComponentSource::Path {
                path: PathBuf::from("/tmp/ddsm115"),
            },
            resolved_dir: Some(PathBuf::from("/tmp/ddsm115")),
            registry_runtime: None,
        };
        resolved.components.push(ResolvedComponent {
            instance: "left_drive".to_string(),
            source_name: "ddsm115".to_string(),
            assets: component_package(),
            driver: Some(ResolvedComponentPackage {
                kind: ArtifactKind::ComponentDriver,
                ..component_package()
            }),
            has_driver: true,
        });
        let mut driver = participant(
            "left_drive",
            "ddsm115",
            graph_check::ParticipantScope::Graph,
        );
        driver.participant_kind = graph_check::ParticipantKind::Driver;
        let source_participants = BTreeMap::new();
        let webots = LaunchMode::Webots {
            world: PathBuf::from("/tmp/default.wbt"),
        };
        assert!(!is_robot_launch_participant(
            &webots,
            &driver,
            &source_participants
        ));
        assert!(is_robot_launch_participant(
            &LaunchMode::Run,
            &driver,
            &source_participants
        ));
        assert!(!expected_checked_participant_ids(&webots, &resolved).contains("left_drive"));
        assert!(
            expected_checked_participant_ids(&LaunchMode::Run, &resolved).contains("left_drive")
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

        let plan = build_launch_plan(LaunchMode::Run, &inputs, RunIdentity::default())?;
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
            devices[0].launch.execution, devices[1].launch.execution,
            "co-hosted robot samplers belong to one supervised run"
        );
        assert_ne!(
            devices[0].launch.producer, devices[1].launch.producer,
            "each participant is its own producer"
        );
        assert_eq!(devices[0].startup_requirement, StartupRequirement::Required);
        assert_eq!(
            devices[0].runtime_failure,
            RuntimeFailurePolicy::StopProject
        );
        Ok(())
    }

    /// The `/var/phoxal -> releases/<ts>/` deployment model (#930): the device
    /// identity is taken from the *logical* symlink path, so retargeting the
    /// symlink to a new release directory keeps the same identity - observation
    /// continuity survives an activation. A genuinely different logical root is
    /// still a different device.
    #[cfg(unix)]
    #[test]
    fn webots_launch_records_need_no_simulator_clock_exception() -> anyhow::Result<()> {
        let resolved = empty_resolved_robot("testbot")?;
        let input = CheckedRobotLaunchInput {
            project_root: Path::new("/tmp/robot"),
            resolved: &resolved,
            checked_participants: &[],
            source_participants: &[],
        };
        let mode = LaunchMode::Webots {
            world: PathBuf::from("worlds/default.wbt"),
        };
        let service = participant("mission", "mission", graph_check::ParticipantScope::Graph);

        let participant_id = "simulator-webots-controller-testbot";
        let mut simulator = participant(
            participant_id,
            "webots-controller",
            graph_check::ParticipantScope::Graph,
        );
        simulator.participant_kind = graph_check::ParticipantKind::Simulator;
        assert_eq!(
            participant_launch(&mode, &input, &simulator, RunIdentity::default()).clock,
            ClockMode::Simulation,
            "{participant_id} uses the mode-wide record; its clockless binary policy ignores it"
        );
        assert_eq!(
            participant_launch(&mode, &input, &service, RunIdentity::default()).clock,
            ClockMode::Simulation,
            "services in simulation must advance from published world steps"
        );
        Ok(())
    }

    #[test]
    fn parity_rejects_missing_and_extra_checked_metadata() -> anyhow::Result<()> {
        let mut resolved = empty_resolved_robot("testbot")?;
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
                source_participants: &sources,
            }],
            RunIdentity::default(),
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
            config_schema: None,
            scope,
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
            user_tools: Vec::new(),
            undeclared_runtimes: Vec::new(),
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

    fn platform_runtime(name: &str, kind: ArtifactKind) -> ResolvedPlatformRuntime {
        ResolvedPlatformRuntime {
            name: name.to_string(),
            package: format!("phoxal/simulator-{name}"),
            kind,
            path_override: None,
            train: "0.36.0".to_string(),
            target: Some(host_target_triple_for_tests()),
        }
    }

    fn tool(name: &str) -> ResolvedTool {
        ResolvedTool {
            kind: ArtifactKind::Tool,
            name: name.to_string(),
            package: format!("phoxal/{name}"),
            binary_name: name.to_string(),
            path_override: None,
            train: "0.36.0".to_string(),
            target: host_target_triple_for_tests(),
        }
    }

    fn host_target_triple_for_tests() -> String {
        crate::project::host_target_triple()
    }
}
