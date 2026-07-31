//! The layout-driven launch-plan constructor (#936).
//!
//! Execution derives the complete launch graph from a staged runtime layout
//! alone: canonical `robot.json`, compiler-owned participant declarations,
//! the CLI-internal official catalog, and the
//! embedded metadata of the binaries under `bin/`. Nothing here reads source
//! or Cargo - staging already produced the layout, and this module is the
//! one place that turns that layout into the same
//! [`LaunchPlan`] the supervisor
//! consumes, whether the layout was just staged from a source project or
//! extracted from a `build.phoxal` bundle.
//!
//! It is the source-free counterpart of
//! [`build_launch_plan`](super::super::launch_plan::build_launch_plan): for the
//! same robot the two produce an identical process graph - same participants,
//! identities, policies, and plan digest - which is exactly what lets an
//! extracted bundle run without ever re-resolving from source.
//!
//! The jsonschema validator lives in the bin crate, so this module does not
//! run it. It returns the *input* the bin needs - a config-schema/config
//! pairing per declared user runtime - and the bin-side "validate through the
//! loader" entry runs `validate_user_service_config` over it. That keeps the
//! crate seam clean: core owns the derivation, the bin owns the validator it
//! already has.

use std::collections::BTreeMap;

use crate::identity::ProducerId;
use anyhow::{Context, Result};
use phoxal_runtime_contract::{
    BusProfile, ClockMode, DEFAULT_SHUTDOWN_GRACE_MS, ParticipantLaunch,
};

use super::super::launch_plan::{
    DEFAULT_ROUTER_CONNECT, LaunchMode, LaunchPlan, ParticipantExecution, ParticipantLaunchRecord,
    RobotLaunch, RunIdentity,
};
use super::{
    DriverSelection, LayoutInspection, RequiredRuntimeKind, RuntimeLayout, SelectedBinary,
};
use crate::runtime::{RuntimeFailurePolicy, StartupRequirement};

/// The non-`(layout, mode)` inputs the plan constructor is parameterized on, so
/// the derived launch plan stays a pure function of `(layout, mode, options)`
/// (#936). Today this carries only the driver policy; the run/start commands map
/// their `--drivers`/`--driver` options onto it, and `build` stages every driver
/// with [`DriverSelection::All`].
#[derive(Debug, Clone, Default)]
pub struct PlanOptions {
    /// Which component drivers are kept in the required set and planned.
    pub drivers: DriverSelection,
}

/// The complete launch graph a staged layout derives, plus the validation
/// input the bin-side "validate through the loader" entry runs its own
/// validator over. The plan is what the supervisor launches from; the config
/// pairings never enter the plan or its content digest.
#[derive(Debug, Clone)]
pub struct ConstructedPlan {
    /// The launch plan, identical in shape and digest to the one the
    /// [`build_launch_plan`](super::super::launch_plan::build_launch_plan)
    /// leg builds for the same robot.
    pub plan: LaunchPlan,
    /// One schema pairing per compiler-owned runtime config, so the project
    /// crate can validate the config the compiled declaration carries.
    pub user_runtime_configs: Vec<UserRuntimeConfig>,
}

/// A compiler-owned runtime config paired with the schema embedded in its
/// selected binary. This covers declared user services/tools and typed
/// compiler-generated official config such as behavior `{root, autostart}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRuntimeConfig {
    pub runtime_id: String,
    pub config_schema: serde_json::Value,
    /// Config from the compiled participant declaration (`None` when omitted).
    pub config: Option<serde_json::Value>,
    /// Declaring compiler domain, for diagnostics.
    pub family: &'static str,
}

impl RuntimeLayout {
    /// Open the staged layout at `root`, inspect every required runtime's
    /// selected binary (architecture check + embedded metadata, no execution),
    /// and construct the launch plan for `mode` - the exact "validate through
    /// the loader without supervising" entry `phoxal build` and the universal
    /// `run`/`start` share. This performs no supervisor, board, or socket
    /// construction; it stops at the immutable plan plus the validation input.
    ///
    /// The jsonschema check is the caller's to run (it lives in the bin
    /// crate); this returns its input on [`ConstructedPlan`].
    pub fn construct_plan(
        root: &std::path::Path,
        options: &PlanOptions,
        run: RunIdentity,
    ) -> Result<ConstructedPlan> {
        Self::construct_plan_with_inspection(root, options, LayoutInspection::Host, run)
    }

    /// [`Self::construct_plan`], inspecting each selected binary against the
    /// architecture `inspection` selects (#936): the host for an in-place
    /// run/start, or a declared `--target` for a `phoxal build` cross bundle
    /// that will never execute on this host.
    pub fn construct_plan_with_inspection(
        root: &std::path::Path,
        options: &PlanOptions,
        inspection: LayoutInspection,
        run: RunIdentity,
    ) -> Result<ConstructedPlan> {
        let layout = Self::open(root)?;
        let required = layout.required_runtimes(&options.drivers);
        let mut selected = BTreeMap::new();
        for runtime in &required {
            // Infrastructure (the router) is resolved by the CLI itself and is
            // never a plan participant, so it needs no inspection here.
            if runtime.kind == RequiredRuntimeKind::Infrastructure {
                continue;
            }
            selected.insert(
                runtime.binary_name.clone(),
                layout.inspect_for(runtime, inspection)?,
            );
        }
        layout.construct_plan_from_selected(options, &selected, run)
    }

    /// Construct the launch plan from an already-inspected selected-binary set,
    /// keyed by canonical `bin/` name. Split from [`Self::construct_plan`] so a
    /// caller that has already inspected the binaries (or wants to substitute
    /// them in a test) reuses the identical derivation.
    fn construct_plan_from_selected(
        &self,
        options: &PlanOptions,
        selected: &BTreeMap<String, SelectedBinary>,
        run: RunIdentity,
    ) -> Result<ConstructedPlan> {
        let robot_id = self.robot().robot_id().to_string();
        let namespace = self.robot().namespace().to_string();
        let bundle_root = self.root().to_path_buf();
        let service_clock = ClockMode::Real;

        let config_schema = |binary_name: &str| -> Result<serde_json::Value> {
            Ok(selected
                .get(binary_name)
                .with_context(|| format!("no inspected binary for `{binary_name}`"))?
                .meta
                .config_schema
                .clone())
        };

        let mut participants = Vec::new();
        let mut user_runtime_configs = Vec::new();

        for required in self.required_runtimes(&options.drivers) {
            match required.kind {
                // The router is CLI-resolved infrastructure, never a plan
                // participant (it is registered as a project-scoped process by
                // the supervisor directly).
                RequiredRuntimeKind::Infrastructure => {}
                RequiredRuntimeKind::OfficialService => {
                    let participant_id = required.identity.clone();
                    participants.push(cli_managed_record(
                        participant_id.clone(),
                        ParticipantExecution::OfficialArtifact {
                            binary_name: required.binary_name.clone(),
                        },
                        launch(
                            &participant_id,
                            &namespace,
                            &robot_id,
                            service_clock,
                            required.config.clone(),
                            &bundle_root,
                            None,
                            run,
                        ),
                    ));
                    if required.config.is_some() {
                        user_runtime_configs.push(UserRuntimeConfig {
                            runtime_id: participant_id,
                            config_schema: config_schema(&required.binary_name)?,
                            config: required.config.clone(),
                            family: "official",
                        });
                    }
                }
                RequiredRuntimeKind::OfficialTool => {
                    // Tools are per-robot instances (`tool-<short>-<robot_id>`)
                    // and always run under the real clock, exactly like the
                    // resolved-robot leg's tool loop.
                    let artifact_id = format!("tool-{}", required.identity);
                    let participant_id = format!("{artifact_id}-{robot_id}");
                    participants.push(cli_managed_record(
                        artifact_id,
                        ParticipantExecution::OfficialTool {
                            binary_name: required.binary_name.clone(),
                        },
                        launch(
                            &participant_id,
                            &namespace,
                            &robot_id,
                            ClockMode::Real,
                            None,
                            &bundle_root,
                            None,
                            run,
                        ),
                    ));
                }
                RequiredRuntimeKind::UserService => {
                    let participant_id = required.identity.clone();
                    participants.push(cli_managed_record(
                        participant_id.clone(),
                        ParticipantExecution::UserService {
                            binary_name: required.binary_name.clone(),
                        },
                        launch(
                            &participant_id,
                            &namespace,
                            &robot_id,
                            service_clock,
                            required.config.clone(),
                            &bundle_root,
                            None,
                            run,
                        ),
                    ));
                    user_runtime_configs.push(UserRuntimeConfig {
                        runtime_id: participant_id,
                        config_schema: config_schema(&required.binary_name)?,
                        config: required.config.clone(),
                        family: "services",
                    });
                }
                RequiredRuntimeKind::UserTool => {
                    let participant_id = required.identity.clone();
                    participants.push(cli_managed_record(
                        participant_id.clone(),
                        ParticipantExecution::UserTool {
                            binary_name: required.binary_name.clone(),
                        },
                        launch(
                            &participant_id,
                            &namespace,
                            &robot_id,
                            service_clock,
                            required.config.clone(),
                            &bundle_root,
                            None,
                            run,
                        ),
                    ));
                    // The tool's config validates against its embedded schema
                    // through the same pairing user services use (#950).
                    user_runtime_configs.push(UserRuntimeConfig {
                        runtime_id: participant_id,
                        config_schema: config_schema(&required.binary_name)?,
                        config: required.config.clone(),
                        family: "tools",
                    });
                }
                RequiredRuntimeKind::ComponentDriver => {
                    // One binary, N explicitly compiled driver declarations:
                    // driverless mounts of the same component type never enter
                    // the launch graph.
                    for declaration in self.participants() {
                        if declaration.kind != phoxal_manifest::ParticipantKind::Driver
                            || declaration.id != required.identity
                        {
                            continue;
                        }
                        let instance =
                            declaration.component_instance.as_deref().with_context(|| {
                                format!(
                                    "compiled driver '{}' has no component instance",
                                    declaration.id
                                )
                            })?;
                        if !options.drivers.includes_instance(instance) {
                            continue;
                        }
                        participants.push(cli_managed_record(
                            required.identity.clone(),
                            ParticipantExecution::ComponentDriver {
                                binary_name: required.binary_name.clone(),
                            },
                            launch(
                                instance,
                                &namespace,
                                &robot_id,
                                service_clock,
                                None,
                                &bundle_root,
                                Some(instance.to_string()),
                                run,
                            ),
                        ));
                    }
                }
            }
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
        Ok(ConstructedPlan {
            plan,
            user_runtime_configs,
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn launch(
    participant_id: &str,
    namespace: &str,
    robot_id: &str,
    clock: ClockMode,
    config: Option<serde_json::Value>,
    bundle_root: &std::path::Path,
    component_instance: Option<String>,
    run: RunIdentity,
) -> ParticipantLaunch {
    ParticipantLaunch {
        participant_id: participant_id.to_string(),
        execution: run.execution(),
        producer: ProducerId::mint(),
        execution_origin: Some(run.origin()),
        namespace: namespace.to_string(),
        robot_id: robot_id.to_string(),
        bus: BusProfile {
            connect_endpoints: vec![DEFAULT_ROUTER_CONNECT.to_string()],
        },
        clock,
        config,
        bundle_root: Some(bundle_root.to_path_buf()),
        component_instance,
        shutdown_grace_ms: DEFAULT_SHUTDOWN_GRACE_MS,
    }
}

fn cli_managed_record(
    artifact_id: String,
    execution: ParticipantExecution,
    launch: ParticipantLaunch,
) -> ParticipantLaunchRecord {
    ParticipantLaunchRecord {
        artifact_id,
        execution,
        launch,
        startup_requirement: StartupRequirement::Required,
        runtime_failure: RuntimeFailurePolicy::StopProject,
    }
}

#[cfg(test)]
mod tests {

    use std::fs;
    use std::path::{Path, PathBuf};

    use crate::check as graph_check;

    use super::super::super::catalog;
    use super::super::super::catalog::ArtifactKind;
    use super::super::super::launch_plan::{
        CheckedRobotLaunchInput, LaunchMode, ParticipantExecution, PlanRevision, build_launch_plan,
        runtime_layout_dir,
    };
    use super::super::super::resolver::{
        BundlePlan, ResolvedComponent, ResolvedComponentPackage, ResolvedComponentSource,
        ResolvedPlatformRuntime, ResolvedTool, ResolvedUserRuntime, official_binary_name,
    };
    use super::super::RuntimeLayout;
    use super::*;
    use crate::check::participant_metadata::host_architecture;
    use crate::check::source::SourceParticipant;

    const ROBOT_YAML: &str = r#"schema: robot/v0
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
      component: ddsm115
      mount_link: base
      driver:
        connection:
          type: serial
          port: /dev/ttyUSB0
          baud: 115200
    right_drive:
      component: ddsm115
      mount_link: base
      driver:
        connection:
          type: serial
          port: /dev/ttyUSB1
          baud: 115200
    caster:
      component: caster
      mount_link: base
services:
  mission:
    config:
      speed: 1
"#;

    /// Synthesize a host-architecture object file carrying `payload` as the
    /// phoxal metadata section, so the constructor inspects real object shapes
    /// off-disk without building a binary (mirrors the loader's own test).
    fn synthesize_binary(payload: &[u8]) -> Vec<u8> {
        synthesize_binary_for(host_architecture(), payload)
    }

    fn synthesize_binary_for(arch: object::Architecture, payload: &[u8]) -> Vec<u8> {
        use object::write::Object;
        let format = crate::check::participant_metadata::host_binary_format();
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
        obj.append_section_data(section, payload, 1);
        obj.write().expect("synthesize object file")
    }

    /// A binary carries no known config-schema by default; its declared `id`
    /// must still equal the required runtime's own identity, or `inspect_for`
    /// rejects it.
    fn metadata_payload(id: &str, kind: &str, config_schema: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schema": phoxal_runtime_contract::PARTICIPANT_METADATA_SCHEMA,
            "id": id,
            "kind": kind,
            "class": "checked",
            "config_schema": config_schema,
        }))
        .expect("metadata serializes")
    }

    fn required_kind(required: RequiredRuntimeKind) -> &'static str {
        match required {
            RequiredRuntimeKind::OfficialService | RequiredRuntimeKind::UserService => "service",
            RequiredRuntimeKind::OfficialTool | RequiredRuntimeKind::UserTool => "tool",
            RequiredRuntimeKind::ComponentDriver => "driver",
            RequiredRuntimeKind::Infrastructure => "tool",
        }
    }

    fn no_config_payload(id: &str, kind: RequiredRuntimeKind) -> Vec<u8> {
        metadata_payload(id, required_kind(kind), serde_json::json!({"type":"null"}))
    }

    /// Write canonical `robot.json` and synthesize a host-architecture binary
    /// under every canonical `bin/` name the Native profile requires, so the
    /// layout is a complete, runnable-shaped store. `payloads` overrides the
    /// metadata for named binaries; the rest carry the default (no-config)
    /// metadata, with the required runtime's own identity as `id` so
    /// `inspect_for`'s identity check passes.
    fn stage_layout(root: &Path, payloads: &[(&str, &[u8])]) -> Result<()> {
        fs::create_dir_all(root)?;
        super::super::write_test_layout(root, ROBOT_YAML)?;
        let bin = root.join("bin");
        let layout = RuntimeLayout::open(root)?;
        for required in layout.required_runtimes(&DriverSelection::All) {
            if required.kind == RequiredRuntimeKind::Infrastructure {
                continue;
            }
            let payload = payloads
                .iter()
                .find(|(name, _)| *name == required.binary_name)
                .map_or_else(
                    || no_config_payload(&required.identity, required.kind),
                    |(_, payload)| payload.to_vec(),
                );
            fs::write(bin.join(&required.binary_name), synthesize_binary(&payload))?;
        }
        Ok(())
    }

    fn catalog_short(package: &str) -> &str {
        package.rsplit_once('-').map_or(package, |(_, short)| short)
    }

    /// The resolved graph the legacy `build_launch_plan` leg consumes, built so
    /// its official set is exactly the CLI catalog the loader derives from - the
    /// only way the two legs can produce the same robot's plan.
    fn resolved_full_catalog(project_root: &Path, layout_root: &Path) -> Result<BundlePlan> {
        let robot = phoxal_manifest::source::robot::parse_from_string(ROBOT_YAML)?;
        let _ = project_root;
        let platform_runtimes = catalog::NATIVE
            .iter()
            .filter(|official| official.kind == ArtifactKind::Service)
            .map(|official| platform_runtime(catalog_short(official.package)))
            .collect();
        let tools = catalog::NATIVE
            .iter()
            .filter(|official| official.kind == ArtifactKind::Tool)
            .map(|official| official_tool(&format!("tool-{}", catalog_short(official.package))))
            .collect();
        Ok(BundlePlan {
            source_manifest: robot,
            compiled: Default::default(),
            train: "0.36.0".to_string(),
            target: layout_root
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("triple")
                .to_string(),
            platform_runtimes,
            simulators: Vec::new(),
            user_runtimes: vec![ResolvedUserRuntime {
                name: "mission".to_string(),
                path: PathBuf::from("services/mission"),
                source_hash: "hash".to_string(),
            }],
            user_tools: Vec::new(),
            undeclared_runtimes: Vec::new(),
            components: vec![
                driven_component("left_drive"),
                driven_component("right_drive"),
            ],
            tools,
            path_overrides: Vec::new(),
        })
    }

    fn platform_runtime(short: &str) -> ResolvedPlatformRuntime {
        ResolvedPlatformRuntime {
            name: short.to_string(),
            package: format!("phoxal/service-{short}"),
            kind: ArtifactKind::Service,
            path_override: None,
            train: "0.36.0".to_string(),
            target: None,
        }
    }

    fn official_tool(name: &str) -> ResolvedTool {
        let short = name.strip_prefix("tool-").unwrap_or(name);
        ResolvedTool {
            kind: ArtifactKind::Tool,
            name: name.to_string(),
            package: format!("phoxal/tool-{short}"),
            binary_name: official_binary_name(ArtifactKind::Tool, short),
            path_override: None,
            train: "0.36.0".to_string(),
            target: "triple".to_string(),
        }
    }

    fn driven_component(instance: &str) -> ResolvedComponent {
        ResolvedComponent {
            instance: instance.to_string(),
            source_name: "ddsm115".to_string(),
            assets: ResolvedComponentPackage {
                package: "phoxal/component-fixture".to_string(),
                kind: ArtifactKind::ComponentAssets,
                source: ResolvedComponentSource::Registry,
                resolved_dir: None,
                registry_runtime: None,
            },
            driver: None,
            has_driver: true,
        }
    }

    fn checked(
        participant_id: &str,
        artifact_id: &str,
        kind: graph_check::ParticipantKind,
        scope: graph_check::ParticipantScope,
    ) -> graph_check::ParticipantApis {
        graph_check::ParticipantApis {
            participant_id: participant_id.to_string(),
            artifact_id: artifact_id.to_string(),
            participant_kind: kind,
            participant_class: graph_check::ParticipantClass::Checked,
            config_schema: None,
            scope,
        }
    }

    /// The acceptance criterion made executable: the legacy resolved-robot leg
    /// and the layout constructor produce the same robot's plan - identical
    /// participant set, identities, policies, and content digest - so an
    /// extracted bundle runs the exact process graph the source run does.
    #[test]
    fn source_and_bundle_produce_identical_process_graphs() -> Result<()> {
        let project = tempfile::tempdir()?;
        let layout_root = runtime_layout_dir(project.path());
        stage_layout(&layout_root, &[])?;

        // The legacy leg: a resolved graph whose officials are the CLI catalog,
        // its checked participants synthesized to match, and the source records
        // the user service and workspace drivers build from.
        let mut resolved = resolved_full_catalog(project.path(), &layout_root)?;
        resolved.compiled.participants = RuntimeLayout::open(&layout_root)?.participants().to_vec();
        let mut checked_participants = catalog::NATIVE
            .iter()
            .filter(|official| official.kind == ArtifactKind::Service)
            .map(|official| {
                let short = catalog_short(official.package);
                checked(
                    short,
                    short,
                    graph_check::ParticipantKind::Service,
                    graph_check::ParticipantScope::Graph,
                )
            })
            .collect::<Vec<_>>();
        checked_participants.push(checked(
            "mission",
            "mission",
            graph_check::ParticipantKind::Service,
            graph_check::ParticipantScope::Graph,
        ));
        for instance in ["left_drive", "right_drive"] {
            checked_participants.push(checked(
                instance,
                "ddsm115",
                graph_check::ParticipantKind::Driver,
                graph_check::ParticipantScope::ComponentInstance(instance.to_string()),
            ));
        }
        let source_participants = vec![
            SourceParticipant::user_service("mission", PathBuf::from("services/mission")),
            SourceParticipant::component_driver_with_artifact_id(
                "left_drive",
                "ddsm115",
                PathBuf::from("components/ddsm115"),
            ),
            SourceParticipant::component_driver_with_artifact_id(
                "right_drive",
                "ddsm115",
                PathBuf::from("components/ddsm115"),
            ),
        ];
        // Both legs share one run identity so the comparison is about content.
        let run = RunIdentity::default();
        let legacy = build_launch_plan(
            LaunchMode::Run,
            &[CheckedRobotLaunchInput {
                project_root: project.path(),
                resolved: &resolved,
                checked_participants: &checked_participants,
                source_participants: &source_participants,
            }],
            run,
        )?;

        // The layout leg: read the same staged layout, source-free. Both legs
        // use the *same* run identity, since the identities are per supervised
        // run by design (#952 section B) and say nothing about plan content.
        let constructed =
            RuntimeLayout::construct_plan(&layout_root, &PlanOptions::default(), run)?;

        assert_eq!(
            crate::project::launch_plan::content_only(constructed.plan.clone()),
            crate::project::launch_plan::content_only(legacy.clone()),
            "the layout constructor must reproduce the resolved-robot plan exactly"
        );
        assert_eq!(
            PlanRevision::compile(1, constructed.plan.clone())?.digest,
            PlanRevision::compile(1, legacy.clone())?.digest,
            "identical plans must have identical content digests"
        );
        Ok(())
    }

    /// Recursively copy a staged layout directory, standing in for extracting a
    /// `build.phoxal` bundle to an arbitrary location.
    fn copy_dir(from: &Path, to: &Path) -> Result<()> {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            let target = to.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                copy_dir(&entry.path(), &target)?;
            } else {
                fs::copy(entry.path(), &target)?;
            }
        }
        Ok(())
    }

    /// Erase what is deployment- or run-scoped rather than plan content: the
    /// deployment root, and the identities every supervised run mints fresh
    /// (#952 section B).
    fn normalize_deployment_paths(plan: &mut LaunchPlan) {
        *plan = crate::project::launch_plan::content_only(plan.clone());
        for robot in &mut plan.robots {
            for participant in &mut robot.participants {
                participant.launch.bundle_root = None;
            }
        }
    }

    /// The acceptance criterion the extracted bundle depends on: the launch graph
    /// is determined by the staged layout's CONTENT (canonical model plus
    /// `bin/` metadata), not by where it lives. Staging a layout, then
    /// "extracting" (copying) it to an arbitrary directory and constructing
    /// again, yields the identical plan and content digest once the
    /// deployment-scoped layout root and freshly minted run identities are
    /// normalized. Nothing else is path-dependent, so a `build.phoxal` extracted
    /// anywhere runs the same process graph its source staged.
    #[test]
    fn staged_and_extracted_layout_construct_identical_graphs() -> Result<()> {
        let staged_project = tempfile::tempdir()?;
        let staged_root = runtime_layout_dir(staged_project.path());
        stage_layout(&staged_root, &[])?;
        let mut staged_plan = RuntimeLayout::construct_plan(
            &staged_root,
            &PlanOptions::default(),
            RunIdentity::default(),
        )?
        .plan;

        // "Extract the bundle" to an unrelated directory and construct again.
        let extracted = tempfile::tempdir()?;
        let extracted_root = extracted.path().join("build.phoxal.extracted");
        copy_dir(&staged_root, &extracted_root)?;
        let mut extracted_plan = RuntimeLayout::construct_plan(
            &extracted_root,
            &PlanOptions::default(),
            RunIdentity::default(),
        )?
        .plan;

        // Before normalization the deployment root genuinely differs, proving the
        // two constructions ran against distinct locations.
        assert_ne!(
            staged_plan, extracted_plan,
            "the deployment root must differ before normalization"
        );

        normalize_deployment_paths(&mut staged_plan);
        normalize_deployment_paths(&mut extracted_plan);
        assert_eq!(
            staged_plan, extracted_plan,
            "the layout content, not its path, must determine the process graph"
        );
        assert_eq!(
            PlanRevision::compile(1, staged_plan)?.digest,
            PlanRevision::compile(1, extracted_plan)?.digest,
            "content-identical plans must have identical content digests"
        );
        Ok(())
    }

    #[test]
    fn dormant_officials_are_planned_and_no_source_path_leaks() -> Result<()> {
        let project = tempfile::tempdir()?;
        let layout_root = runtime_layout_dir(project.path());
        stage_layout(&layout_root, &[])?;
        let constructed = RuntimeLayout::construct_plan(
            &layout_root,
            &PlanOptions::default(),
            RunIdentity::default(),
        )?;
        let robot = &constructed.plan.robots[0];

        // Every catalog service is a planned participant, dormant or not - the
        // full official set runs per robot (#945).
        for official in catalog::NATIVE
            .iter()
            .filter(|official| official.kind == ArtifactKind::Service)
        {
            let short = catalog_short(official.package);
            assert!(
                robot
                    .participants
                    .iter()
                    .any(|participant| participant.launch.participant_id == short),
                "dormant official service {short} must be planned"
            );
        }
        // Every planned participant is startup-required with the project-stop
        // failure policy the catalog owns.
        for participant in &robot.participants {
            assert_eq!(
                participant.startup_requirement,
                StartupRequirement::Required
            );
            assert_eq!(
                participant.runtime_failure,
                RuntimeFailurePolicy::StopProject
            );
        }
        // The router is CLI infrastructure, never a plan participant.
        assert!(
            !robot
                .participants
                .iter()
                .any(|participant| participant.artifact_id.contains("router")),
            "the infrastructure router is not a plan participant"
        );

        // A layout-built plan names only `bin/` binaries - no absolute or source
        // path ever appears in its serialized form (except the deployment
        // `bundle_root`, which is the layout itself).
        let serialized = serde_json::to_string(&constructed.plan)?;
        assert!(
            !serialized.contains("services/mission"),
            "no crate directory may leak into the plan: {serialized}"
        );
        assert!(
            !serialized.contains("components/ddsm115"),
            "no crate directory may leak into the plan: {serialized}"
        );
        Ok(())
    }

    #[test]
    fn one_driver_binary_serves_every_instance() -> Result<()> {
        let project = tempfile::tempdir()?;
        let layout_root = runtime_layout_dir(project.path());
        stage_layout(&layout_root, &[])?;
        let constructed = RuntimeLayout::construct_plan(
            &layout_root,
            &PlanOptions::default(),
            RunIdentity::default(),
        )?;
        let robot = &constructed.plan.robots[0];

        let drivers = robot
            .participants
            .iter()
            .filter(|participant| {
                matches!(
                    participant.execution,
                    ParticipantExecution::ComponentDriver { .. }
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(drivers.len(), 2, "both ddsm115 instances are launched");
        for driver in &drivers {
            assert_eq!(driver.artifact_id, "ddsm115");
            assert_eq!(driver.execution.binary_name(), "phoxal-component-ddsm115");
            assert_eq!(
                driver.launch.config, None,
                "compiler-side driver wiring must not become participant runtime config"
            );
        }
        let ids = drivers
            .iter()
            .map(|driver| driver.launch.participant_id.as_str())
            .collect::<Vec<_>>();
        assert!(ids.contains(&"left_drive") && ids.contains(&"right_drive"));
        Ok(())
    }

    #[test]
    fn user_service_config_schema_pairing_is_surfaced() -> Result<()> {
        let project = tempfile::tempdir()?;
        let layout_root = runtime_layout_dir(project.path());
        // The `mission` user-service binary carries a real object schema.
        let mission_meta = metadata_payload(
            "mission",
            "service",
            serde_json::json!({"type":"object","properties":{"speed":{"type":"integer"}}}),
        );
        stage_layout(&layout_root, &[("mission", &mission_meta)])?;
        let constructed = RuntimeLayout::construct_plan(
            &layout_root,
            &PlanOptions::default(),
            RunIdentity::default(),
        )?;

        let mission = constructed
            .user_runtime_configs
            .iter()
            .find(|config| config.runtime_id == "mission")
            .expect("mission config pairing surfaced");
        assert_eq!(
            mission.config_schema,
            serde_json::json!({"type":"object","properties":{"speed":{"type":"integer"}}})
        );
        assert_eq!(
            constructed.user_runtime_configs.len(),
            1,
            "only runtime config, not compiler-side driver wiring, is schema-paired"
        );
        Ok(())
    }

    #[test]
    fn a_foreign_arch_binary_fails_naming_the_identity() -> Result<()> {
        let project = tempfile::tempdir()?;
        let layout_root = runtime_layout_dir(project.path());
        stage_layout(&layout_root, &[])?;
        // Replace the mission binary with a foreign-architecture object file.
        let foreign = if host_architecture() == object::Architecture::X86_64 {
            object::Architecture::Aarch64
        } else {
            object::Architecture::X86_64
        };
        fs::write(
            layout_root.join("bin/mission"),
            synthesize_binary_for(
                foreign,
                &no_config_payload("mission", RequiredRuntimeKind::UserService),
            ),
        )?;
        let error = format!(
            "{:#}",
            RuntimeLayout::construct_plan(
                &layout_root,
                &PlanOptions::default(),
                RunIdentity::default()
            )
            .expect_err("a foreign-arch binary must fail construction")
        );
        assert!(error.contains("mission"), "{error}");
        assert!(error.contains("built for"), "{error}");
        Ok(())
    }

    #[test]
    fn a_missing_binary_fails_naming_the_identity() -> Result<()> {
        let project = tempfile::tempdir()?;
        let layout_root = runtime_layout_dir(project.path());
        stage_layout(&layout_root, &[])?;
        fs::remove_file(layout_root.join("bin/phoxal-service-drive"))?;
        let error = format!(
            "{:#}",
            RuntimeLayout::construct_plan(
                &layout_root,
                &PlanOptions::default(),
                RunIdentity::default()
            )
            .expect_err("a missing binary must fail construction")
        );
        assert!(error.contains("drive"), "{error}");
        assert!(error.contains("phoxal-service-drive"), "{error}");
        Ok(())
    }

    /// The regression the driver policy fixes (#936): `construct_plan` used to
    /// inspect every driver binary unconditionally, so a driven robot could not
    /// run on a host whose driver binaries it cannot inspect (a bench host
    /// missing that target's driver builds). With drivers off the excluded driver is
    /// never required, resolved, inspected, or planned - so a foreign-arch driver
    /// binary that hard-fails with drivers ON constructs cleanly with drivers OFF.
    #[test]
    fn drivers_off_excludes_drivers_and_never_inspects_them() -> Result<()> {
        let project = tempfile::tempdir()?;
        let layout_root = runtime_layout_dir(project.path());
        stage_layout(&layout_root, &[])?;
        // A foreign-architecture driver binary: inspection would hard-fail.
        let foreign = if host_architecture() == object::Architecture::X86_64 {
            object::Architecture::Aarch64
        } else {
            object::Architecture::X86_64
        };
        fs::write(
            layout_root.join("bin/phoxal-component-ddsm115"),
            synthesize_binary_for(
                foreign,
                &no_config_payload("ddsm115", RequiredRuntimeKind::ComponentDriver),
            ),
        )?;

        // Drivers on: the foreign driver binary is required and inspected, so
        // construction hard-fails naming the driver identity.
        let on_error = format!(
            "{:#}",
            RuntimeLayout::construct_plan(
                &layout_root,
                &PlanOptions::default(),
                RunIdentity::default()
            )
            .expect_err("a foreign-arch driver binary must fail with drivers on")
        );
        assert!(on_error.contains("ddsm115"), "{on_error}");

        // Drivers off: no driver is required/resolved/inspected/planned, so the
        // bench-host scenario becomes runnable despite the foreign binary.
        let constructed = RuntimeLayout::construct_plan(
            &layout_root,
            &PlanOptions {
                drivers: DriverSelection::None,
            },
            RunIdentity::default(),
        )?;
        assert!(
            !constructed.plan.robots[0]
                .participants
                .iter()
                .any(|participant| matches!(
                    participant.execution,
                    ParticipantExecution::ComponentDriver { .. }
                )),
            "drivers off must exclude every component driver from the plan"
        );
        Ok(())
    }

    /// A `--driver <ID>` subset keeps the shared driver binary required (so it is
    /// still inspected once) but plans only the named component instances; the
    /// unselected sibling instance is not planned.
    #[test]
    fn a_driver_subset_plans_only_the_selected_instances() -> Result<()> {
        let project = tempfile::tempdir()?;
        let layout_root = runtime_layout_dir(project.path());
        stage_layout(&layout_root, &[])?;
        let constructed = RuntimeLayout::construct_plan(
            &layout_root,
            &PlanOptions {
                drivers: DriverSelection::Only(["left_drive".to_string()].into_iter().collect()),
            },
            RunIdentity::default(),
        )?;
        let drivers = constructed.plan.robots[0]
            .participants
            .iter()
            .filter(|participant| {
                matches!(
                    participant.execution,
                    ParticipantExecution::ComponentDriver { .. }
                )
            })
            .map(|participant| participant.launch.participant_id.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            drivers,
            vec!["left_drive".to_string()],
            "only the selected instance is planned, though both share one binary"
        );
        Ok(())
    }
}
