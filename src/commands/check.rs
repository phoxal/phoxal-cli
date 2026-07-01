use std::{
    collections::BTreeSet,
    fmt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use phoxal::check as graph_check;
use phoxal::model::component::v1::CapabilityRef;
use phoxal::model::robot::v1::KinematicConfig;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::AppContext;
use crate::catalog::CATALOG;
use crate::commands::MessageFormat;
use crate::component_driver::component_crate_dir;
use crate::resolver::{
    ResolveOptions, ResolvedComponent, ResolvedRobot, RobotManifestExtras, discover_robot_yaml,
    load_robot_with_extras, resolve,
};
use crate::simulator_staging::cached_tool_path;
use crate::utils::{cargo_binary_name, resolve_project_path};

#[derive(Debug, Args)]
pub struct CheckCmd {
    #[arg(
        long,
        help = "Refresh official runtime images and host tools before running emit-apis."
    )]
    pub pull: bool,
    #[arg(
        long,
        value_name = "NAME",
        help = "Only build/check the named user runtime crate after resolving the full project."
    )]
    pub runtime: Option<String>,
    #[arg(
        long,
        value_enum,
        default_value_t = MessageFormat::Human,
        help = "Output format for the check result."
    )]
    pub message_format: MessageFormat,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckOptions {
    pub pull: bool,
    pub runtime: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RawEmitApis {
    pub artifact: RawArtifact,
    #[serde(default = "default_participant_class")]
    pub participant_class: String,
    pub api_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bus_abi: Option<String>,
    #[serde(alias = "contracts")]
    pub required_contracts: Vec<RawContract>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<Value>,
}

fn default_participant_class() -> String {
    "checked".to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct RawArtifact {
    pub kind: String,
    pub id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct RawContract {
    pub family: String,
    pub topic: String,
    pub direction: String,
    /// The framework's normalized transitive wire-shape hash for this contract
    /// body (`emit-apis` per-contract `schema_id`).
    pub schema_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckOutcome {
    pub missing_images: Vec<String>,
    pub report: graph_check::Report,
}

#[derive(Debug, Clone, Copy)]
pub struct CheckGraphContext<'a> {
    pub robot_graph: &'a graph_check::RobotGraph,
    pub manifest_extras: &'a RobotManifestExtras,
}

#[derive(Debug, Clone, Copy)]
pub struct CheckParticipants<'a> {
    pub platform_image_refs: &'a [(String, String)],
    pub user_runtime_images: &'a [UserRuntimeImageParticipant],
    pub tool_participants: &'a [ToolParticipant],
    pub source_participants: &'a [SourceParticipant],
}

impl CheckOutcome {
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.missing_images.is_empty() && self.report.is_ok()
    }
}

impl CheckCmd {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let project_root = app.project.root().to_path_buf();
        let options = CheckOptions {
            pull: self.pull,
            runtime: self.runtime.clone(),
        };
        let ui = app.ui;
        let result = tokio::task::spawn_blocking(move || run(&project_root, options, &ui))
            .await
            .context("check worker failed")??;

        // Emit the graph warnings and the v0 pre-stable warning to stderr BEFORE
        // the hard outcome check so a failing `phoxal check` still surfaces them.
        // These go to stderr only; JSON stdout (below) stays clean.
        for warning in &result.outcome.report.warnings {
            eprintln!("warning: {}", format_warning(warning));
        }
        eprintln!(
            "warning: v0 is pre-stable: artifacts built at different times may not interoperate; pin digests with phoxal-cli deploy build"
        );

        ensure_check_outcome_ok(&result.api_version, &result.channel, &result.outcome)?;

        let output = CheckOutput {
            status: "ok",
            api_version: result.api_version.clone(),
            channel: result.channel.clone(),
            participant_count: result.participant_count,
            warning_count: result.outcome.report.warnings.len(),
        };
        crate::commands::print_message(
            &output,
            || {
                println!(
                    "ok: {} participants validated against api_version {} (channel {})",
                    result.participant_count, result.api_version, result.channel
                );
                Ok(())
            },
            self.message_format,
        )?;
        Ok(())
    }
}

#[derive(Debug, Serialize)]
struct CheckOutput {
    status: &'static str,
    api_version: String,
    channel: String,
    participant_count: usize,
    warning_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CheckRunResult {
    api_version: String,
    channel: String,
    participant_count: usize,
    outcome: CheckOutcome,
}

fn run(
    project_start: &std::path::Path,
    options: CheckOptions,
    ui: &crate::Ui,
) -> Result<CheckRunResult> {
    let robot_path = discover_robot_yaml(project_start)
        .with_context(|| format!("failed to find robot.yaml from {}", project_start.display()))?;
    let project_root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?;
    let loaded = load_robot_with_extras(&robot_path)?;
    let robot = loaded.robot;
    let manifest_extras = loaded.extras;
    // `check` resolves live: it never pins registry/tool digests
    // (`resolve_external_artifacts: false`), but it does resolve git component
    // commits (`resolve_source_commits: true`) so component drivers can be
    // located and staged. A path-only / official-only graph needs no network; a
    // git component pinned to a commit SHA resolves offline; a tag/branch ref is
    // resolved live via `git ls-remote` (with an actionable error if the network
    // is unavailable).
    let resolved = resolve(
        &robot,
        project_root,
        &CATALOG,
        ResolveOptions {
            resolve_external_artifacts: false,
            resolve_source_commits: true,
        },
    )?;
    let platform_refs = resolved
        .platform_runtimes
        .iter()
        .map(|runtime| (runtime.name.clone(), runtime.tag_ref()))
        .collect::<Vec<_>>();
    if options.pull {
        crate::local_build::pull_platform_image_refs(&platform_refs)?;
    }
    let tool_names = resolved
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    crate::tool_provisioning::ensure_tool_binaries_with_mode(
        ui,
        &resolved,
        tool_names,
        crate::tool_provisioning::ProvisioningMode::from_pull(options.pull),
    )?;
    let tool_participants = tool_participants_from_resolved(&resolved)?;
    let all_source_participants =
        source_participants_from_resolved(project_root, &resolved, component_crate_dir)?;
    if let Some(runtime_name) = options.runtime.as_deref() {
        ensure_user_runtime_exists(&resolved, runtime_name)?;
    }
    let source_participants =
        source_participants_for_runtime(&all_source_participants, options.runtime.as_deref());
    let participant_count =
        platform_refs.len() + tool_participants.len() + source_participants.len();
    let robot_graph = robot_graph_from_resolved(&resolved);
    let outcome = run_check_with_context(
        &platform_refs,
        &tool_participants,
        &source_participants,
        CheckGraphContext {
            robot_graph: &robot_graph,
            manifest_extras: &manifest_extras,
        },
        fetch_emit_apis_from_docker,
        fetch_emit_apis_from_tool,
        build_emit_apis_from_source,
    )?;

    Ok(CheckRunResult {
        api_version: resolved.api_version,
        channel: resolved.channel.to_string(),
        participant_count,
        outcome,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceParticipant {
    pub name: String,
    pub expected_artifact_id: String,
    pub crate_dir: PathBuf,
    pub kind: SourceParticipantKind,
    /// Whether `check` should run the expensive build for this participant or
    /// reuse already-emitted metadata. `check --runtime <name>` scopes the
    /// build to the named user runtime (`Build`) while keeping every other
    /// participant in the graph via cached metadata (`UseCached`).
    pub build_mode: SourceBuildMode,
}

impl SourceParticipant {
    #[must_use]
    pub fn user_runtime(name: impl Into<String>, crate_dir: PathBuf) -> Self {
        let name = name.into();
        Self {
            expected_artifact_id: name.clone(),
            name,
            crate_dir,
            kind: SourceParticipantKind::UserRuntime,
            build_mode: SourceBuildMode::Build,
        }
    }

    #[must_use]
    pub fn component_driver(name: impl Into<String>, crate_dir: PathBuf) -> Self {
        let name = name.into();
        Self::component_driver_with_artifact_id(name.clone(), name, crate_dir)
    }

    #[must_use]
    pub fn component_driver_with_artifact_id(
        name: impl Into<String>,
        expected_artifact_id: impl Into<String>,
        crate_dir: PathBuf,
    ) -> Self {
        Self {
            name: name.into(),
            expected_artifact_id: expected_artifact_id.into(),
            crate_dir,
            kind: SourceParticipantKind::ComponentDriver,
            build_mode: SourceBuildMode::Build,
        }
    }

    fn kind_label(&self) -> &'static str {
        match self.kind {
            SourceParticipantKind::UserRuntime => "user runtime",
            SourceParticipantKind::ComponentDriver => "component driver",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceParticipantKind {
    UserRuntime,
    ComponentDriver,
}

/// How `check` obtains a source participant's `emit-apis` metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceBuildMode {
    /// Build the crate and run `emit-apis` on the fresh binary (expensive).
    Build,
    /// Reuse already-emitted metadata instead of rebuilding. Used for the
    /// participants outside a `check --runtime <name>` build scope so the full
    /// graph is still validated without rebuilding (or being failed by) crates
    /// the user did not ask to build.
    UseCached,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolParticipant {
    pub name: String,
    pub binary_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRuntimeImageParticipant {
    pub name: String,
    pub image_ref: String,
}

pub(crate) fn tool_participants_from_resolved(
    resolved: &ResolvedRobot,
) -> Result<Vec<ToolParticipant>> {
    resolved
        .tools
        .iter()
        .map(|tool| {
            Ok(ToolParticipant {
                name: tool.name.clone(),
                binary_path: cached_tool_path(&tool.name, &tool.resolved, &tool.binary_name)?,
            })
        })
        .collect()
}

pub(crate) fn source_participants_from_resolved(
    project_root: &Path,
    resolved: &ResolvedRobot,
    mut locate_component_crate: impl FnMut(&ResolvedComponent, &Path) -> Result<PathBuf>,
) -> Result<Vec<SourceParticipant>> {
    let mut participants = resolved
        .user_runtimes
        .iter()
        .map(|runtime| {
            SourceParticipant::user_runtime(
                runtime.name.clone(),
                resolve_project_path(project_root, &runtime.path),
            )
        })
        .collect::<Vec<_>>();

    for component in resolved
        .components
        .iter()
        .filter(|component| component.has_driver)
    {
        let crate_dir = locate_component_crate(component, project_root).with_context(|| {
            format!(
                "failed to locate component driver {} source",
                component.instance
            )
        })?;
        participants.push(SourceParticipant::component_driver_with_artifact_id(
            component.instance.clone(),
            component.source_name.clone(),
            crate_dir,
        ));
    }

    Ok(participants)
}

fn ensure_user_runtime_exists(resolved: &ResolvedRobot, runtime_name: &str) -> Result<()> {
    if !resolved
        .user_runtimes
        .iter()
        .any(|runtime| runtime.name == runtime_name)
    {
        let available = resolved
            .user_runtimes
            .iter()
            .map(|runtime| runtime.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        if available.is_empty() {
            bail!("user runtime '{runtime_name}' is not defined in user_participants");
        }
        bail!(
            "user runtime '{runtime_name}' is not defined in user_participants; available: {available}"
        );
    }
    Ok(())
}

/// Apply a `check --runtime <name>` build scope to the source participants.
///
/// Every participant stays in the returned set so the full graph is still
/// validated for topology/api consistency. When `runtime_name` is `Some`, only
/// the named user runtime is marked `Build`; every other source participant is
/// marked `UseCached` so the expensive build is scoped to the one crate the
/// user asked about, while broken or unrelated crates still contribute their
/// already-emitted metadata to the graph. With no scope, everything builds.
fn source_participants_for_runtime(
    source_participants: &[SourceParticipant],
    runtime_name: Option<&str>,
) -> Vec<SourceParticipant> {
    source_participants
        .iter()
        .map(|participant| {
            let mut participant = participant.clone();
            participant.build_mode = match runtime_name {
                Some(name)
                    if participant.kind == SourceParticipantKind::UserRuntime
                        && participant.name == name =>
                {
                    SourceBuildMode::Build
                }
                Some(_) => SourceBuildMode::UseCached,
                None => SourceBuildMode::Build,
            };
            participant
        })
        .collect()
}

pub fn run_check(
    resolved_platform_image_refs: &[(String, String)],
    tool_participants: &[ToolParticipant],
    source_participants: &[SourceParticipant],
    fetch: impl FnMut(&str) -> Result<RawEmitApis>,
    fetch_tool: impl FnMut(&Path) -> Result<RawEmitApis>,
    build: impl FnMut(&SourceParticipant) -> Result<RawEmitApis>,
) -> Result<CheckOutcome> {
    let robot_graph = graph_check::RobotGraph::default();
    let manifest_extras = RobotManifestExtras::default();
    run_check_with_context(
        resolved_platform_image_refs,
        tool_participants,
        source_participants,
        CheckGraphContext {
            robot_graph: &robot_graph,
            manifest_extras: &manifest_extras,
        },
        fetch,
        fetch_tool,
        build,
    )
}

pub fn run_check_with_context(
    resolved_platform_image_refs: &[(String, String)],
    tool_participants: &[ToolParticipant],
    source_participants: &[SourceParticipant],
    context: CheckGraphContext<'_>,
    fetch: impl FnMut(&str) -> Result<RawEmitApis>,
    fetch_tool: impl FnMut(&Path) -> Result<RawEmitApis>,
    build: impl FnMut(&SourceParticipant) -> Result<RawEmitApis>,
) -> Result<CheckOutcome> {
    run_check_with_deployed_user_runtime_images(
        CheckParticipants {
            platform_image_refs: resolved_platform_image_refs,
            user_runtime_images: &[],
            tool_participants,
            source_participants,
        },
        context,
        fetch,
        fetch_tool,
        build,
    )
}

pub fn run_check_with_deployed_user_runtime_images(
    inputs: CheckParticipants<'_>,
    context: CheckGraphContext<'_>,
    mut fetch: impl FnMut(&str) -> Result<RawEmitApis>,
    mut fetch_tool: impl FnMut(&Path) -> Result<RawEmitApis>,
    mut build: impl FnMut(&SourceParticipant) -> Result<RawEmitApis>,
) -> Result<CheckOutcome> {
    let mut missing_images = Vec::new();
    let mut participants = Vec::new();
    let mut config_problems = Vec::new();

    for (runtime_name, image_ref) in inputs.platform_image_refs {
        let raw = match fetch(image_ref) {
            Ok(raw) => raw,
            Err(error) if error.downcast_ref::<MissingImageError>().is_some() => {
                missing_images.push(image_ref.clone());
                continue;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to obtain emit-apis for runtime {runtime_name} ({image_ref})")
                });
            }
        };
        validate_artifact_identity("official service", runtime_name, "service", &raw)?;
        let participant = graph_check::ParticipantApis::try_from(raw).with_context(|| {
            format!("failed to interpret emit-apis for runtime {runtime_name} ({image_ref})")
        })?;
        participants.push(participant);
    }

    for runtime in inputs.user_runtime_images {
        let raw = fetch(&runtime.image_ref).with_context(|| {
            format!(
                "failed to obtain emit-apis for user runtime {} ({})",
                runtime.name, runtime.image_ref
            )
        })?;
        validate_runtime_artifact_identity("user runtime", &runtime.name, &raw)?;
        let participant = graph_check::ParticipantApis::try_from(raw).with_context(|| {
            format!(
                "failed to interpret emit-apis for user runtime {} ({})",
                runtime.name, runtime.image_ref
            )
        })?;
        if let Some(problem) = validate_user_runtime_config(
            &runtime.name,
            participant.config_schema.as_ref(),
            context.manifest_extras,
        ) {
            config_problems.push(problem);
        }
        participants.push(participant);
    }

    for tool in inputs.tool_participants {
        let raw = fetch_tool(&tool.binary_path).with_context(|| {
            format!(
                "failed to obtain emit-apis for tool {} ({})",
                tool.name,
                tool.binary_path.display()
            )
        })?;
        validate_artifact_identity("tool", &tool.name, "tool", &raw)?;
        let participant = graph_check::ParticipantApis::try_from(raw).with_context(|| {
            format!(
                "failed to interpret emit-apis for tool {} ({})",
                tool.name,
                tool.binary_path.display()
            )
        })?;
        participants.push(participant);
    }

    for participant in inputs.source_participants {
        let raw = build(participant).with_context(|| {
            format!(
                "failed to obtain emit-apis for {} {} ({})",
                participant.kind_label(),
                participant.name,
                participant.crate_dir.display()
            )
        })?;
        validate_source_artifact_identity(participant, &raw)?;
        let mut participant_apis =
            graph_check::ParticipantApis::try_from(raw).with_context(|| {
                format!(
                    "failed to interpret emit-apis for {} {} ({})",
                    participant.kind_label(),
                    participant.name,
                    participant.crate_dir.display()
                )
            })?;
        if participant.kind == SourceParticipantKind::ComponentDriver {
            // A component driver is launched once per component instance. Several
            // instances of the same driver share `artifact_id` (validated against
            // the emitted artifact identity), so key graph membership and
            // diagnostics by the concrete instance id instead.
            participant_apis.participant_id = participant.name.clone();
            participant_apis.scope =
                graph_check::ParticipantScope::ComponentInstance(participant.name.clone());
        } else if participant.kind == SourceParticipantKind::UserRuntime
            && let Some(problem) = validate_user_runtime_config(
                &participant.name,
                participant_apis.config_schema.as_ref(),
                context.manifest_extras,
            )
        {
            config_problems.push(problem);
        }
        participants.push(participant_apis);
    }

    let mut report = graph_check::check_graph_with_topology(&participants, context.robot_graph);
    report.problems.extend(config_problems);
    Ok(CheckOutcome {
        missing_images,
        report,
    })
}

pub(crate) fn robot_graph_from_resolved(resolved: &ResolvedRobot) -> graph_check::RobotGraph {
    let mut component_capabilities = Vec::new();
    for (instance_name, instance) in &resolved.robot.components.instances {
        for (capability_id, parameters) in &instance.parameters {
            component_capabilities.push(graph_check::ComponentCapability {
                instance: instance_name.clone(),
                capability: capability_id.clone(),
                kind: parameters.kind_name().to_string(),
            });
        }
    }
    component_capabilities.sort();
    component_capabilities.dedup();

    let mut motion_capabilities = BTreeSet::new();
    collect_motion_capabilities(&resolved.robot.motion.kinematic, &mut motion_capabilities);

    graph_check::RobotGraph {
        component_capabilities,
        motion_capabilities,
    }
}

fn collect_motion_capabilities(
    kinematic: &KinematicConfig,
    motion_capabilities: &mut BTreeSet<(String, String)>,
) {
    let mut insert = |capability: &CapabilityRef| {
        motion_capabilities.insert((
            capability.component_id.clone(),
            capability.capability_id.clone(),
        ));
    };
    match kinematic {
        KinematicConfig::Differential {
            left_actuators,
            right_actuators,
            left_encoders,
            right_encoders,
            ..
        } => {
            for capability in left_actuators
                .iter()
                .chain(right_actuators)
                .chain(left_encoders)
                .chain(right_encoders)
            {
                insert(capability);
            }
        }
        KinematicConfig::Mecanum {
            front_left_actuator,
            front_right_actuator,
            rear_left_actuator,
            rear_right_actuator,
            ..
        } => {
            for capability in [
                front_left_actuator,
                front_right_actuator,
                rear_left_actuator,
                rear_right_actuator,
            ] {
                insert(capability);
            }
        }
        KinematicConfig::Ackermann {
            steering_actuator,
            drive_actuator,
            steering_encoder,
            drive_encoder,
            ..
        } => {
            insert(steering_actuator);
            insert(drive_actuator);
            if let Some(capability) = steering_encoder {
                insert(capability);
            }
            if let Some(capability) = drive_encoder {
                insert(capability);
            }
        }
        KinematicConfig::Omnidirectional {
            actuators,
            encoders,
        } => {
            for capability in actuators.iter().chain(encoders) {
                insert(capability);
            }
        }
    }
}

fn validate_user_runtime_config(
    runtime_id: &str,
    schema: Option<&Value>,
    manifest_extras: &RobotManifestExtras,
) -> Option<graph_check::Problem> {
    let schema = schema?;
    let config = manifest_extras
        .user_runtime_config(runtime_id)
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let errors = validate_json_schema(
        schema,
        &config,
        &format!("user_participants.{runtime_id}.config"),
    );
    if errors.is_empty() {
        None
    } else {
        Some(graph_check::Problem::InvalidConfig {
            runtime_id: runtime_id.to_string(),
            errors,
        })
    }
}

fn validate_json_schema(schema: &Value, value: &Value, path: &str) -> Vec<String> {
    let validator = match jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .build(schema)
    {
        Ok(validator) => validator,
        Err(error) => {
            return vec![format!("{path}: emitted config_schema is invalid: {error}")];
        }
    };

    validator
        .iter_errors(value)
        .map(|error| {
            let instance_path = error.instance_path().to_string();
            if instance_path.is_empty() {
                format!("{path}: {error}")
            } else {
                format!("{path}{instance_path}: {error}")
            }
        })
        .collect()
}

pub(crate) fn fetch_emit_apis_from_docker(image_ref: &str) -> Result<RawEmitApis> {
    let output = crate::shell::run_output("docker", ["run", "--rm", image_ref, "emit-apis"], None)
        .with_context(|| format!("failed to start docker emit-apis for {image_ref}"))?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let cause = classify_docker_emit_apis_failure(image_ref, &stdout, &stderr);
        return match cause {
            DockerEmitApisFailure::MissingImage(message) => {
                Err(MissingImageError::new(anyhow!(message)).into())
            }
            DockerEmitApisFailure::Hard(message) => {
                bail!("docker emit-apis for {image_ref} failed: {message}")
            }
        };
    }
    let output = String::from_utf8(output.stdout)
        .with_context(|| format!("docker emit-apis output for {image_ref} was not UTF-8"))?;
    serde_json::from_str(&output)
        .with_context(|| format!("docker emit-apis output for {image_ref} was not valid JSON"))
}

enum DockerEmitApisFailure {
    MissingImage(String),
    Hard(String),
}

fn classify_docker_emit_apis_failure(
    image_ref: &str,
    stdout: &str,
    stderr: &str,
) -> DockerEmitApisFailure {
    let combined = format!("{stdout}\n{stderr}");
    let lower = combined.to_ascii_lowercase();
    if is_missing_image_failure(&lower) {
        return DockerEmitApisFailure::MissingImage(format!(
            "official runtime image {image_ref} is not available: {}",
            first_nonempty_line(&combined)
        ));
    }
    let cause = if lower.contains("cannot connect to the docker daemon")
        || lower.contains("is the docker daemon running")
    {
        "Docker daemon is not running".to_string()
    } else if lower.contains("unauthorized")
        || lower.contains("authentication required")
        || lower.contains("access denied")
        || lower.contains("requested access to the resource is denied")
    {
        "registry authentication or authorization failed".to_string()
    } else if lower.contains("executable file not found")
        || lower.contains("unknown command")
        || lower.contains("no such file or directory")
    {
        "artifact does not expose a runnable top-level `emit-apis` command".to_string()
    } else {
        format!(
            "container exited while running `emit-apis`: {}",
            first_nonempty_line(&combined)
        )
    };
    DockerEmitApisFailure::Hard(cause)
}

fn is_missing_image_failure(lower: &str) -> bool {
    if lower.contains("unauthorized")
        || lower.contains("authentication required")
        || lower.contains("requested access to the resource is denied")
        || lower.contains("executable file not found")
    {
        return false;
    }
    lower.contains("manifest unknown")
        || lower.contains("no matching manifest")
        || lower.contains("manifest for")
        || lower.contains("repository does not exist")
        || (lower.contains("not found") && lower.contains("manifest"))
}

fn first_nonempty_line(output: &str) -> String {
    output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no stdout/stderr")
        .to_string()
}

pub(crate) fn fetch_emit_apis_from_tool(binary_path: &Path) -> Result<RawEmitApis> {
    let executable = binary_path.to_string_lossy();
    let output = crate::shell::run_stdout(executable.as_ref(), ["emit-apis"], None)?;
    serde_json::from_str(&output).with_context(|| {
        format!(
            "emit-apis output from tool {} was not valid JSON",
            binary_path.display()
        )
    })
}

pub(crate) fn build_emit_apis_from_source(participant: &SourceParticipant) -> Result<RawEmitApis> {
    match participant.build_mode {
        SourceBuildMode::Build => {
            let raw = build_emit_apis_by_building(&participant.crate_dir)?;
            // Cache the freshly-emitted metadata keyed by the source tree so a
            // later `check --runtime <other>` can reuse it without rebuilding.
            if let Err(error) = write_source_emit_apis_cache(&participant.crate_dir, &raw) {
                tracing::debug!(
                    "failed to cache emit-apis for {}: {error:#}",
                    participant.crate_dir.display()
                );
            }
            Ok(raw)
        }
        SourceBuildMode::UseCached => read_source_emit_apis_cache(&participant.crate_dir)
            .with_context(|| {
                format!(
                    "no cached emit-apis for {} {} ({}); run `phoxal check` once (no --runtime) \
                     to build every participant and populate the cache, then re-run \
                     `phoxal check --runtime <name>`",
                    participant.kind_label(),
                    participant.name,
                    participant.crate_dir.display()
                )
            }),
    }
}

fn build_emit_apis_by_building(dir: &Path) -> Result<RawEmitApis> {
    let crate_dir = dir
        .canonicalize()
        .with_context(|| format!("failed to canonicalize source crate {}", dir.display()))?;
    let binary_name = cargo_binary_name(&crate_dir, None)?;
    // Build + run via `cargo run` rather than locating the binary by hand: a crate
    // that is a workspace member (e.g. a `phoxal/components` driver) compiles into the
    // *workspace-root* `target/`, not `<crate_dir>/target/`, so a fixed
    // `<crate_dir>/target/debug/<bin>` path would miss it. `cargo run` resolves the
    // location workspace-aware, and `--quiet` keeps stdout to just the binary's
    // `emit-apis` JSON (cargo's own progress goes to stderr).
    let output = crate::shell::run_stdout(
        "cargo",
        ["run", "--quiet", "--bin", &binary_name, "--", "emit-apis"],
        Some(&crate_dir),
    )
    .with_context(|| {
        format!(
            "failed to build/run `{binary_name} emit-apis` for source crate {}",
            crate_dir.display()
        )
    })?;
    serde_json::from_str(&output).with_context(|| {
        format!(
            "emit-apis output from source crate {} was not valid JSON",
            crate_dir.display()
        )
    })
}

/// Cache file for a source crate's last-built `emit-apis`, keyed by the source
/// tree hash so cached metadata always matches the current source. A scoped
/// `check --runtime <name>` reads this for the participants it does not rebuild.
fn source_emit_apis_cache_path(crate_dir: &Path) -> Result<PathBuf> {
    let source_hash = crate::utils::hash_tree(crate_dir).with_context(|| {
        format!(
            "failed to hash source crate {} for emit-apis cache",
            crate_dir.display()
        )
    })?;
    Ok(crate::host_paths::cache_dir()?
        .join("emit-apis")
        .join(format!("{source_hash}.json")))
}

fn write_source_emit_apis_cache(crate_dir: &Path, raw: &RawEmitApis) -> Result<()> {
    let path = source_emit_apis_cache_path(crate_dir)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("failed to create emit-apis cache dir {}", parent.display())
        })?;
    }
    let json = serde_json::to_string(raw).context("failed to serialize emit-apis for cache")?;
    std::fs::write(&path, json)
        .with_context(|| format!("failed to write emit-apis cache {}", path.display()))
}

fn read_source_emit_apis_cache(crate_dir: &Path) -> Result<RawEmitApis> {
    let path = source_emit_apis_cache_path(crate_dir)?;
    let json = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read emit-apis cache {}", path.display()))?;
    serde_json::from_str(&json)
        .with_context(|| format!("cached emit-apis {} was not valid JSON", path.display()))
}

fn validate_runtime_artifact_identity(
    label: &str,
    expected_id: &str,
    raw: &RawEmitApis,
) -> Result<()> {
    validate_artifact_identity(label, expected_id, "service", raw)
}

fn validate_source_artifact_identity(
    participant: &SourceParticipant,
    raw: &RawEmitApis,
) -> Result<()> {
    let expected_kind = match participant.kind {
        SourceParticipantKind::UserRuntime => "service",
        SourceParticipantKind::ComponentDriver => "driver",
    };
    validate_artifact_identity(
        participant.kind_label(),
        participant.expected_artifact_id.as_str(),
        expected_kind,
        raw,
    )
}

fn validate_artifact_identity(
    label: &str,
    expected_id: &str,
    expected_kind: &str,
    raw: &RawEmitApis,
) -> Result<()> {
    if raw.artifact.id != expected_id {
        bail!(
            "{label} emit-apis artifact.id '{}' does not match expected artifact id '{}'",
            raw.artifact.id,
            expected_id
        );
    }
    // Keep accepting the legacy universal kind until the cross-repo migration
    // lands. phoxal/components drivers, and any tools still on phoxal 0.19 with
    // `#[derive(Runtime)]`, emit `kind = "runtime"`. Exact `service`/`driver`/
    // `tool` enforcement is deferred until the components and tools crates move
    // to phoxal 0.20+ true kinds.
    if raw.artifact.kind != expected_kind && raw.artifact.kind != "runtime" {
        bail!(
            "{label} emit-apis artifact.kind '{}' is neither the expected kind '{}' nor the tolerated legacy 'runtime'",
            raw.artifact.kind,
            expected_kind
        );
    }
    Ok(())
}

impl TryFrom<RawEmitApis> for graph_check::ParticipantApis {
    type Error = anyhow::Error;

    fn try_from(raw: RawEmitApis) -> Result<Self> {
        let artifact_id = raw.artifact.id;
        let participant_class =
            graph_check::ParticipantClass::parse(&raw.participant_class).unwrap_or_default();
        let contracts = raw
            .required_contracts
            .into_iter()
            .map(|contract| {
                let direction =
                    graph_check::Direction::parse(&contract.direction).ok_or_else(|| {
                        anyhow!(
                            "unrecognized emit-apis direction '{}' for artifact '{}'",
                            contract.direction,
                            artifact_id
                        )
                    })?;
                Ok(graph_check::Contract {
                    family: contract.family,
                    topic: contract.topic,
                    direction,
                    schema_id: contract.schema_id,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(Self {
            // Default the participant id to the artifact id; callers that launch
            // one artifact per instance (component drivers) override it with the
            // concrete instance id below.
            participant_id: artifact_id.clone(),
            artifact_id,
            participant_class,
            api_version: raw.api_version,
            bus_abi: raw.bus_abi,
            config_schema: raw.config_schema,
            scope: graph_check::ParticipantScope::Graph,
            contracts,
        })
    }
}

pub(crate) fn ensure_check_outcome_ok(
    api_version: &str,
    channel: &str,
    outcome: &CheckOutcome,
) -> Result<()> {
    if !outcome.missing_images.is_empty() {
        bail!(
            "{}",
            format_missing_images_error(api_version, channel, &outcome.missing_images)
        );
    }

    if !outcome.report.is_ok() {
        bail!("{}", format_report_error(&outcome.report));
    }

    Ok(())
}

fn format_missing_images_error(
    api_version: &str,
    channel: &str,
    missing_images: &[String],
) -> String {
    let mut message = format!("API version {api_version} is not available on channel {channel}");
    message.push_str("\n\nMissing official runtime images:");
    for image_ref in missing_images {
        message.push_str("\n  - ");
        message.push_str(image_ref);
    }
    message.push_str("\n\nFix:");
    if let Some(api) = suggested_available_api_version(api_version) {
        message.push_str("\n  - use api_version: ");
        message.push_str(api);
    } else {
        message.push_str("\n  - use an api_version listed by `phoxal-cli version`");
    }
    message.push_str(
        "\n  - or use phoxal_participants.channel: edge if this API version is intentionally experimental",
    );
    message.push_str("\n  - or wait until Phoxal publishes the complete ");
    message.push_str(api_version);
    message.push('-');
    message.push_str(channel);
    message.push_str(" official runtime set");
    message
}

fn suggested_available_api_version(requested: &str) -> Option<&'static str> {
    let mut versions = CATALOG
        .entries
        .iter()
        .flat_map(|entry| entry.api_versions.iter().copied())
        .filter(|api| *api != requested)
        .collect::<Vec<_>>();
    versions.sort_unstable();
    versions.dedup();
    versions.pop()
}

fn format_report_error(report: &graph_check::Report) -> String {
    let mut message = String::from("robot graph check failed:");
    for problem in &report.problems {
        message.push_str("\n  - ");
        message.push_str(&format_problem(problem));
    }
    message
}

fn format_problem(problem: &graph_check::Problem) -> String {
    match problem {
        graph_check::Problem::ContractSchemaMismatch {
            family,
            topic,
            schema_ids,
        } => {
            let reporters = schema_ids
                .iter()
                .map(|(schema_id, participants)| {
                    format!("{schema_id} (reported by {})", participants.join(", "))
                })
                .collect::<Vec<_>>()
                .join("; ");
            format!(
                "contract {family} ({topic}) has disagreeing wire shapes: {reporters}; \
                 rebuild the disagreeing side(s) so they compile against the same contract wire shape (schema_id)"
            )
        }
        graph_check::Problem::MissingProducer {
            family,
            topic,
            consumers,
        } => {
            format!(
                "no producer for {family} ({topic}); consumed by: {}",
                consumers.join(", ")
            )
        }
        graph_check::Problem::MissingConsumer {
            family,
            topic,
            producers,
        } => {
            format!(
                "no consumer for {family} ({topic}); produced by: {}",
                producers.join(", ")
            )
        }
        graph_check::Problem::MultipleServerResponders {
            family,
            topic,
            responders,
        } => {
            format!(
                "query topic {family} ({topic}) has more than one server: {}; keep exactly one",
                responders.join(", ")
            )
        }
        graph_check::Problem::InvalidConfig { runtime_id, errors } => {
            format!(
                "invalid config for user runtime {runtime_id}: {}",
                errors.join("; ")
            )
        }
        graph_check::Problem::UnresolvedComponentTemplate {
            artifact_id,
            template,
            family,
            missing,
        } => {
            format!(
                "unresolved component template for {artifact_id}: {family} ({template}) expands to no concrete topic ({missing})"
            )
        }
    }
}

fn format_warning(warning: &graph_check::Warning) -> String {
    match warning {
        graph_check::Warning::MissingConsumer {
            family,
            topic,
            producers,
        } => {
            format!(
                "no consumer for {family} ({topic}); produced by: {}",
                producers.join(", ")
            )
        }
    }
}

#[derive(Debug)]
pub struct MissingImageError {
    source: anyhow::Error,
}

impl MissingImageError {
    pub fn new(source: anyhow::Error) -> Self {
        Self { source }
    }
}

impl fmt::Display for MissingImageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("official runtime image could not be obtained")
    }
}

impl std::error::Error for MissingImageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::ResolvedComponentSource;
    use graph_check::{Direction, ParticipantClass, Problem};
    use phoxal::model::robot::v1::{Channel, Robot};
    use std::collections::BTreeMap;

    #[test]
    fn healthy_graph_passes_with_fake_emit_apis() -> Result<()> {
        let images = vec![("mission".to_string(), "mission:ok".to_string())];
        let sources = vec![SourceParticipant::user_runtime(
            "drive".to_string(),
            PathBuf::from("/fake/project/runtimes/drive"),
        )];

        let outcome = run_check(
            &images,
            &[],
            &sources,
            |image_ref| match image_ref {
                "mission:ok" => Ok(raw(
                    "mission",
                    "y2026_1",
                    &[("drive::Target", "drive/target", "publish")],
                )),
                unexpected => bail!("unexpected image {unexpected}"),
            },
            |_| bail!("no tools should be fetched"),
            |participant| {
                let dir = participant.crate_dir.as_path();
                if dir == Path::new("/fake/project/runtimes/drive") {
                    Ok(raw(
                        "drive",
                        "y2026_1",
                        &[("drive::Target", "drive/target", "subscribe")],
                    ))
                } else {
                    bail!("unexpected source dir {}", dir.display())
                }
            },
        )?;

        assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
        Ok(())
    }

    #[test]
    fn healthy_graph_passes_with_platform_and_component_driver_source() -> Result<()> {
        let images = vec![("mission".to_string(), "mission:ok".to_string())];
        let sources = vec![SourceParticipant::component_driver_with_artifact_id(
            "left_drive".to_string(),
            "ddsm115".to_string(),
            PathBuf::from("/fake/project/components/ddsm115"),
        )];

        let outcome = run_check(
            &images,
            &[],
            &sources,
            |image_ref| match image_ref {
                "mission:ok" => Ok(raw(
                    "mission",
                    "y2026_1",
                    &[("drive::Target", "drive/target", "publish")],
                )),
                unexpected => bail!("unexpected image {unexpected}"),
            },
            |_| bail!("no tools should be fetched"),
            |participant| {
                let dir = participant.crate_dir.as_path();
                if dir == Path::new("/fake/project/components/ddsm115") {
                    Ok(raw_kind(
                        "driver",
                        "ddsm115",
                        "y2026_1",
                        &[("drive::Target", "drive/target", "subscribe")],
                    ))
                } else {
                    bail!("unexpected source dir {}", dir.display())
                }
            },
        )?;

        assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
        Ok(())
    }

    #[test]
    fn privileged_tools_are_included_in_schema_agreement() -> Result<()> {
        let tools = vec![ToolParticipant {
            name: "joypad".to_string(),
            binary_path: PathBuf::from("/fake/cache/joypad"),
        }];
        let sources = vec![SourceParticipant::user_runtime(
            "drive".to_string(),
            PathBuf::from("/fake/project/runtimes/drive"),
        )];

        let outcome = run_check(
            &[],
            &tools,
            &sources,
            |_| bail!("no platform images should be fetched"),
            |path| {
                if path == Path::new("/fake/cache/joypad") {
                    Ok(raw_kind_with_schema(
                        "tool",
                        "joypad",
                        "y2026_1",
                        &[("drive::Target", "drive/target", "subscribe", "bbbb")],
                        "privileged",
                    ))
                } else {
                    bail!("unexpected tool path {}", path.display())
                }
            },
            |participant| {
                let dir = participant.crate_dir.as_path();
                if dir == Path::new("/fake/project/runtimes/drive") {
                    Ok(raw_with_schema(
                        "drive",
                        "y2026_1",
                        &[("drive::Target", "drive/target", "publish", "aaaa")],
                    ))
                } else {
                    bail!("unexpected source dir {}", dir.display())
                }
            },
        )?;

        assert_eq!(
            outcome.report.problems,
            vec![Problem::ContractSchemaMismatch {
                family: "drive::Target".to_string(),
                topic: "drive/target".to_string(),
                schema_ids: vec![
                    ("aaaa".to_string(), vec!["drive".to_string()]),
                    ("bbbb".to_string(), vec!["joypad".to_string()]),
                ],
            }]
        );
        Ok(())
    }

    #[test]
    fn privileged_tools_are_exempt_from_topology() -> Result<()> {
        let tools = vec![ToolParticipant {
            name: "joypad".to_string(),
            binary_path: PathBuf::from("/fake/cache/joypad"),
        }];

        let outcome = run_check(
            &[],
            &tools,
            &[],
            |_| bail!("no platform images should be fetched"),
            |path| {
                if path == Path::new("/fake/cache/joypad") {
                    Ok(raw_kind_class(
                        "tool",
                        "joypad",
                        "y2026_1",
                        &[
                            ("drive::Target", "drive/target", "subscribe"),
                            ("odometry::State", "odometry/state", "publish"),
                        ],
                        "privileged",
                    ))
                } else {
                    bail!("unexpected tool path {}", path.display())
                }
            },
            |_| bail!("no source runtimes should be built"),
        )?;

        assert!(outcome.report.problems.is_empty());
        assert!(outcome.report.warnings.is_empty());
        Ok(())
    }

    #[test]
    fn deployed_user_runtime_images_are_checked_from_image_refs() -> Result<()> {
        let user_images = vec![UserRuntimeImageParticipant {
            name: "avoid".to_string(),
            image_ref: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .to_string(),
        }];
        let sources = vec![SourceParticipant::component_driver_with_artifact_id(
            "left_drive".to_string(),
            "ddsm115".to_string(),
            PathBuf::from("/fake/project/components/ddsm115"),
        )];
        let robot_graph = graph_check::RobotGraph::default();
        let extras = RobotManifestExtras::default();

        let mut fetched_images = Vec::new();
        let mut built_sources = Vec::new();
        let outcome = run_check_with_deployed_user_runtime_images(
            CheckParticipants {
                platform_image_refs: &[],
                user_runtime_images: &user_images,
                tool_participants: &[],
                source_participants: &sources,
            },
            CheckGraphContext {
                robot_graph: &robot_graph,
                manifest_extras: &extras,
            },
            |image_ref| {
                fetched_images.push(image_ref.to_string());
                Ok(raw("avoid", "y2026_1", &[]))
            },
            |_| bail!("no tools should be fetched"),
            |participant| {
                let dir = participant.crate_dir.as_path();
                built_sources.push(dir.to_path_buf());
                Ok(raw_kind("driver", "ddsm115", "y2026_1", &[]))
            },
        )?;

        assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
        assert_eq!(
            fetched_images,
            vec!["sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
        );
        assert_eq!(
            built_sources,
            vec![PathBuf::from("/fake/project/components/ddsm115")]
        );
        Ok(())
    }

    #[test]
    fn source_wrong_schema_id_fails_with_mismatch_problem() -> Result<()> {
        // A platform publisher and a source subscriber share `drive/target` but
        // report different `schema_id`s -> one `ContractSchemaMismatch`.
        let images = vec![("mission".to_string(), "mission:ok".to_string())];
        let sources = vec![SourceParticipant::user_runtime(
            "drive".to_string(),
            PathBuf::from("/fake/project/runtimes/drive"),
        )];

        let outcome = run_check(
            &images,
            &[],
            &sources,
            |image_ref| match image_ref {
                "mission:ok" => Ok(raw_with_schema(
                    "mission",
                    "y2026_1",
                    &[("drive::Target", "drive/target", "publish", "aaaa")],
                )),
                unexpected => bail!("unexpected image {unexpected}"),
            },
            |_| bail!("no tools should be fetched"),
            |_| {
                Ok(raw_with_schema(
                    "drive",
                    "y2026_1",
                    &[("drive::Target", "drive/target", "subscribe", "bbbb")],
                ))
            },
        )?;

        assert_eq!(
            outcome.report.problems,
            vec![Problem::ContractSchemaMismatch {
                family: "drive::Target".to_string(),
                topic: "drive/target".to_string(),
                schema_ids: vec![
                    ("aaaa".to_string(), vec!["mission".to_string()]),
                    ("bbbb".to_string(), vec!["drive".to_string()]),
                ],
            }]
        );
        assert!(!outcome.is_ok());
        Ok(())
    }

    #[test]
    fn user_runtime_artifact_id_must_match_manifest_key() {
        let sources = vec![SourceParticipant::user_runtime(
            "avoid".to_string(),
            PathBuf::from("/fake/project/runtimes/avoid"),
        )];

        let error = run_check(
            &[],
            &[],
            &sources,
            |_| bail!("no platform images should be fetched"),
            |_| bail!("no tools should be fetched"),
            |_| Ok(raw("surprise", "y2026_1", &[])),
        )
        .expect_err("mismatched user runtime artifact id should abort check");

        let message = error.to_string();
        assert!(
            message.contains("artifact.id 'surprise'")
                && message.contains("expected artifact id 'avoid'"),
            "{message}"
        );
    }

    #[test]
    fn official_service_artifact_identity_must_match_resolved_name() {
        let images = vec![("drive".to_string(), "drive:swapped".to_string())];

        let error = run_check(
            &images,
            &[],
            &[],
            |image_ref| match image_ref {
                "drive:swapped" => Ok(raw("mission", "y2026_1", &[])),
                unexpected => bail!("unexpected image {unexpected}"),
            },
            |_| bail!("no tools should be fetched"),
            |_| bail!("no source runtimes should be built"),
        )
        .expect_err("swapped official service image should abort check");

        let message = error.to_string();
        assert!(
            message.contains("official service emit-apis artifact.id 'mission'")
                && message.contains("expected artifact id 'drive'"),
            "{message}"
        );
    }

    #[test]
    fn tool_artifact_identity_must_match_resolved_tool() {
        let tools = vec![ToolParticipant {
            name: "joypad".to_string(),
            binary_path: PathBuf::from("/fake/cache/joypad"),
        }];

        let error = run_check(
            &[],
            &tools,
            &[],
            |_| bail!("no platform images should be fetched"),
            |path| {
                if path == Path::new("/fake/cache/joypad") {
                    Ok(raw_kind_class(
                        "tool",
                        "simulator_webots_controller",
                        "y2026_1",
                        &[],
                        "privileged",
                    ))
                } else {
                    bail!("unexpected tool path {}", path.display())
                }
            },
            |_| bail!("no source runtimes should be built"),
        )
        .expect_err("swapped tool binary should abort check");

        let message = error.to_string();
        assert!(
            message.contains("tool emit-apis artifact.id 'simulator_webots_controller'")
                && message.contains("expected artifact id 'joypad'"),
            "{message}"
        );
    }

    #[test]
    fn tool_artifact_kind_legacy_runtime_is_tolerated() -> Result<()> {
        let tools = vec![ToolParticipant {
            name: "joypad".to_string(),
            binary_path: PathBuf::from("/fake/cache/joypad"),
        }];

        let outcome = run_check(
            &[],
            &tools,
            &[],
            |_| bail!("no platform images should be fetched"),
            |path| {
                if path == Path::new("/fake/cache/joypad") {
                    Ok(raw_kind_class(
                        "runtime",
                        "joypad",
                        "y2026_1",
                        &[],
                        "privileged",
                    ))
                } else {
                    bail!("unexpected tool path {}", path.display())
                }
            },
            |_| bail!("no source runtimes should be built"),
        )?;

        assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
        Ok(())
    }

    #[test]
    fn component_driver_artifact_kind_legacy_runtime_is_tolerated() -> Result<()> {
        let sources = vec![SourceParticipant::component_driver_with_artifact_id(
            "left_motor",
            "ddsm115",
            PathBuf::from("/fake/project/components/ddsm115"),
        )];

        let outcome = run_check(
            &[],
            &[],
            &sources,
            |_| bail!("no platform images should be fetched"),
            |_| bail!("no tools should be fetched"),
            |_| {
                Ok(raw_kind_class(
                    "runtime",
                    "ddsm115",
                    "y2026_1",
                    &[],
                    "checked",
                ))
            },
        )?;

        assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
        Ok(())
    }

    #[test]
    fn tool_artifact_kind_garbage_is_rejected() {
        let tools = vec![ToolParticipant {
            name: "joypad".to_string(),
            binary_path: PathBuf::from("/fake/cache/joypad"),
        }];

        let error = run_check(
            &[],
            &tools,
            &[],
            |_| bail!("no platform images should be fetched"),
            |path| {
                if path == Path::new("/fake/cache/joypad") {
                    Ok(raw_kind_class(
                        "nonsense",
                        "joypad",
                        "y2026_1",
                        &[],
                        "privileged",
                    ))
                } else {
                    bail!("unexpected tool path {}", path.display())
                }
            },
            |_| bail!("no source runtimes should be built"),
        )
        .expect_err("tool binary reporting a garbage kind should abort check");

        let message = error.to_string();
        assert!(
            message.contains("tool emit-apis artifact.kind 'nonsense'")
                && message.contains("expected kind 'tool'"),
            "{message}"
        );
    }

    #[test]
    fn scoped_runtime_check_only_builds_the_named_runtime() -> Result<()> {
        // `check --runtime other` keeps every source participant in the graph
        // (so topology is still validated) but scopes the expensive BUILD to the
        // named user runtime; every other participant is marked `UseCached`.
        let all_sources = vec![
            SourceParticipant::user_runtime(
                "bad".to_string(),
                PathBuf::from("/fake/project/runtimes/bad"),
            ),
            SourceParticipant::user_runtime(
                "other".to_string(),
                PathBuf::from("/fake/project/runtimes/other"),
            ),
            SourceParticipant::component_driver_with_artifact_id(
                "left_drive".to_string(),
                "ddsm115".to_string(),
                PathBuf::from("/fake/project/components/ddsm115"),
            ),
        ];
        let sources = source_participants_for_runtime(&all_sources, Some("other"));

        assert_eq!(
            sources
                .iter()
                .map(|participant| (participant.name.clone(), participant.build_mode))
                .collect::<Vec<_>>(),
            vec![
                ("bad".to_string(), SourceBuildMode::UseCached),
                ("other".to_string(), SourceBuildMode::Build),
                ("left_drive".to_string(), SourceBuildMode::UseCached),
            ]
        );

        let mut built = Vec::new();
        let outcome = run_check(
            &[],
            &[],
            &sources,
            |_| bail!("no platform images should be fetched"),
            |_| bail!("no tools should be fetched"),
            |participant| {
                let dir = participant.crate_dir.as_path();
                // Model the real builder's contract: `Build` participants run the
                // expensive build; `UseCached` participants only ever read cached
                // metadata and must never trigger a build.
                if participant.build_mode == SourceBuildMode::Build {
                    built.push(dir.to_path_buf());
                }
                if dir == Path::new("/fake/project/runtimes/bad") {
                    Ok(raw("bad", "y2026_1", &[]))
                } else if dir == Path::new("/fake/project/runtimes/other") {
                    Ok(raw("other", "y2026_1", &[]))
                } else if dir == Path::new("/fake/project/components/ddsm115") {
                    Ok(raw_kind("driver", "ddsm115", "y2026_1", &[]))
                } else {
                    bail!(
                        "unrelated source participant should not be built: {}",
                        dir.display()
                    )
                }
            },
        )?;

        assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
        // Only the named runtime crate is actually built.
        assert_eq!(built, vec![PathBuf::from("/fake/project/runtimes/other")]);
        Ok(())
    }

    #[test]
    fn scoped_runtime_check_ignores_unrelated_build_failures_but_keeps_topology() -> Result<()> {
        // `check --runtime other`: an unrelated user runtime that fails to BUILD
        // must NOT fail the scoped check (its metadata comes from cache), yet a
        // topology problem contributed by another participant is still detected.
        let all_sources = vec![
            SourceParticipant::user_runtime(
                "bad".to_string(),
                PathBuf::from("/fake/project/runtimes/bad"),
            ),
            SourceParticipant::user_runtime(
                "other".to_string(),
                PathBuf::from("/fake/project/runtimes/other"),
            ),
        ];
        let sources = source_participants_for_runtime(&all_sources, Some("other"));

        let outcome = run_check(
            &[],
            &[],
            &sources,
            |_| bail!("no platform images should be fetched"),
            |_| bail!("no tools should be fetched"),
            |participant| {
                let dir = participant.crate_dir.as_path();
                match participant.build_mode {
                    // The only participant we actually build is `other`.
                    SourceBuildMode::Build => {
                        assert_eq!(dir, Path::new("/fake/project/runtimes/other"));
                        Ok(raw("other", "y2026_1", &[]))
                    }
                    // Cached metadata for the unrelated `bad` runtime: it would
                    // fail to BUILD, but the scoped check reads cache instead and
                    // still folds its (broken topology) contract into the graph.
                    SourceBuildMode::UseCached => {
                        assert_eq!(dir, Path::new("/fake/project/runtimes/bad"));
                        Ok(raw(
                            "bad",
                            "y2026_1",
                            &[("drive::Target", "drive/target", "subscribe")],
                        ))
                    }
                }
            },
        )?;

        // The unrelated build failure did not abort the check, but `bad`'s
        // unsatisfied consumer (from cached metadata) is still reported.
        assert!(matches!(
            outcome.report.problems.as_slice(),
            [Problem::MissingProducer { consumers, .. }] if consumers == &vec!["bad".to_string()]
        ));
        Ok(())
    }

    #[test]
    fn scoped_runtime_check_detects_component_driver_topology_problems() -> Result<()> {
        let all_sources = vec![
            SourceParticipant::user_runtime(
                "other".to_string(),
                PathBuf::from("/fake/project/runtimes/other"),
            ),
            SourceParticipant::component_driver_with_artifact_id(
                "left_drive".to_string(),
                "ddsm115".to_string(),
                PathBuf::from("/fake/project/components/ddsm115"),
            ),
        ];
        let sources = source_participants_for_runtime(&all_sources, Some("other"));

        let outcome = run_check(
            &[],
            &[],
            &sources,
            |_| bail!("no platform images should be fetched"),
            |_| bail!("no tools should be fetched"),
            |participant| {
                let dir = participant.crate_dir.as_path();
                if dir == Path::new("/fake/project/runtimes/other") {
                    Ok(raw("other", "y2026_1", &[]))
                } else if dir == Path::new("/fake/project/components/ddsm115") {
                    Ok(raw_kind(
                        "driver",
                        "ddsm115",
                        "y2026_1",
                        &[("drive::Target", "drive/target", "subscribe")],
                    ))
                } else {
                    bail!("unexpected source dir {}", dir.display())
                }
            },
        )?;

        assert!(matches!(
            outcome.report.problems.as_slice(),
            // Keyed by the concrete component instance id, not the driver artifact.
            [Problem::MissingProducer { consumers, .. }] if consumers == &vec!["left_drive".to_string()]
        ));
        Ok(())
    }

    #[test]
    fn scoped_runtime_check_detects_other_user_runtime_topology_problems() -> Result<()> {
        let all_sources = vec![
            SourceParticipant::user_runtime(
                "bad".to_string(),
                PathBuf::from("/fake/project/runtimes/bad"),
            ),
            SourceParticipant::user_runtime(
                "other".to_string(),
                PathBuf::from("/fake/project/runtimes/other"),
            ),
        ];
        let sources = source_participants_for_runtime(&all_sources, Some("other"));

        let outcome = run_check(
            &[],
            &[],
            &sources,
            |_| bail!("no platform images should be fetched"),
            |_| bail!("no tools should be fetched"),
            |participant| {
                let dir = participant.crate_dir.as_path();
                if dir == Path::new("/fake/project/runtimes/bad") {
                    Ok(raw(
                        "bad",
                        "y2026_1",
                        &[("drive::Target", "drive/target", "subscribe")],
                    ))
                } else if dir == Path::new("/fake/project/runtimes/other") {
                    Ok(raw("other", "y2026_1", &[]))
                } else {
                    bail!("unexpected source dir {}", dir.display())
                }
            },
        )?;

        assert!(matches!(
            outcome.report.problems.as_slice(),
            [Problem::MissingProducer { consumers, .. }] if consumers == &vec!["bad".to_string()]
        ));
        Ok(())
    }

    #[test]
    fn component_driver_wrong_schema_id_fails_with_mismatch_problem() -> Result<()> {
        // A component driver (subscribing `drive/target`) and a platform publisher
        // report different `schema_id`s for the shared contract. The mismatch is
        // reported per contract, and the driver appears under its concrete instance
        // id (`left_drive`), not the shared driver artifact (`ddsm115`), so multiple
        // instances of one driver stay distinct in the report.
        let images = vec![("mission".to_string(), "mission:ok".to_string())];
        let sources = vec![SourceParticipant::component_driver_with_artifact_id(
            "left_drive".to_string(),
            "ddsm115".to_string(),
            PathBuf::from("/fake/project/components/ddsm115"),
        )];

        let outcome = run_check(
            &images,
            &[],
            &sources,
            |image_ref| match image_ref {
                "mission:ok" => Ok(raw_with_schema(
                    "mission",
                    "y2026_1",
                    &[("drive::Target", "drive/target", "publish", "aaaa")],
                )),
                unexpected => bail!("unexpected image {unexpected}"),
            },
            |_| bail!("no tools should be fetched"),
            |_| {
                Ok(raw_kind_with_schema(
                    "driver",
                    "ddsm115",
                    "y2026_1",
                    &[("drive::Target", "drive/target", "subscribe", "bbbb")],
                    "checked",
                ))
            },
        )?;

        assert_eq!(
            outcome.report.problems,
            vec![Problem::ContractSchemaMismatch {
                family: "drive::Target".to_string(),
                topic: "drive/target".to_string(),
                schema_ids: vec![
                    ("aaaa".to_string(), vec!["mission".to_string()]),
                    ("bbbb".to_string(), vec!["left_drive".to_string()]),
                ],
            }]
        );
        assert!(!outcome.is_ok());
        Ok(())
    }

    #[test]
    fn source_build_error_is_a_hard_error() {
        let sources = vec![SourceParticipant::user_runtime(
            "drive".to_string(),
            PathBuf::from("/fake/project/runtimes/drive"),
        )];

        let error = run_check(
            &[],
            &[],
            &sources,
            |_| bail!("no platform images should be fetched"),
            |_| bail!("no tools should be fetched"),
            |_| Err(MissingImageError::new(anyhow!("source build failed")).into()),
        )
        .expect_err("source build failures should abort check");

        let message = format!("{error:#}");
        assert!(
            message.contains("failed to obtain emit-apis for user runtime drive"),
            "{message}"
        );
        assert!(message.contains("source build failed"), "{message}");
    }

    #[test]
    fn component_driver_build_error_is_a_hard_error() {
        let sources = vec![SourceParticipant::component_driver_with_artifact_id(
            "left_drive".to_string(),
            "ddsm115".to_string(),
            PathBuf::from("/fake/project/components/ddsm115"),
        )];

        let error = run_check(
            &[],
            &[],
            &sources,
            |_| bail!("no platform images should be fetched"),
            |_| bail!("no tools should be fetched"),
            |_| Err(anyhow!("component build failed")),
        )
        .expect_err("component driver build failures should abort check");

        let message = format!("{error:#}");
        assert!(
            message.contains("failed to obtain emit-apis for component driver left_drive"),
            "{message}"
        );
        assert!(message.contains("component build failed"), "{message}");
    }

    #[test]
    fn components_without_drivers_are_not_built() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let resolved = resolved_with_components(vec![
            ResolvedComponent {
                instance: "left_drive".to_string(),
                source_name: "ddsm115".to_string(),
                source: ResolvedComponentSource::Path {
                    path: PathBuf::from("components/ddsm115"),
                },
                has_driver: true,
            },
            ResolvedComponent {
                instance: "caster".to_string(),
                source_name: "passive_caster".to_string(),
                source: ResolvedComponentSource::Path {
                    path: PathBuf::from("components/passive_caster"),
                },
                has_driver: false,
            },
        ])?;
        let mut located = Vec::new();
        let source_participants = source_participants_from_resolved(
            temp.path(),
            &resolved,
            |component, project_root| {
                located.push(component.instance.clone());
                Ok(project_root
                    .join("component-crates")
                    .join(&component.instance))
            },
        )?;

        assert_eq!(located, vec!["left_drive"]);
        assert_eq!(
            source_participants,
            vec![SourceParticipant::component_driver_with_artifact_id(
                "left_drive".to_string(),
                "ddsm115".to_string(),
                temp.path().join("component-crates/left_drive")
            )]
        );

        let mut built = Vec::new();
        let outcome = run_check(
            &[],
            &[],
            &source_participants,
            |_| bail!("no platform images should be fetched"),
            |_| bail!("no tools should be fetched"),
            |participant| {
                let dir = participant.crate_dir.as_path();
                built.push(dir.to_path_buf());
                Ok(raw_kind("driver", "ddsm115", "y2026_1", &[]))
            },
        )?;

        assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
        assert_eq!(built, vec![temp.path().join("component-crates/left_drive")]);
        Ok(())
    }

    #[test]
    fn missing_image_is_reported_after_other_images_are_checked() -> Result<()> {
        let images = vec![
            ("mission".to_string(), "mission:ok".to_string()),
            (
                "drive".to_string(),
                "ghcr.io/phoxal/service-drive:y2026_1-stable".to_string(),
            ),
        ];

        let outcome = run_check(
            &images,
            &[],
            &[],
            |image_ref| match image_ref {
                "mission:ok" => Ok(raw("mission", "y2026_1", &[])),
                "ghcr.io/phoxal/service-drive:y2026_1-stable" => {
                    Err(MissingImageError::new(anyhow!("not found")).into())
                }
                unexpected => bail!("unexpected image {unexpected}"),
            },
            |_| bail!("no tools should be fetched"),
            |_| bail!("no source runtimes should be built"),
        )?;

        assert_eq!(
            outcome.missing_images,
            vec!["ghcr.io/phoxal/service-drive:y2026_1-stable".to_string()]
        );
        assert!(!outcome.is_ok());
        Ok(())
    }

    #[test]
    fn unrecognized_direction_names_artifact() {
        let raw = raw(
            "drive",
            "y2026_1",
            &[("drive::Target", "drive/target", "future_direction")],
        );

        let error =
            graph_check::ParticipantApis::try_from(raw).expect_err("unknown direction should fail");

        assert!(
            error.to_string().contains(
                "unrecognized emit-apis direction 'future_direction' for artifact 'drive'"
            )
        );
    }

    #[test]
    fn raw_emit_apis_accepts_required_contracts_json() -> Result<()> {
        let parsed: RawEmitApis = serde_json::from_str(
            r#"{
                "artifact": { "kind": "service", "id": "drive", "ignored": true },
                "api_version": "y2026_1",
                "bus_abi": "v0",
                "required_contracts": [
                    {
                        "family": "drive::Target",
                        "topic": "drive/target",
                        "direction": "subscribe",
                        "schema_id": "deadbeef",
                        "ignored": true
                    }
                ],
                "config_schema": { "type": "object" }
            }"#,
        )?;
        let participant = graph_check::ParticipantApis::try_from(parsed)?;

        assert_eq!(participant.artifact_id, "drive");
        assert_eq!(participant.participant_class, ParticipantClass::Checked);
        assert_eq!(participant.api_version, "y2026_1");
        assert_eq!(participant.bus_abi.as_deref(), Some("v0"));
        assert_eq!(
            participant
                .config_schema
                .as_ref()
                .and_then(|schema| schema.get("type"))
                .and_then(Value::as_str),
            Some("object")
        );
        assert_eq!(participant.contracts[0].direction, Direction::Subscribe);
        Ok(())
    }

    #[test]
    fn raw_emit_apis_threads_privileged_participant_class() -> Result<()> {
        let parsed: RawEmitApis = serde_json::from_str(
            r#"{
                "artifact": { "kind": "tool", "id": "joypad" },
                "participant_class": "privileged",
                "api_version": "y2026_1",
                "required_contracts": []
            }"#,
        )?;
        let participant = graph_check::ParticipantApis::try_from(parsed)?;

        assert_eq!(participant.participant_class, ParticipantClass::Privileged);
        Ok(())
    }

    #[test]
    fn raw_emit_apis_unknown_participant_class_defaults_to_checked() -> Result<()> {
        let mut raw = raw("drive", "y2026_1", &[]);
        raw.participant_class = "future".to_string();
        let participant = graph_check::ParticipantApis::try_from(raw)?;

        assert_eq!(participant.participant_class, ParticipantClass::Checked);
        Ok(())
    }

    #[test]
    fn multiple_server_responders_format_names_query_topic_and_fix() {
        let message = format_problem(&Problem::MultipleServerResponders {
            family: "asset::GetResponse".to_string(),
            topic: "asset/get".to_string(),
            responders: vec!["asset-alpha".to_string(), "asset-beta".to_string()],
        });

        assert_eq!(
            message,
            "query topic asset::GetResponse (asset/get) has more than one server: asset-alpha, asset-beta; keep exactly one"
        );
    }

    #[test]
    fn user_runtime_config_is_validated_against_emitted_schema() -> Result<()> {
        let sources = vec![SourceParticipant::user_runtime(
            "avoid".to_string(),
            PathBuf::from("/fake/project/runtimes/avoid"),
        )];
        let extras = RobotManifestExtras {
            user_runtimes: BTreeMap::from([(
                "avoid".to_string(),
                crate::resolver::UserRuntimeManifestExtras {
                    image: None,
                    config: Some(serde_json::json!({ "gain": "fast" })),
                },
            )]),
            ..RobotManifestExtras::default()
        };
        let robot_graph = graph_check::RobotGraph::default();

        let outcome = run_check_with_context(
            &[],
            &[],
            &sources,
            CheckGraphContext {
                robot_graph: &robot_graph,
                manifest_extras: &extras,
            },
            |_| bail!("no platform images should be fetched"),
            |_| bail!("no tools should be fetched"),
            |_| {
                let mut raw = raw("avoid", "y2026_1", &[]);
                raw.config_schema = Some(serde_json::json!({
                    "type": "object",
                    "required": ["gain"],
                    "properties": {
                        "gain": { "type": "number" }
                    },
                    "additionalProperties": false
                }));
                Ok(raw)
            },
        )?;

        assert_eq!(outcome.report.problems.len(), 1);
        assert!(matches!(
            &outcome.report.problems[0],
            Problem::InvalidConfig { runtime_id, errors }
                if runtime_id == "avoid"
                    && errors.iter().any(|error| error.contains("gain"))
        ));
        Ok(())
    }

    #[test]
    fn user_runtime_config_uses_full_json_schema_keywords() -> Result<()> {
        let sources = vec![SourceParticipant::user_runtime(
            "avoid".to_string(),
            PathBuf::from("/fake/project/runtimes/avoid"),
        )];
        let extras = RobotManifestExtras {
            user_runtimes: BTreeMap::from([(
                "avoid".to_string(),
                crate::resolver::UserRuntimeManifestExtras {
                    image: None,
                    config: Some(serde_json::json!({
                        "gains": [0.25, 5.5],
                        "mode": "FAST",
                        "extra": true
                    })),
                },
            )]),
            ..RobotManifestExtras::default()
        };
        let robot_graph = graph_check::RobotGraph::default();

        let outcome = run_check_with_context(
            &[],
            &[],
            &sources,
            CheckGraphContext {
                robot_graph: &robot_graph,
                manifest_extras: &extras,
            },
            |_| bail!("no platform images should be fetched"),
            |_| bail!("no tools should be fetched"),
            |_| {
                let mut raw = raw("avoid", "y2026_1", &[]);
                raw.config_schema = Some(serde_json::json!({
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "required": ["gains", "mode"],
                    "properties": {
                        "gains": {
                            "type": "array",
                            "minItems": 2,
                            "maxItems": 2,
                            "items": { "$ref": "#/$defs/gain" }
                        },
                        "mode": {
                            "type": "string",
                            "pattern": "^[a-z]+$"
                        }
                    },
                    "additionalProperties": false,
                    "$defs": {
                        "gain": {
                            "type": "number",
                            "minimum": 0.0,
                            "maximum": 1.0
                        }
                    }
                }));
                Ok(raw)
            },
        )?;

        let [Problem::InvalidConfig { runtime_id, errors }] = outcome.report.problems.as_slice()
        else {
            panic!(
                "expected one InvalidConfig problem, got {:?}",
                outcome.report.problems
            );
        };
        assert_eq!(runtime_id, "avoid");
        assert!(
            errors.iter().any(|error| error.contains("/gains/1")),
            "{errors:?}"
        );
        assert!(
            errors.iter().any(|error| error.contains("/mode")),
            "{errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|error| error.to_ascii_lowercase().contains("additional properties")),
            "{errors:?}"
        );
        Ok(())
    }

    #[test]
    fn docker_emit_apis_classifier_only_treats_manifest_absence_as_missing() {
        let missing = classify_docker_emit_apis_failure(
            "ghcr.io/phoxal/service-drive:y2026_2-stable",
            "",
            "manifest unknown: manifest unknown",
        );
        assert!(matches!(missing, DockerEmitApisFailure::MissingImage(_)));

        let missing_subcommand = classify_docker_emit_apis_failure(
            "ghcr.io/phoxal/service-drive:y2026_1-stable",
            "",
            r#"exec: "emit-apis": executable file not found in $PATH"#,
        );
        assert!(matches!(missing_subcommand, DockerEmitApisFailure::Hard(_)));

        let auth_failure = classify_docker_emit_apis_failure(
            "ghcr.io/phoxal/service-drive:y2026_1-stable",
            "",
            "unauthorized: authentication required",
        );
        assert!(matches!(auth_failure, DockerEmitApisFailure::Hard(_)));
    }

    fn raw(id: &str, api_version: &str, contracts: &[(&str, &str, &str)]) -> RawEmitApis {
        raw_kind("service", id, api_version, contracts)
    }

    /// Like `raw`, but each contract carries an explicit `schema_id` so a test can
    /// force two participants to disagree on a shared `(family, topic)`.
    fn raw_with_schema(
        id: &str,
        api_version: &str,
        contracts: &[(&str, &str, &str, &str)],
    ) -> RawEmitApis {
        raw_kind_with_schema("service", id, api_version, contracts, "checked")
    }

    fn raw_kind_with_schema(
        kind: &str,
        id: &str,
        api_version: &str,
        contracts: &[(&str, &str, &str, &str)],
        participant_class: &str,
    ) -> RawEmitApis {
        RawEmitApis {
            artifact: RawArtifact {
                kind: kind.to_string(),
                id: id.to_string(),
            },
            participant_class: participant_class.to_string(),
            api_version: api_version.to_string(),
            bus_abi: None,
            required_contracts: contracts
                .iter()
                .map(|(family, topic, direction, schema_id)| RawContract {
                    family: (*family).to_string(),
                    topic: (*topic).to_string(),
                    direction: (*direction).to_string(),
                    schema_id: (*schema_id).to_string(),
                })
                .collect(),
            config_schema: None,
        }
    }

    fn raw_kind(
        kind: &str,
        id: &str,
        api_version: &str,
        contracts: &[(&str, &str, &str)],
    ) -> RawEmitApis {
        raw_kind_class(kind, id, api_version, contracts, "checked")
    }

    fn raw_kind_class(
        kind: &str,
        id: &str,
        api_version: &str,
        contracts: &[(&str, &str, &str)],
        participant_class: &str,
    ) -> RawEmitApis {
        RawEmitApis {
            artifact: RawArtifact {
                kind: kind.to_string(),
                id: id.to_string(),
            },
            participant_class: participant_class.to_string(),
            api_version: api_version.to_string(),
            bus_abi: None,
            required_contracts: contracts
                .iter()
                .map(|(family, topic, direction)| RawContract {
                    family: (*family).to_string(),
                    topic: (*topic).to_string(),
                    direction: (*direction).to_string(),
                    // A single default wire-shape id: every contract in these
                    // fixtures shares one id, so a shared (family, topic) agrees
                    // unless a test deliberately overrides it.
                    schema_id: "deadbeef".to_string(),
                })
                .collect(),
            config_schema: None,
        }
    }

    fn resolved_with_components(components: Vec<ResolvedComponent>) -> Result<ResolvedRobot> {
        Ok(ResolvedRobot {
            robot: Robot::parse_from_string(MINIMAL_ROBOT)?,
            api_version: "y2026_1".to_string(),
            channel: Channel::Stable,
            platform_runtimes: Vec::new(),
            user_runtimes: Vec::new(),
            components,
            tools: Vec::new(),
        })
    }

    const MINIMAL_ROBOT: &str = r#"schema: v0
api_version: y2026_1

identity:
  id: testbot
  namespace: test

structure: structure.urdf

phoxal_participants:
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
