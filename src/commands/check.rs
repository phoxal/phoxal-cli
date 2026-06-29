use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use phoxal::model::component::v1::CapabilityRef;
use phoxal::model::robot::v1::KinematicConfig;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::AppContext;
use crate::catalog::CATALOG;
use crate::check as graph_check;
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
    #[arg(long, value_enum, default_value_t = MessageFormat::Human)]
    pub message_format: MessageFormat,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckOptions {
    pub pull: bool,
    pub runtime: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct RawEmitApis {
    pub artifact: RawArtifact,
    pub api_version: String,
    #[serde(default)]
    pub bus_abi: Option<String>,
    #[serde(alias = "contracts")]
    pub required_contracts: Vec<RawContract>,
    #[serde(default)]
    pub config_schema: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RawArtifact {
    pub kind: String,
    pub id: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RawContract {
    pub family: String,
    pub topic: String,
    pub direction: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckOutcome {
    pub missing_images: Vec<String>,
    pub official_runtime_refs: BTreeMap<String, String>,
    pub report: graph_check::Report,
}

#[derive(Debug, Clone, Copy)]
pub struct CheckGraphContext<'a> {
    pub root_api: &'a str,
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

        ensure_check_outcome_ok(&result.api_version, &result.channel, &result.outcome)?;

        for warning in &result.outcome.report.warnings {
            eprintln!("warning: {}", format_warning(warning));
        }
        eprintln!(
            "warning: v0 is pre-stable: artifacts built at different times may not interoperate; pin digests with phoxal-cli deploy build"
        );

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
    let resolved = resolve(
        &robot,
        project_root,
        &CATALOG,
        ResolveOptions {
            locked: false,
            resolve_external_artifacts: false,
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
            root_api: &resolved.api_version,
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
            bail!("user runtime '{runtime_name}' is not defined in user_runtimes");
        }
        bail!(
            "user runtime '{runtime_name}' is not defined in user_runtimes; available: {available}"
        );
    }
    Ok(())
}

fn source_participants_for_runtime(
    source_participants: &[SourceParticipant],
    _runtime_name: Option<&str>,
) -> Vec<SourceParticipant> {
    source_participants.to_vec()
}

pub fn run_check(
    resolved_platform_image_refs: &[(String, String)],
    tool_participants: &[ToolParticipant],
    source_participants: &[SourceParticipant],
    root_api: &str,
    fetch: impl FnMut(&str) -> Result<RawEmitApis>,
    fetch_tool: impl FnMut(&Path) -> Result<RawEmitApis>,
    build: impl FnMut(&Path) -> Result<RawEmitApis>,
) -> Result<CheckOutcome> {
    let robot_graph = graph_check::RobotGraph::default();
    let manifest_extras = RobotManifestExtras::default();
    run_check_with_context(
        resolved_platform_image_refs,
        tool_participants,
        source_participants,
        CheckGraphContext {
            root_api,
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
    build: impl FnMut(&Path) -> Result<RawEmitApis>,
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
    mut build: impl FnMut(&Path) -> Result<RawEmitApis>,
) -> Result<CheckOutcome> {
    let mut missing_images = Vec::new();
    let mut official_runtime_refs = BTreeMap::new();
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
        validate_artifact_identity("official runtime", runtime_name, "runtime", &raw)?;
        let artifact_id = raw.artifact.id.clone();
        let participant = graph_check::ParticipantApis::try_from(raw).with_context(|| {
            format!("failed to interpret emit-apis for runtime {runtime_name} ({image_ref})")
        })?;
        official_runtime_refs.insert(runtime_name.clone(), image_ref.clone());
        official_runtime_refs.insert(format!("runtime-{runtime_name}"), image_ref.clone());
        official_runtime_refs.insert(artifact_id, image_ref.clone());
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
        let raw = build(&participant.crate_dir).with_context(|| {
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

    let mut report = graph_check::check_graph_with_topology(
        &participants,
        context.root_api,
        context.robot_graph,
    );
    report.problems.extend(config_problems);
    Ok(CheckOutcome {
        missing_images,
        official_runtime_refs,
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
        &format!("user_runtimes.{runtime_id}.config"),
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

pub(crate) fn build_emit_apis_from_source(dir: &Path) -> Result<RawEmitApis> {
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

fn validate_runtime_artifact_identity(
    label: &str,
    expected_id: &str,
    raw: &RawEmitApis,
) -> Result<()> {
    validate_artifact_identity(label, expected_id, "runtime", raw)
}

fn validate_source_artifact_identity(
    participant: &SourceParticipant,
    raw: &RawEmitApis,
) -> Result<()> {
    let expected_kind = match participant.kind {
        SourceParticipantKind::UserRuntime => "runtime",
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
    if raw.artifact.kind != expected_kind {
        bail!(
            "{label} emit-apis artifact.kind '{}' does not match expected kind '{}'",
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
                })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(Self {
            artifact_id,
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
        bail!(
            "{}",
            format_report_error(&outcome.report, &outcome.official_runtime_refs)
        );
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
        "\n  - or use phoxal_runtimes.channel: edge if this API version is intentionally experimental",
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

fn format_report_error(
    report: &graph_check::Report,
    official_runtime_refs: &BTreeMap<String, String>,
) -> String {
    let mut message = String::from("robot graph check failed:");
    for problem in &report.problems {
        if let Some(formatted) = format_official_runtime_mismatch(problem, official_runtime_refs) {
            message.push_str("\n\n");
            message.push_str(&formatted);
        } else {
            message.push_str("\n  - ");
            message.push_str(&format_problem(problem));
        }
    }
    message
}

fn format_official_runtime_mismatch(
    problem: &graph_check::Problem,
    official_runtime_refs: &BTreeMap<String, String>,
) -> Option<String> {
    let graph_check::Problem::ApiVersionMismatch {
        artifact_id,
        expected,
        found,
    } = problem
    else {
        return None;
    };
    let selected = official_runtime_refs.get(artifact_id)?;
    Some(format!(
        "official runtime image reports the wrong api_version\n\n{artifact_id}:\n  selected: {selected}\n  expected: {expected}\n  emitted:  {found}"
    ))
}

fn format_problem(problem: &graph_check::Problem) -> String {
    match problem {
        graph_check::Problem::ApiVersionMismatch {
            artifact_id,
            expected,
            found,
        } => {
            format!("participant {artifact_id} reports api_version {found}, expected {expected}")
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
        graph_check::Problem::InvalidConfig { runtime_id, errors } => {
            format!(
                "invalid config for user runtime {runtime_id}: {}",
                errors.join("; ")
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
    use graph_check::{Direction, Problem};
    use phoxal::model::robot::v1::{Channel, Robot};

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
            "y2026_1",
            |image_ref| match image_ref {
                "mission:ok" => Ok(raw(
                    "mission",
                    "y2026_1",
                    &[("drive::Target", "drive/target", "publish")],
                )),
                unexpected => bail!("unexpected image {unexpected}"),
            },
            |_| bail!("no tools should be fetched"),
            |dir| {
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
            "y2026_1",
            |image_ref| match image_ref {
                "mission:ok" => Ok(raw(
                    "mission",
                    "y2026_1",
                    &[("drive::Target", "drive/target", "publish")],
                )),
                unexpected => bail!("unexpected image {unexpected}"),
            },
            |_| bail!("no tools should be fetched"),
            |dir| {
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
    fn tools_are_included_in_graph_check() -> Result<()> {
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
            "y2026_1",
            |_| bail!("no platform images should be fetched"),
            |path| {
                if path == Path::new("/fake/cache/joypad") {
                    Ok(raw_kind(
                        "tool",
                        "joypad",
                        "y2026_1",
                        &[("drive::Target", "drive/target", "publish")],
                    ))
                } else {
                    bail!("unexpected tool path {}", path.display())
                }
            },
            |dir| {
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
                root_api: "y2026_1",
                robot_graph: &robot_graph,
                manifest_extras: &extras,
            },
            |image_ref| {
                fetched_images.push(image_ref.to_string());
                Ok(raw("avoid", "y2026_1", &[]))
            },
            |_| bail!("no tools should be fetched"),
            |dir| {
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
    fn source_wrong_api_version_fails_with_mismatch_problem() -> Result<()> {
        let sources = vec![SourceParticipant::user_runtime(
            "drive".to_string(),
            PathBuf::from("/fake/project/runtimes/drive"),
        )];

        let outcome = run_check(
            &[],
            &[],
            &sources,
            "y2026_1",
            |_| bail!("no platform images should be fetched"),
            |_| bail!("no tools should be fetched"),
            |_| Ok(raw("drive", "y2026_2", &[])),
        )?;

        assert_eq!(
            outcome.report.problems,
            vec![Problem::ApiVersionMismatch {
                artifact_id: "drive".to_string(),
                expected: "y2026_1".to_string(),
                found: "y2026_2".to_string()
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
            "y2026_1",
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
    fn official_runtime_artifact_identity_must_match_resolved_name() {
        let images = vec![("drive".to_string(), "drive:swapped".to_string())];

        let error = run_check(
            &images,
            &[],
            &[],
            "y2026_1",
            |image_ref| match image_ref {
                "drive:swapped" => Ok(raw("mission", "y2026_1", &[])),
                unexpected => bail!("unexpected image {unexpected}"),
            },
            |_| bail!("no tools should be fetched"),
            |_| bail!("no source runtimes should be built"),
        )
        .expect_err("swapped official runtime image should abort check");

        let message = error.to_string();
        assert!(
            message.contains("official runtime emit-apis artifact.id 'mission'")
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
            "y2026_1",
            |_| bail!("no platform images should be fetched"),
            |path| {
                if path == Path::new("/fake/cache/joypad") {
                    Ok(raw_kind(
                        "tool",
                        "simulator_webots_controller",
                        "y2026_1",
                        &[],
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
    fn tool_artifact_kind_must_be_tool() {
        let tools = vec![ToolParticipant {
            name: "joypad".to_string(),
            binary_path: PathBuf::from("/fake/cache/joypad"),
        }];

        let error = run_check(
            &[],
            &tools,
            &[],
            "y2026_1",
            |_| bail!("no platform images should be fetched"),
            |path| {
                if path == Path::new("/fake/cache/joypad") {
                    Ok(raw("joypad", "y2026_1", &[]))
                } else {
                    bail!("unexpected tool path {}", path.display())
                }
            },
            |_| bail!("no source runtimes should be built"),
        )
        .expect_err("tool binary reporting runtime kind should abort check");

        let message = error.to_string();
        assert!(
            message.contains("tool emit-apis artifact.kind 'runtime'")
                && message.contains("expected kind 'tool'"),
            "{message}"
        );
    }

    #[test]
    fn scoped_runtime_check_keeps_all_source_participants() -> Result<()> {
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

        let mut built = Vec::new();
        let outcome = run_check(
            &[],
            &[],
            &sources,
            "y2026_1",
            |_| bail!("no platform images should be fetched"),
            |_| bail!("no tools should be fetched"),
            |dir| {
                built.push(dir.to_path_buf());
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
        assert_eq!(
            built,
            vec![
                PathBuf::from("/fake/project/runtimes/bad"),
                PathBuf::from("/fake/project/runtimes/other"),
                PathBuf::from("/fake/project/components/ddsm115")
            ]
        );
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
            "y2026_1",
            |_| bail!("no platform images should be fetched"),
            |_| bail!("no tools should be fetched"),
            |dir| {
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
            [Problem::MissingProducer { consumers, .. }] if consumers == &vec!["ddsm115".to_string()]
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
            "y2026_1",
            |_| bail!("no platform images should be fetched"),
            |_| bail!("no tools should be fetched"),
            |dir| {
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
    fn component_driver_wrong_api_version_fails_with_mismatch_problem() -> Result<()> {
        let sources = vec![SourceParticipant::component_driver_with_artifact_id(
            "left_drive".to_string(),
            "ddsm115".to_string(),
            PathBuf::from("/fake/project/components/ddsm115"),
        )];

        let outcome = run_check(
            &[],
            &[],
            &sources,
            "y2026_1",
            |_| bail!("no platform images should be fetched"),
            |_| bail!("no tools should be fetched"),
            |_| Ok(raw_kind("driver", "ddsm115", "y2026_2", &[])),
        )?;

        assert_eq!(
            outcome.report.problems,
            vec![Problem::ApiVersionMismatch {
                artifact_id: "ddsm115".to_string(),
                expected: "y2026_1".to_string(),
                found: "y2026_2".to_string()
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
            "y2026_1",
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
            "y2026_1",
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
            "y2026_1",
            |_| bail!("no platform images should be fetched"),
            |_| bail!("no tools should be fetched"),
            |dir| {
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
                "ghcr.io/phoxal/runtime-drive:y2026_1-stable".to_string(),
            ),
        ];

        let outcome = run_check(
            &images,
            &[],
            &[],
            "y2026_1",
            |image_ref| match image_ref {
                "mission:ok" => Ok(raw("mission", "y2026_1", &[])),
                "ghcr.io/phoxal/runtime-drive:y2026_1-stable" => {
                    Err(MissingImageError::new(anyhow!("not found")).into())
                }
                unexpected => bail!("unexpected image {unexpected}"),
            },
            |_| bail!("no tools should be fetched"),
            |_| bail!("no source runtimes should be built"),
        )?;

        assert_eq!(
            outcome.missing_images,
            vec!["ghcr.io/phoxal/runtime-drive:y2026_1-stable".to_string()]
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
                "artifact": { "kind": "runtime", "id": "drive", "ignored": true },
                "api_version": "y2026_1",
                "bus_abi": "v0",
                "required_contracts": [
                    {
                        "family": "drive::Target",
                        "topic": "drive/target",
                        "direction": "subscribe",
                        "ignored": true
                    }
                ],
                "config_schema": { "type": "object" }
            }"#,
        )?;
        let participant = graph_check::ParticipantApis::try_from(parsed)?;

        assert_eq!(participant.artifact_id, "drive");
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
                root_api: "y2026_1",
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
                root_api: "y2026_1",
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
            "ghcr.io/phoxal/runtime-drive:y2026_2-stable",
            "",
            "manifest unknown: manifest unknown",
        );
        assert!(matches!(missing, DockerEmitApisFailure::MissingImage(_)));

        let missing_subcommand = classify_docker_emit_apis_failure(
            "ghcr.io/phoxal/runtime-drive:y2026_1-stable",
            "",
            r#"exec: "emit-apis": executable file not found in $PATH"#,
        );
        assert!(matches!(missing_subcommand, DockerEmitApisFailure::Hard(_)));

        let auth_failure = classify_docker_emit_apis_failure(
            "ghcr.io/phoxal/runtime-drive:y2026_1-stable",
            "",
            "unauthorized: authentication required",
        );
        assert!(matches!(auth_failure, DockerEmitApisFailure::Hard(_)));
    }

    fn raw(id: &str, api_version: &str, contracts: &[(&str, &str, &str)]) -> RawEmitApis {
        raw_kind("runtime", id, api_version, contracts)
    }

    fn raw_kind(
        kind: &str,
        id: &str,
        api_version: &str,
        contracts: &[(&str, &str, &str)],
    ) -> RawEmitApis {
        RawEmitApis {
            artifact: RawArtifact {
                kind: kind.to_string(),
                id: id.to_string(),
            },
            api_version: api_version.to_string(),
            bus_abi: None,
            required_contracts: contracts
                .iter()
                .map(|(family, topic, direction)| RawContract {
                    family: (*family).to_string(),
                    topic: (*topic).to_string(),
                    direction: (*direction).to_string(),
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

phoxal_runtimes:
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
