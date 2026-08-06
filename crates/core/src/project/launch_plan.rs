//! Checked launch-plan construction for run and simulation sessions.

use std::path::{Path, PathBuf};

use crate::identity::ExecutionId;
use anyhow::Result;
use phoxal_runtime_contract::ExecutionOrigin;
use phoxal_runtime_contract::ParticipantLaunch;
use serde::{Deserialize, Serialize};

use super::resolver::BundlePlan;
use crate::check::source::SourceParticipant;
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
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
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
    /// The one mandatory root brain, resolved from `bin/<binary_name>`
    /// (canonically `bin/brain`). Deliberately its own variant rather than a
    /// reused `UserService`: simulation staging derives the canonical staged
    /// name from the execution variant, and `run::participants::participant_kind`
    /// derives the observable supervisor kind from it, so collapsing the brain
    /// into a user service would erase it from both (organization#973).
    Brain { binary_name: String },
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
            Self::Brain { binary_name }
            | Self::OfficialArtifact { binary_name }
            | Self::UserService { binary_name }
            | Self::ComponentDriver { binary_name } => binary_name,
        }
    }
}

/// Erase the per-run identities so two plans can be compared for content.
#[must_use]
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn content_only(mut plan: LaunchPlan) -> LaunchPlan {
    let fixed_execution =
        ExecutionId::parse(&"0".repeat(ExecutionId::LEN)).expect("fixed execution id");
    for robot in &mut plan.robots {
        for participant in &mut robot.participants {
            participant.launch.execution = fixed_execution;
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
/// previous run cannot be observed as current. Producer identity is not
/// planned: each participant's producer is the Zenoh session it opens.
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
