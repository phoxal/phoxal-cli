//! Checked launch-plan construction for run and simulation sessions.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::check as graph_check;
use crate::identity::{ExecutionId, ProducerId};
use anyhow::{Result, bail};
use phoxal_runtime_contract::ExecutionOrigin;
use phoxal_runtime_contract::{
    BusProfile, ClockMode, DEFAULT_SHUTDOWN_GRACE_MS, ParticipantLaunch,
};
use serde::{Deserialize, Serialize};

use super::catalog::ArtifactKind;
use super::resolver::{BundlePlan, official_binary_name};
use crate::check::source::{SourceParticipant, SourceParticipantKind};
use crate::runtime::{RuntimeFailurePolicy, StartupRequirement};

pub const DEFAULT_ROUTER_CONNECT: &str = "tcp/localhost:7447";
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
    pub resolved: BundlePlan,
    pub source_participants: Vec<SourceParticipant>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaunchPlan {
    pub mode: LaunchMode,
    pub robots: Vec<RobotLaunch>,
}

/// Reject a launch graph that cannot be represented by runtime state.
pub(crate) fn validate_runtime_bounds(plan: &LaunchPlan) -> Result<()> {
    let process_count = plan
        .robots
        .iter()
        .map(|robot| robot.participants.len())
        .sum::<usize>()
        // Bounded supervisor-owned helpers. The comms router is not counted:
        // it runs inside the supervisor process (organization#978).
        .saturating_add(3);
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
/// (`BundlePlan`) and source-participant records when it needs to rebuild;
/// the plan only ever names the `bin/` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "execution", rename_all = "snake_case")]
pub enum ParticipantExecution {
    /// An official platform artifact - a service or a Webots simulator,
    /// vendored or built from a workspace override - resolved from
    /// `bin/<binary_name>`.
    OfficialArtifact { binary_name: String },
    /// A user service, resolved from `bin/<binary_name>`.
    UserService { binary_name: String },
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
            | Self::UserService { binary_name }
            | Self::ComponentDriver { binary_name } => binary_name,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CheckedRobotLaunchInput<'a> {
    pub project_root: &'a Path,
    pub resolved: &'a BundlePlan,
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

    let plan = LaunchPlan { mode, robots };
    validate_runtime_bounds(&plan)?;
    Ok(plan)
}

/// Erase the per-run identities so two plans can be compared for content.
///
/// A supervised run mints a fresh `ExecutionId`, a fresh `ExecutionOrigin`, and
/// one `ProducerId` per participant, so no two plan builds ever agree on them -
/// and none of them describes *what* the plan launches.
#[must_use]
#[cfg(test)]
pub(crate) fn content_only(mut plan: LaunchPlan) -> LaunchPlan {
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
        .filter(|participant| is_robot_launch_participant(mode, participant))
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
    participants.sort_by(|left, right| {
        left.launch
            .participant_id
            .cmp(&right.launch.participant_id)
            .then_with(|| left.artifact_id.cmp(&right.artifact_id))
    });

    Ok(RobotLaunch {
        id: input.resolved.source_manifest.robot.id.clone(),
        namespace: input.resolved.source_manifest.robot.namespace.clone(),
        participants,
    })
}

fn is_robot_launch_participant(
    mode: &LaunchMode,
    participant: &graph_check::ParticipantApis,
) -> bool {
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
            // Component drivers are handled by the component-instance branch
            // above.
            SourceParticipantKind::ComponentDriver => bail!(
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
    // Compiled driver declarations carry CLI/compiler-side wiring metadata,
    // not the participant binary's typed runtime config. Official drivers in
    // the published framework train use unit config, so never forward the
    // declaration through PHOXAL_CONFIG.
    let config = match &checked.scope {
        graph_check::ParticipantScope::ComponentInstance(_) => None,
        graph_check::ParticipantScope::Graph => input
            .resolved
            .compiled
            .participants
            .iter()
            .find(|participant| {
                participant.component_instance.is_none() && participant.id == checked.participant_id
            })
            .and_then(|participant| participant.config.clone()),
    };
    ParticipantLaunch {
        participant_id: checked.participant_id.clone(),
        execution: run.execution(),
        producer: ProducerId::mint(),
        execution_origin: Some(run.origin()),
        namespace: input.resolved.source_manifest.robot.namespace.clone(),
        robot_id: input.resolved.source_manifest.robot.id.clone(),
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
        config,
        bundle_root: Some(runtime_layout_dir(input.project_root)),
        component_instance,
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

fn expected_checked_participant_ids(mode: &LaunchMode, resolved: &BundlePlan) -> BTreeSet<String> {
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
    if !matches!(mode, LaunchMode::Webots { .. }) {
        expected.extend(
            resolved
                .components
                .iter()
                .filter(|component| component.driver.is_some())
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

    use crate::project::resolver::{
        BundlePlan, ResolvedComponent, ResolvedComponentDriver, ResolvedUserRuntime,
    };

    use super::*;

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
                bundle_root: Some(PathBuf::from("/var/phoxal")),
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

        validate_runtime_bounds(&plan(37)).expect("40-process reference graph should fit");
        let error =
            validate_runtime_bounds(&plan(38)).expect_err("41 processes must remain bounded");
        let message = error.to_string();
        assert!(message.contains("has 41 supervised processes"), "{error:#}");
        assert!(message.contains("runtime supports at most 40"), "{error:#}");
    }

    #[test]
    fn launch_plan_constructor_enforces_runtime_process_bound() -> anyhow::Result<()> {
        let mut resolved = empty_bundle_plan("testbot")?;
        let names = (0..38)
            .map(|index| format!("service-{index}"))
            .collect::<Vec<_>>();
        for name in &names {
            add_user_service(&mut resolved, name);
        }
        let checked = names
            .iter()
            .map(|name| participant(name, name, graph_check::ParticipantScope::Graph))
            .collect::<Vec<_>>();
        let sources = names
            .iter()
            .map(|name| {
                SourceParticipant::user_service(name.clone(), PathBuf::from(format!("/tmp/{name}")))
            })
            .collect::<Vec<_>>();

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
        .expect_err("the authoritative constructor must reject 41 processes");
        let message = error.to_string();
        assert!(message.contains("has 41 supervised processes"), "{error:#}");
        assert!(message.contains("runtime supports at most 40"), "{error:#}");
        Ok(())
    }

    #[test]
    fn launch_plan_carries_user_service_config_through_the_launch_env() -> anyhow::Result<()> {
        let mut resolved = empty_bundle_plan("testbot")?;
        resolved.user_runtimes.push(ResolvedUserRuntime {
            name: "mission".to_string(),
            path: PathBuf::from("runtimes/mission"),
            source_hash: "hash".to_string(),
        });
        resolved.source_manifest.services.insert(
            "mission".to_string(),
            phoxal_manifest::source::robot::v0::UserService {
                config: Some(serde_json::json!({"message": "line\nquoted \"value\""})),
            },
        );
        resolved
            .compiled
            .participants
            .push(phoxal_manifest::Participant {
                id: "mission".to_string(),
                kind: phoxal_manifest::ParticipantKind::Service,
                component_instance: None,
                config: Some(serde_json::json!({"message": "line\nquoted \"value\""})),
            });
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

        let mission = plan.robots[0]
            .participants
            .iter()
            .find(|participant| participant.launch.participant_id == "mission")
            .expect("mission participant");
        let encoded = crate::runtime::launch::encode_participant_env(&mission.launch)?;
        assert_eq!(
            encoded
                .variables()
                .get(phoxal_runtime_contract::env::CONFIG)
                .map(String::as_str),
            Some(r#"{"message":"line\nquoted \"value\""}"#)
        );
        Ok(())
    }

    #[test]
    fn webots_excludes_physical_drivers_from_expected_and_resident_sets() -> anyhow::Result<()> {
        let mut resolved = empty_bundle_plan("testbot")?;
        resolved.components.push(ResolvedComponent {
            instance: "left_drive".to_string(),
            source_name: "ddsm115".to_string(),
            assets_root: PathBuf::from("/tmp/ddsm115"),
            driver: Some(ResolvedComponentDriver::Local {
                crate_dir: PathBuf::from("/tmp/ddsm115"),
            }),
        });
        let mut driver = participant(
            "left_drive",
            "ddsm115",
            graph_check::ParticipantScope::Graph,
        );
        driver.participant_kind = graph_check::ParticipantKind::Driver;
        let webots = LaunchMode::Webots {
            world: PathBuf::from("/tmp/default.wbt"),
        };
        assert!(!is_robot_launch_participant(&webots, &driver));
        assert!(is_robot_launch_participant(&LaunchMode::Run, &driver));
        assert!(!expected_checked_participant_ids(&webots, &resolved).contains("left_drive"));
        assert!(
            expected_checked_participant_ids(&LaunchMode::Run, &resolved).contains("left_drive")
        );
        Ok(())
    }

    #[test]
    fn run_launch_plan_omits_authored_drivers_not_selected_by_policy() -> anyhow::Result<()> {
        let mut resolved = empty_bundle_plan("testbot")?;
        resolved.source_manifest = phoxal_manifest::source::robot::parse_from_string(
            r#"schema: robot/v0
robot:
  id: testbot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
  components:
    left_drive:
      component: wheel
      mount_link: base
      driver:
        connection:
          type: serial
          port: /dev/ttyUSB0
          baud: 115200
    right_drive:
      component: wheel
      mount_link: base
      driver:
        connection:
          type: serial
          port: /dev/ttyUSB1
          baud: 115200
"#,
        )?;

        // A `--driver left_drive` policy materializes only that one declared
        // driver. Its unselected peer must not make checked-plan parity fail.
        resolved.components.push(ResolvedComponent {
            instance: "left_drive".to_string(),
            source_name: "wheel".to_string(),
            assets_root: PathBuf::from("/tmp/wheel"),
            driver: Some(ResolvedComponentDriver::Local {
                crate_dir: PathBuf::from("/tmp/wheel"),
            }),
        });
        resolved.components.push(ResolvedComponent {
            instance: "right_drive".to_string(),
            source_name: "wheel".to_string(),
            assets_root: PathBuf::from("/tmp/wheel"),
            driver: None,
        });
        let mut left_driver = participant(
            "left_drive",
            "wheel",
            graph_check::ParticipantScope::ComponentInstance("left_drive".to_string()),
        );
        left_driver.participant_kind = graph_check::ParticipantKind::Driver;
        let selected = build_launch_plan(
            LaunchMode::Run,
            &[CheckedRobotLaunchInput {
                project_root: Path::new("/tmp/robot"),
                resolved: &resolved,
                checked_participants: &[left_driver],
                source_participants: &[],
            }],
            RunIdentity::default(),
        )?;
        assert_eq!(
            selected.robots[0]
                .participants
                .iter()
                .map(|participant| participant.launch.participant_id.as_str())
                .collect::<Vec<_>>(),
            ["left_drive"]
        );

        // `--drivers off` uses the same resolved-model fact (`driver: None`)
        // for every authored driver. It must likewise build a plan without
        // inventing checked participants from the source declaration.
        for component in &mut resolved.components {
            component.driver = None;
        }
        let disabled = build_launch_plan(
            LaunchMode::Run,
            &[empty_checked_input(Path::new("/tmp/robot"), &resolved)],
            RunIdentity::default(),
        )?;
        assert!(disabled.robots[0].participants.is_empty());
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
        let resolved = empty_bundle_plan("testbot")?;
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
        let mut resolved = empty_bundle_plan("testbot")?;
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
            config_schema: None,
            scope,
        }
    }

    fn empty_checked_input<'a>(
        project_root: &'a Path,
        resolved: &'a BundlePlan,
    ) -> CheckedRobotLaunchInput<'a> {
        CheckedRobotLaunchInput {
            project_root,
            resolved,
            checked_participants: &[],
            source_participants: &[],
        }
    }

    fn empty_bundle_plan(id: &str) -> anyhow::Result<BundlePlan> {
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
        let robot = phoxal_manifest::source::robot::parse_from_string(&yaml)?;
        Ok(BundlePlan {
            source_manifest: robot,
            compiled: Default::default(),
            train: "0.36.0".to_string(),
            target: host_target_triple_for_tests(),
            platform_runtimes: Vec::new(),
            simulators: Vec::new(),
            user_runtimes: Vec::new(),
            undeclared_runtimes: Vec::new(),
            components: Vec::new(),
            path_overrides: Vec::new(),
        })
    }

    /// Declare one user service on `resolved`, complete enough for the launch
    /// planner: the workspace crate, the authored declaration, and the
    /// compiled participant record.
    fn add_user_service(resolved: &mut BundlePlan, name: &str) {
        resolved.user_runtimes.push(ResolvedUserRuntime {
            name: name.to_string(),
            path: PathBuf::from(format!("services/{name}")),
            source_hash: "hash".to_string(),
        });
        resolved.source_manifest.services.insert(
            name.to_string(),
            phoxal_manifest::source::robot::v0::UserService { config: None },
        );
        resolved
            .compiled
            .participants
            .push(phoxal_manifest::Participant {
                id: name.to_string(),
                kind: phoxal_manifest::ParticipantKind::Service,
                component_instance: None,
                config: None,
            });
    }

    fn host_target_triple_for_tests() -> String {
        crate::project::host_target_triple()
    }
}
