//! Participants responsibilities for run.
//!
//! Execution resolves every participant binary from the staged runtime layout's
//! flat `bin/` store and inspects it off-disk (host-architecture check plus
//! embedded metadata) before it is ever spawned. There is no cargo-target /
//! vendored-store lookup and no graceful "pending" board note at launch: a
//! participant whose binary the staging step could not produce, or whose staged
//! binary is built for a foreign architecture, is a HARD startup failure naming
//! the required identity (#936). Staging (`crate::stager`) is the only code that
//! knows about `cargo install` materialization; this module only reads what
//! staging produced.

use super::{DriverPolicy, build_source_binary, device_missing_note, missing_device_path};
use crate::supervisor::BoardBackend;
use crate::supervisor::ParticipantSpec;
use crate::supervisor::ParticipantState;
use crate::supervisor::ParticipantStatus;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use phoxal_cli_core::check::participant_metadata::inspect_selected_binary;
use phoxal_cli_core::check::source::SourceParticipant;
use phoxal_cli_core::check::source::SourceParticipantKind;
use phoxal_cli_core::project::catalog::ArtifactKind;
use phoxal_cli_core::project::launch_plan::LaunchPlan;
use phoxal_cli_core::project::launch_plan::ParticipantExecution;
use phoxal_cli_core::project::launch_plan::ParticipantLaunchRecord;
use phoxal_cli_core::project::layout::DriverSelection;
use phoxal_cli_core::project::layout::RuntimeLayout;
use phoxal_cli_core::project::resolver::ResolvedPlatformRuntime;
use phoxal_cli_core::project::resolver::ResolvedRobot;
use phoxal_cli_core::project::resolver::official_binary_name;
use phoxal_cli_core::session::ParticipantKind;
use phoxal_cli_core::session::launch_env::{encode_participant_env, encode_tool_env};
use phoxal_cli_core::session::stores::telemetry::RobotScope;
use phoxal_cli_core::session::{ProcessKey, RobotKey};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DriverDecision {
    Launch,
    Degraded(String),
}

/// The staging-side record of source crate directories, keyed by launch
/// participant id, that accompanies a plan built from a source project. The
/// source-free plan (#936) never names a crate directory, so cargo rebuild and
/// process cwd for the user services and workspace-built component drivers a
/// source project owns are recovered from this map instead. It is empty for a
/// plan built from an extracted bundle, which has no source at all.
pub(crate) fn source_dirs_by_participant(
    source_participants: &[phoxal_cli_core::check::source::SourceParticipant],
) -> BTreeMap<String, PathBuf> {
    source_participants
        .iter()
        .map(|participant| (participant.name.clone(), participant.crate_dir.clone()))
        .collect()
}

/// Populate the staged layout's flat `bin/` store with every binary the loader
/// requires, so [`RuntimeLayout::construct_plan`] can inspect the complete set
/// off-disk before any process launches (#936). This is the staging-side
/// counterpart of the execution path: it is the only code that resolves source,
/// keyed by the resolved graph (`ResolvedRobot`) and its source-participant
/// records, never by a plan.
///
/// It links: every user service and workspace/path-overridden component driver,
/// built from its crate; every suite-provided component driver, from the
/// vendored store; and every official service, tool, and the infrastructure
/// router, from the vendored store or a source override. After it runs, `bin/`
/// is the complete lookup store an extracted bundle would carry - the loader
/// resolves every required runtime from it with no source present.
///
/// `drivers` gates the component-driver work exactly as the plan constructor
/// does (#936): a driver the run excludes (drivers off, or an instance outside a
/// `--driver` subset) is not built or linked here, so `--drivers off` never
/// force-builds a driver crate the run will not launch.
#[allow(clippy::too_many_arguments)]
pub(crate) fn stage_complete_bin_store(
    staged_root: &Path,
    resolved: &ResolvedRobot,
    source_participants: &[SourceParticipant],
    drivers: &DriverSelection,
    offline: bool,
    build: &crate::run::StagingBuild,
    ui: &crate::Ui,
) -> Result<()> {
    let mut staged_names = BTreeSet::new();
    // Source-built user services and workspace/path-overridden component
    // drivers. Official-service/tool/simulator source overrides are
    // materialized by `materialize_official_store` below (it owns the
    // cargo-install-vs-override resolution for the official set), so they are
    // skipped here.
    for participant in source_participants {
        let binary_name = match participant.kind {
            SourceParticipantKind::UserService | SourceParticipantKind::UserTool => {
                participant.name.clone()
            }
            SourceParticipantKind::ComponentDriver => {
                // An excluded driver instance is never built: its binary is not
                // required, so force-building the crate would be wasted work the
                // run will not launch (and, on a foreign host, may not compile).
                if !drivers.includes_instance(&participant.name) {
                    continue;
                }
                official_binary_name(
                    ArtifactKind::ComponentDriver,
                    &participant.expected_artifact_id,
                )
            }
            SourceParticipantKind::OfficialService
            | SourceParticipantKind::Tool
            | SourceParticipantKind::Simulator => continue,
        };
        if !staged_names.insert(binary_name.clone()) {
            continue;
        }
        let built =
            build.build_user_binary(&participant.crate_dir, &participant.name, ui, offline)?;
        crate::stager::stage_named_binary(staged_root, &binary_name, &built)?;
    }
    // Registry-provided component drivers: one binary per driven component
    // id, materialized straight into `bin/` via `cargo install`. A
    // workspace/path-overridden driver for the same component id was already
    // staged above (its binary name is in `staged_names`), so it is not
    // re-materialized here.
    for component in &resolved.components {
        if !component.has_driver {
            continue;
        }
        // Skip a driver whose instance the policy excludes; a sibling instance
        // that is selected still materializes the shared binary through its
        // own row.
        if !drivers.includes_instance(&component.instance) {
            continue;
        }
        let binary_name =
            official_binary_name(ArtifactKind::ComponentDriver, &component.source_name);
        if !staged_names.insert(binary_name.clone()) {
            continue;
        }
        let Some(runtime) = component
            .driver
            .as_ref()
            .and_then(|driver| driver.registry_runtime.as_ref())
        else {
            continue;
        };
        crate::stager::materialize_component_driver(
            staged_root,
            runtime,
            offline,
            build.officials_source(),
        )?;
    }
    // Every official service, tool, and the infrastructure router.
    crate::stager::materialize_official_store(
        staged_root,
        resolved,
        offline,
        build.officials_source(),
        |crate_dir, name| build.build_user_binary(crate_dir, name, ui, offline),
    )
    .context("failed to complete the staged bin store with the full official runtime set")?;
    Ok(())
}

/// Build the participant specs for a launch plan whose binaries already live in
/// the staged layout's flat `bin/` store, resolving every executable directly
/// from `bin/` with no source, Cargo, or resolved graph (#936). This is the ONE
/// execution-side spec builder for a staged runtime layout - a source project's
/// `.phoxal/bundle/` (after [`stage_complete_bin_store`] populated and
/// [`crate::loader::validate_layout_plan`] validated it) or an extracted
/// `build.phoxal`. Because it reads the already-validated `bin/` and never
/// rebuilds, the executed bytes are exactly the validated bytes - there is no
/// second resolve/rebuild pass that could diverge (#936, finding 3).
///
/// `cwd_for` supplies the source-only working directory the source-free plan no
/// longer carries: the source run passes [`source_cwd`] so a participant built
/// from local source runs from its crate directory, and an extracted-bundle run
/// passes a closure returning `None` (a bundle has no source). Board
/// classification, the component-driver missing-device check (read straight
/// from the compiled `robot.yaml`), and readiness/env/policy encoding are
/// shared by both. Driver policy needs no gate here: the plan constructor
/// already excluded non-selected drivers, so every driver in the plan launches.
pub(crate) fn build_layout_specs(
    plan: &LaunchPlan,
    layout: &RuntimeLayout,
    board: &BoardBackend,
    specs: &mut Vec<ParticipantSpec>,
    cwd_for: &dyn Fn(&ParticipantLaunchRecord) -> Option<PathBuf>,
) -> Result<()> {
    let bin_dir = layout.bin_dir();
    for robot in &plan.robots {
        let robot_key = RobotKey::new(&robot.namespace, &robot.id);
        let scope = RobotScope {
            namespace: robot.namespace.clone(),
            robot_id: robot.id.clone(),
        };
        for participant in &robot.participants {
            let id = participant.launch.participant_id.clone();
            let key = ProcessKey::robot(robot_key.clone(), &id);
            let (kind, base_local) = participant_kind(&participant.execution);
            // Board-only staging provenance (#936, finding 10): a source-
            // overridden official or tool runs from the project workspace, so it
            // has a source cwd here even though its source-free `execution` is
            // byte-identical to a vendored one. Reflect that on the board (local
            // = runs from the robot's own code) WITHOUT touching the plan, which
            // stays identical across origins. An extracted layout supplies no
            // cwd, so its officials correctly stay origin-unknown (vendored).
            let cwd = cwd_for(participant);
            let source_overridden_official = cwd.is_some()
                && matches!(
                    participant.execution,
                    ParticipantExecution::OfficialArtifact { .. }
                        | ParticipantExecution::OfficialTool { .. }
                );
            let local = base_local || source_overridden_official;
            board.upsert_process(
                key.clone(),
                ParticipantStatus::new(&id, kind, ParticipantState::Starting)
                    .with_local(local)
                    .with_scope(scope.clone()),
                participant.startup_requirement,
            );
            if source_overridden_official {
                board.set_note(
                    key.clone(),
                    "source-override: built from the project workspace",
                );
            }
            if matches!(
                participant.execution,
                ParticipantExecution::ComponentDriver { .. }
            ) && let Some(note) = layout_device_missing_note(layout, &id)
            {
                board.set_state(&key, ParticipantState::Failed, Some(note));
                continue;
            }
            let executable = bin_dir.join(participant.execution.binary_name());
            inspect_selected_binary(&executable).with_context(|| {
                format!(
                    "failed to inspect staged runtime `{}` at {}",
                    id,
                    executable.display()
                )
            })?;
            specs.push(participant_spec(
                participant,
                &robot_key,
                kind,
                executable,
                cwd,
            )?);
        }
    }
    Ok(())
}

/// The missing-device board note for a driver participant in a compiled
/// `robot.yaml`, computed directly from the layout's robot model (no resolved
/// graph). Mirrors [`device_missing_note`], which reads the same connection
/// config off a `ResolvedRobot`.
fn layout_device_missing_note(layout: &RuntimeLayout, participant_id: &str) -> Option<String> {
    let component = layout.robot().robot.components.get(participant_id)?;
    let driver = component.driver.as_ref()?;
    let missing = missing_device_path(&driver.connection)?;
    Some(format!(
        "DeviceMissing: {missing} for driver {participant_id}"
    ))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_robot_participants(
    plan: &LaunchPlan,
    resolved: &ResolvedRobot,
    source_dirs: &BTreeMap<String, PathBuf>,
    staged_root: &Path,
    driver_policy: &DriverPolicy,
    board: &BoardBackend,
    specs: &mut Vec<ParticipantSpec>,
    offline: bool,
    ui: &crate::Ui,
) -> Result<()> {
    let official_by_name = official_runtimes_by_name(resolved);
    for robot in &plan.robots {
        let robot_key = RobotKey::new(&robot.namespace, &robot.id);
        let scope = RobotScope {
            namespace: robot.namespace.clone(),
            robot_id: robot.id.clone(),
        };
        for participant in &robot.participants {
            let id = participant.launch.participant_id.clone();
            let key = ProcessKey::robot(robot_key.clone(), &id);
            let (kind, local) = participant_kind(&participant.execution);
            board.upsert_process(
                key.clone(),
                ParticipantStatus::new(&id, kind, ParticipantState::Starting)
                    .with_local(local)
                    .with_scope(scope.clone()),
                participant.startup_requirement,
            );
            // Component-driver launch gating (bench subset, missing
            // device) is a board/policy decision that precedes any binary
            // resolution: a gated-out driver never needs its binary staged.
            if matches!(
                participant.execution,
                ParticipantExecution::ComponentDriver { .. }
            ) {
                match driver_policy.decision(&id) {
                    DriverDecision::Degraded(note) => {
                        board.set_state(&key, ParticipantState::Degraded, Some(note));
                        continue;
                    }
                    DriverDecision::Launch => {}
                }
                if let Some(note) = device_missing_note(resolved, &id) {
                    board.set_state(&key, ParticipantState::Failed, Some(note));
                    continue;
                }
            }
            let source = resolve_participant_source(
                staged_root,
                participant,
                resolved,
                &official_by_name,
                source_dirs,
                offline,
                ui,
            )?;
            let executable = stage_and_inspect(staged_root, participant, &source)?;
            let cwd = source_cwd(participant, resolved, source_dirs);
            specs.push(participant_spec(
                participant,
                &robot_key,
                kind,
                executable,
                cwd,
            )?);
        }
    }
    Ok(())
}

/// The board `ParticipantKind` plus whether the participant runs from local
/// (user/robot-owned) code, for a participant's source-free `execution` (#936).
/// The role alone decides both here: officials and tools are framework binaries
/// (`local = false`), user services and component drivers are the robot's own
/// code (`local = true`). An official the robot overrides in its Cargo workspace
/// still resolves to the one official `bin/` entry, so the PLAN cannot and does
/// not distinguish "overridden" from "vendored" - it stays byte-identical across
/// origins. The board, however, refines this `local` bit from the source-side
/// staging origin (a source cwd) so a source-overridden official is shown as
/// local with a provenance note; an extracted layout has no such origin and its
/// officials stay vendored (#936, finding 10). See [`build_layout_specs`].
pub(crate) fn participant_kind(execution: &ParticipantExecution) -> (ParticipantKind, bool) {
    match execution {
        ParticipantExecution::OfficialArtifact { .. } => (ParticipantKind::Service, false),
        ParticipantExecution::OfficialTool { .. } => (ParticipantKind::Tool, false),
        ParticipantExecution::UserService { .. } => (ParticipantKind::Service, true),
        ParticipantExecution::UserTool { .. } => (ParticipantKind::Tool, true),
        ParticipantExecution::ComponentDriver { .. } => (ParticipantKind::Driver, true),
    }
}

/// Every official platform runtime the loader may need to resolve, keyed by
/// its launch identity: the services and simulators in `platform_runtimes`
/// plus the registry-sourced component drivers carried on
/// `components[].driver.registry_runtime` (a registry driver projects onto
/// the same `ResolvedPlatformRuntime` shape and is keyed by its component
/// id). Source-sourced drivers are not here - they build from their crate
/// through the source-execution path.
fn official_runtimes_by_name(resolved: &ResolvedRobot) -> BTreeMap<&str, &ResolvedPlatformRuntime> {
    resolved
        .platform_runtimes
        .iter()
        .chain(
            resolved
                .components
                .iter()
                .filter_map(|component| component.driver.as_ref())
                .filter_map(|driver| driver.registry_runtime.as_ref()),
        )
        .map(|runtime| (runtime.name.as_str(), runtime))
        .collect()
}

/// Resolve the binary one launched participant runs from, so it can be staged
/// into the layout's `bin/`. The source-free plan (#936) names only the
/// participant's role and `bin/` binary, so where its bytes come from is
/// recovered here from the resolved graph and the staging-side `source_dirs`
/// record: a user service and a workspace-built component driver build
/// through `cargo` from their crate directory; an official artifact, tool, or
/// registry-provided driver materializes via `cargo install`, straight into
/// `staged_root/bin/`. Every path hard-fails - there is no graceful
/// `None`/pending note - naming the required identity.
fn resolve_participant_source(
    staged_root: &Path,
    participant: &ParticipantLaunchRecord,
    resolved: &ResolvedRobot,
    official_by_name: &BTreeMap<&str, &ResolvedPlatformRuntime>,
    source_dirs: &BTreeMap<String, PathBuf>,
    offline: bool,
    ui: &crate::Ui,
) -> Result<PathBuf> {
    let id = &participant.launch.participant_id;
    match &participant.execution {
        ParticipantExecution::UserService { .. } | ParticipantExecution::UserTool { .. } => {
            let crate_dir = source_dirs.get(id).ok_or_else(|| {
                anyhow!("staged plan is missing the source crate directory for user runtime {id}")
            })?;
            build_source_binary(crate_dir, id, ui, None, offline)
        }
        ParticipantExecution::ComponentDriver { .. } => {
            // A workspace-built driver has a crate directory in the staging
            // record; a registry-provided one does not and materializes via
            // `cargo install`, keyed by its component id.
            if let Some(crate_dir) = source_dirs.get(id) {
                return build_source_binary(crate_dir, id, ui, None, offline);
            }
            let runtime = official_by_name
                .get(participant.artifact_id.as_str())
                .ok_or_else(|| {
                    anyhow!(
                        "resolved graph is missing component driver {}",
                        participant.artifact_id
                    )
                })?;
            // This resolution path (single-participant execution, used only
            // by simulation) never runs through the container builder, so
            // there is no pre-materialized officials directory to check.
            crate::stager::materialize_component_driver(staged_root, runtime, offline, None)?;
            Ok(staged_root.join("bin").join(
                phoxal_cli_core::project::resolver::official_binary_name(
                    runtime.kind,
                    &runtime.name,
                ),
            ))
        }
        ParticipantExecution::OfficialTool { .. } => {
            let tool = resolved
                .tools
                .iter()
                .find(|tool| tool.name == participant.artifact_id)
                .ok_or_else(|| {
                    anyhow!("resolved graph is missing tool {}", participant.artifact_id)
                })?;
            if let Some(crate_dir) = &tool.path_override {
                return build_source_binary(
                    crate_dir,
                    phoxal_cli_core::project::resolver::tool_participant_id(&tool.name),
                    ui,
                    None,
                    offline,
                );
            }
            crate::materialize::cargo_install(
                staged_root,
                &crate::materialize::MaterializeSpec::new(tool.package.clone(), tool.train.clone())
                    .with_target(Some(tool.target.clone())),
                offline,
            )
        }
        ParticipantExecution::OfficialArtifact { .. } => {
            let runtime = official_by_name
                .get(participant.artifact_id.as_str())
                .ok_or_else(|| {
                    anyhow!(
                        "resolved graph is missing official artifact {}",
                        participant.artifact_id
                    )
                })?;
            if let Some(crate_dir) = &runtime.path_override {
                return build_source_binary(crate_dir, &runtime.name, ui, None, offline);
            }
            crate::materialize::cargo_install(
                staged_root,
                &crate::materialize::MaterializeSpec::new(
                    runtime.package.clone(),
                    runtime.train.clone(),
                )
                .with_target(runtime.target.clone()),
                offline,
            )
        }
    }
}

/// Stage one participant's resolved source binary into the layout's flat `bin/`
/// and inspect the staged entry off-disk: verify it is executable on the host
/// architecture and read its embedded metadata, never running it. A missing or
/// foreign-architecture binary is a hard startup failure naming the identity.
fn stage_and_inspect(
    staged_root: &Path,
    participant: &ParticipantLaunchRecord,
    source: &Path,
) -> Result<PathBuf> {
    let staged = crate::stager::stage_participant_binary(staged_root, participant, source)?;
    inspect_selected_binary(&staged).with_context(|| {
        format!(
            "failed to inspect staged runtime `{}` at {}",
            participant.launch.participant_id,
            staged.display()
        )
    })?;
    Ok(staged)
}

/// The working directory a launched participant runs in: a participant built
/// from source (a user service, a workspace-built driver, or an official the
/// robot overrides in its Cargo workspace) runs from its crate directory so
/// relative asset paths resolve; a vendored official runs from the layout with
/// no crate context. The crate directory comes from the staging-side record
/// (user services / workspace drivers) or the resolved graph's path override
/// (overridden officials and tools).
pub(crate) fn source_cwd(
    participant: &ParticipantLaunchRecord,
    resolved: &ResolvedRobot,
    source_dirs: &BTreeMap<String, PathBuf>,
) -> Option<PathBuf> {
    let id = &participant.launch.participant_id;
    match &participant.execution {
        ParticipantExecution::UserService { .. }
        | ParticipantExecution::UserTool { .. }
        | ParticipantExecution::ComponentDriver { .. } => source_dirs.get(id).cloned(),
        ParticipantExecution::OfficialArtifact { .. } => resolved
            .platform_runtimes
            .iter()
            .find(|runtime| runtime.name == participant.artifact_id)
            .and_then(|runtime| runtime.path_override.clone()),
        ParticipantExecution::OfficialTool { .. } => resolved
            .tools
            .iter()
            .find(|tool| tool.name == participant.artifact_id)
            .and_then(|tool| tool.path_override.clone()),
    }
}

/// Whether a participant launches with tool env (privileged) rather than the
/// bus-participant env: an official tool, vendored or overridden.
fn is_tool_execution(execution: &ParticipantExecution) -> bool {
    matches!(execution, ParticipantExecution::OfficialTool { .. })
}

/// Assemble the `ParticipantSpec` the supervisor spawns: the staged executable
/// plus the launch env/readiness/policies carried by the plan record.
fn participant_spec(
    participant: &ParticipantLaunchRecord,
    robot_key: &RobotKey,
    kind: ParticipantKind,
    executable: PathBuf,
    cwd: Option<PathBuf>,
) -> Result<ParticipantSpec> {
    let id = participant.launch.participant_id.clone();
    let env = if is_tool_execution(&participant.execution) {
        encode_tool_env(&participant.launch)?
    } else {
        encode_participant_env(&participant.launch)?
    };
    Ok(ParticipantSpec {
        key: ProcessKey::robot(robot_key.clone(), &id),
        id,
        kind,
        executable,
        args: Vec::new(),
        cwd,
        env: env.spawn_env(),
        shutdown_grace: Duration::from_millis(participant.launch.shutdown_grace_ms),
        process_group: true,
        note: None,
        bus_participant: true,
        readiness: ParticipantSpec::exact_liveliness_template(
            robot_key.clone(),
            &participant.launch.participant_id,
        ),
        startup_requirement: participant.startup_requirement,
        runtime_failure: participant.runtime_failure,
        restart_policy: Default::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal::participant::launch::{
        BusProfile, ClockMode, DEFAULT_SHUTDOWN_GRACE_MS, ParticipantLaunch,
    };
    use phoxal_cli_core::check::participant_metadata::host_architecture;
    use phoxal_cli_core::project::launch_plan::RunIdentity;
    use phoxal_cli_core::session::RuntimeFailurePolicy;
    use phoxal_cli_core::session::StartupRequirement;

    /// Synthesize a host-format object of a given architecture carrying the
    /// phoxal metadata section, so inspection is exercised against real object
    /// shapes without building a binary (mirrors the loader's own synthesis).
    /// `id` is the binary's own declared participant id (organization#957: a
    /// staged binary's id must match the required runtime's identity, so a
    /// caller staging more than one required runtime must give each its own
    /// matching id rather than reusing a fixed payload).
    fn synthesize_binary_with_id(arch: object::Architecture, id: &str) -> Vec<u8> {
        use object::write::Object;
        let format = phoxal_cli_core::check::participant_metadata::host_binary_format();
        let (segment, name): (&[u8], &[u8]) = match format {
            object::BinaryFormat::MachO => (b"__DATA", b"__phoxal_meta"),
            _ => (b"", b".phoxal_meta"),
        };
        let mut obj = Object::new(format, arch, object::Endianness::Little);
        let section = obj.add_section(
            segment.to_vec(),
            name.to_vec(),
            object::SectionKind::ReadOnlyData,
        );
        let payload = format!(r#"{{"id":"{id}","config_schema":{{"type":"null"}}}}"#);
        obj.append_section_data(section, payload.as_bytes(), 1);
        obj.write().expect("synthesize object file")
    }

    /// [`synthesize_binary_with_id`] for a fixture whose only participant is
    /// `mission`.
    fn synthesize_binary(arch: object::Architecture) -> Vec<u8> {
        synthesize_binary_with_id(arch, "mission")
    }

    fn user_service_record(id: &str) -> ParticipantLaunchRecord {
        ParticipantLaunchRecord {
            artifact_id: id.to_string(),
            execution: ParticipantExecution::UserService {
                binary_name: id.to_string(),
            },
            launch: ParticipantLaunch {
                participant_id: id.to_string(),
                execution: phoxal::bus::ExecutionId::mint(),
                producer: phoxal::bus::ProducerId::mint(),
                execution_origin: None,
                namespace: "dev".to_string(),
                robot_id: "testbot".to_string(),
                bus: BusProfile {
                    connect_endpoints: Vec::new(),
                },
                clock: ClockMode::Real,
                config: None,
                robot_root: None,
                component_instance: None,
                shutdown_grace_ms: DEFAULT_SHUTDOWN_GRACE_MS,
            },
            startup_requirement: StartupRequirement::Required,
            runtime_failure: RuntimeFailurePolicy::StopProject,
        }
    }

    #[test]
    fn a_staged_participant_binary_resolves_from_bin_under_the_layout() -> Result<()> {
        let staged = tempfile::tempdir()?;
        let source = tempfile::tempdir()?;
        let src_bin = source.path().join("mission");
        std::fs::write(&src_bin, synthesize_binary(host_architecture()))?;

        let record = user_service_record("mission");
        let executable = stage_and_inspect(staged.path(), &record, &src_bin)?;

        // The launched executable is the flat `bin/` entry under the layout,
        // not the source binary the staging step resolved.
        assert_eq!(executable, staged.path().join("bin/mission"));
        assert!(executable.starts_with(staged.path().join("bin")));
        assert!(executable.is_file());
        Ok(())
    }

    #[test]
    fn a_foreign_arch_staged_binary_fails_precisely_naming_the_identity() -> Result<()> {
        let staged = tempfile::tempdir()?;
        let source = tempfile::tempdir()?;
        let src_bin = source.path().join("mission");
        let foreign = if host_architecture() == object::Architecture::X86_64 {
            object::Architecture::Aarch64
        } else {
            object::Architecture::X86_64
        };
        std::fs::write(&src_bin, synthesize_binary(foreign))?;

        let record = user_service_record("mission");
        let error = format!(
            "{:#}",
            stage_and_inspect(staged.path(), &record, &src_bin)
                .expect_err("a foreign-arch staged binary must be rejected")
        );
        assert!(error.contains("mission"), "{error}");
        assert!(error.contains("built for"), "{error}");
        Ok(())
    }

    const LAYOUT_ROBOT_YAML: &str = r#"schema: robot/v0
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
  components: {}
services:
  mission:
    config:
      speed: 1
"#;

    /// The layout execution path reads only the staged `bin/` store: every
    /// participant's executable is the flat `bin/<binary_name>` entry, resolved
    /// with no source, Cargo, resolved graph, or materialization state (#936,
    /// organization#951 WS4). A staged layout that carries a stray `.phoxal/`
    /// subdirectory proves the layout path never consults anything there.
    #[test]
    fn layout_specs_resolve_every_executable_from_bin_with_no_other_state() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path();
        std::fs::write(root.join("robot.yaml"), LAYOUT_ROBOT_YAML)?;
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin)?;
        // Stray `.phoxal/` state the layout path must never touch: if it were
        // consulted the run would depend on it, defeating the bundle guarantee.
        std::fs::create_dir_all(root.join(".phoxal/resolve"))?;

        let layout = RuntimeLayout::open(root)?;
        for required in layout.required_runtimes(&DriverSelection::All) {
            if required.kind
                == phoxal_cli_core::project::layout::RequiredRuntimeKind::Infrastructure
            {
                continue;
            }
            std::fs::write(
                bin.join(&required.binary_name),
                synthesize_binary_with_id(host_architecture(), &required.identity),
            )?;
        }

        let plan = RuntimeLayout::construct_plan(
            root,
            &phoxal_cli_core::project::layout::PlanOptions::default(),
            RunIdentity::default(),
        )?
        .plan;
        let board = BoardBackend::new();
        let mut specs = Vec::new();
        build_layout_specs(&plan, &layout, &board, &mut specs, &|_| None)?;

        assert!(
            !specs.is_empty(),
            "the layout must produce launchable specs"
        );
        for spec in &specs {
            assert!(
                spec.executable.starts_with(&bin),
                "every executable must resolve from bin/: {}",
                spec.executable.display()
            );
            assert!(spec.executable.is_file(), "{}", spec.executable.display());
            assert!(
                spec.cwd.is_none(),
                "a staged layout participant has no source crate cwd"
            );
        }
        // The user service is present, proving the compiled robot.yaml's own
        // services join the plan alongside the officials.
        assert!(
            specs.iter().any(|spec| spec.id == "mission"),
            "the user service `mission` must be launchable from the layout"
        );
        Ok(())
    }

    /// Board-only staging provenance (#936, finding 10): when the source path
    /// supplies a cwd for an official (a source override), the board marks that
    /// official `local` and carries a source-override note, while a vendored
    /// official (no cwd) stays `local = false`. The launch plan is untouched
    /// either way - only the board metadata differs.
    #[test]
    fn source_overridden_officials_are_marked_local_on_the_board() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path();
        std::fs::write(root.join("robot.yaml"), LAYOUT_ROBOT_YAML)?;
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin)?;

        let layout = RuntimeLayout::open(root)?;
        for required in layout.required_runtimes(&DriverSelection::All) {
            if required.kind
                == phoxal_cli_core::project::layout::RequiredRuntimeKind::Infrastructure
            {
                continue;
            }
            std::fs::write(
                bin.join(&required.binary_name),
                synthesize_binary_with_id(host_architecture(), &required.identity),
            )?;
        }

        let plan = RuntimeLayout::construct_plan(
            root,
            &phoxal_cli_core::project::layout::PlanOptions::default(),
            RunIdentity::default(),
        )?
        .plan;
        // Pick one official artifact from the plan and pretend the project
        // overrides it in its workspace (a source cwd).
        let overridden = plan
            .robots
            .iter()
            .flat_map(|robot| &robot.participants)
            .find(|participant| {
                matches!(
                    participant.execution,
                    ParticipantExecution::OfficialArtifact { .. }
                )
            })
            .map(|participant| participant.launch.participant_id.clone())
            .expect("the plan has at least one official artifact");
        let overridden_cwd = overridden.clone();
        let cwd_for = move |participant: &ParticipantLaunchRecord| {
            (participant.launch.participant_id == overridden_cwd)
                .then(|| root.join("services").join(&overridden_cwd))
        };

        let board = BoardBackend::new();
        let mut specs = Vec::new();
        build_layout_specs(&plan, &layout, &board, &mut specs, &cwd_for)?;

        let snapshot = board.snapshot();
        let key = ProcessKey::robot(RobotKey::new("dev", "testbot"), &overridden).to_string();
        let status = snapshot
            .participants
            .get(&key)
            .expect("the overridden official is on the board");
        assert!(
            status.local,
            "a source-overridden official must be marked local on the board"
        );
        assert!(
            status
                .note
                .as_deref()
                .is_some_and(|note| note.contains("source-override")),
            "a source-overridden official must carry a provenance note: {:?}",
            status.note
        );
        Ok(())
    }
}
