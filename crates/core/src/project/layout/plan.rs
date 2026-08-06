//! The launch-plan constructor.
//!
//! Execution derives the complete launch graph from a finalized bundle alone:
//! its `robot.yaml`, the CLI-internal catalog through the one
//! [`derive_runtime_requirements`](super::super::requirements::derive_runtime_requirements)
//! owner, and the embedded metadata of the binaries under `bin/`. Nothing here
//! reads source or Cargo - finalization already produced the bundle, and this
//! module is the one place that turns it into the launch plan the supervisor
//! consumes, whether the bundle was just published from a source project or
//! extracted from a `build.phoxal` archive.
//!
//! The jsonschema validator lives in the bin crate, so this module does not run
//! it. It returns the *input* the bin needs - a config-schema/config pairing per
//! declared user participant - and the bin-side "validate through the loader"
//! entry runs its own validator over it.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use phoxal_runtime_contract::{
    BusProfile, ClockMode, DEFAULT_SHUTDOWN_GRACE_MS, ParticipantLaunch,
};

use super::super::launch_plan::{
    DEFAULT_ROUTER_CONNECT, LaunchMode, LaunchPlan, ParticipantExecution, ParticipantLaunchRecord,
    RobotLaunch, RunIdentity, validate_runtime_bounds,
};
use super::super::requirements::{RequiredParticipant, RequiredParticipantKind};
use super::{LayoutInspection, RuntimeLayout, SelectedBinary};
use crate::runtime::{RuntimeFailurePolicy, StartupRequirement};

/// The complete launch graph a finalized bundle derives, plus the validation
/// input the bin-side "validate through the loader" entry runs its own
/// validator over. The plan is what the supervisor launches from; the config
/// pairings never enter the plan.
#[derive(Debug, Clone)]
pub struct ConstructedPlan {
    pub plan: LaunchPlan,
    /// One schema pairing per authored participant config, so the project crate
    /// can validate the config against the schema its binary embeds.
    pub user_runtime_configs: Vec<UserRuntimeConfig>,
}

/// An authored participant config paired with the schema embedded in its
/// selected binary. This covers declared user services; the root brain has no
/// config side channel and officials take no authored configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRuntimeConfig {
    pub runtime_id: String,
    pub config_schema: serde_json::Value,
    pub config: Option<serde_json::Value>,
    /// Declaring authored map, for diagnostics.
    pub family: &'static str,
}

impl RuntimeLayout {
    /// Open the finalized bundle at `root`, inspect every selected binary
    /// (target check, compatibility check, embedded metadata - no execution),
    /// and construct its launch plan.
    pub fn construct_plan(root: &std::path::Path, run: RunIdentity) -> Result<ConstructedPlan> {
        Self::construct_plan_with_inspection(root, LayoutInspection::Host, run)
    }

    /// [`Self::construct_plan`], inspecting each selected binary against the
    /// architecture `inspection` selects: the host for an in-place run/start, or
    /// a declared `--target` for a cross bundle that will never execute here.
    pub fn construct_plan_with_inspection(
        root: &std::path::Path,
        inspection: LayoutInspection,
        run: RunIdentity,
    ) -> Result<ConstructedPlan> {
        let layout = Self::open(root)?;
        let selected = layout.inspect_selected(inspection)?;
        layout.construct_plan_from_selected(&selected, run)
    }

    /// Construct the launch plan from an already-inspected selected-binary set,
    /// keyed by canonical `bin/` name.
    fn construct_plan_from_selected(
        &self,
        selected: &BTreeMap<String, SelectedBinary>,
        run: RunIdentity,
    ) -> Result<ConstructedPlan> {
        let robot_id = self.robot().robot_id().to_string();
        let namespace = self.robot().namespace().to_string();
        let bundle_root = self.root().to_path_buf();
        let clock = match self.manifest().clock {
            phoxal_manifest::source::robot::v0::Clock::Real => ClockMode::Real,
            phoxal_manifest::source::robot::v0::Clock::Simulated => ClockMode::Simulation,
        };

        let mut participants = Vec::new();
        let mut user_runtime_configs = Vec::new();
        for required in &self.requirements().participants {
            let execution = match required.kind {
                RequiredParticipantKind::Brain => ParticipantExecution::Brain {
                    binary_name: required.binary_name.clone(),
                },
                RequiredParticipantKind::OfficialService | RequiredParticipantKind::WorldClock => {
                    ParticipantExecution::OfficialArtifact {
                        binary_name: required.binary_name.clone(),
                    }
                }
                RequiredParticipantKind::UserService => ParticipantExecution::UserService {
                    binary_name: required.binary_name.clone(),
                },
                RequiredParticipantKind::ComponentDriver => ParticipantExecution::ComponentDriver {
                    binary_name: required.binary_name.clone(),
                },
            };
            if required.kind == RequiredParticipantKind::UserService {
                user_runtime_configs.push(UserRuntimeConfig {
                    runtime_id: required.participant_id.clone(),
                    config_schema: selected
                        .get(&required.binary_name)
                        .with_context(|| {
                            format!("no inspected binary for `{}`", required.binary_name)
                        })?
                        .meta
                        .config_schema
                        .clone(),
                    config: required.config.clone(),
                    family: "services",
                });
            }
            participants.push(launch_record(
                required,
                execution,
                clock,
                &robot_id,
                &bundle_root,
                run,
            ));
        }

        participants.sort_by(|left, right| {
            left.launch
                .participant_id
                .cmp(&right.launch.participant_id)
                .then_with(|| left.artifact_id.cmp(&right.artifact_id))
        });

        let plan = LaunchPlan {
            mode: LaunchMode::Run,
            robots: vec![RobotLaunch {
                id: robot_id,
                namespace,
                participants,
            }],
        };
        validate_runtime_bounds(&plan)?;
        Ok(ConstructedPlan {
            plan,
            user_runtime_configs,
        })
    }
}

/// Every required participant gets the same launch policy: startup required,
/// terminal failure stops the project. The catalog owns that policy and no
/// authored document may weaken it.
fn launch_record(
    required: &RequiredParticipant,
    execution: ParticipantExecution,
    clock: ClockMode,
    robot_id: &str,
    bundle_root: &std::path::Path,
    run: RunIdentity,
) -> ParticipantLaunchRecord {
    ParticipantLaunchRecord {
        artifact_id: required.artifact_id.clone(),
        execution,
        launch: ParticipantLaunch {
            participant_id: required.participant_id.clone(),
            execution: run.execution(),
            execution_origin: Some(run.origin()),
            robot_id: robot_id.to_string(),
            bus: BusProfile {
                connect_endpoints: vec![DEFAULT_ROUTER_CONNECT.to_string()],
            },
            clock,
            config: required.config.clone(),
            bundle_root: Some(bundle_root.to_path_buf()),
            component_instance: required.component_instance.clone(),
            shutdown_grace_ms: DEFAULT_SHUTDOWN_GRACE_MS,
        },
        startup_requirement: StartupRequirement::Required,
        runtime_failure: RuntimeFailurePolicy::StopProject,
    }
}
