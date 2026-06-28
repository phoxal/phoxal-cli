use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::AppContext;
use crate::catalog::CATALOG;
use crate::check as graph_check;
use crate::commands::MessageFormat;
use crate::component_driver::component_crate_dir;
use crate::resolver::{
    ResolveOptions, ResolvedComponent, ResolvedRobot, discover_robot_yaml, load_robot, resolve,
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

        if result.channel != "stable" {
            eprintln!(
                "warning: v0 is pre-stable: artifacts built at different times may not interoperate; pin digests with phoxal-cli deploy build"
            );
        }

        let output = CheckOutput {
            status: "ok",
            api_version: result.api_version.clone(),
            channel: result.channel.clone(),
            participant_count: result.participant_count,
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
    let robot = load_robot(&robot_path)?;
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
    crate::tool_provisioning::ensure_tool_binaries(ui, &resolved, tool_names)?;
    let tool_participants = tool_participants_from_resolved(&resolved)?;
    let mut source_participants =
        source_participants_from_resolved(project_root, &resolved, component_crate_dir)?;
    if let Some(runtime_name) = options.runtime.as_deref() {
        filter_to_user_runtime(&resolved, &mut source_participants, runtime_name)?;
    }
    let participant_count =
        platform_refs.len() + tool_participants.len() + source_participants.len();
    let outcome = run_check(
        &platform_refs,
        &tool_participants,
        &source_participants,
        &resolved.api_version,
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
    pub crate_dir: PathBuf,
    pub kind: SourceParticipantKind,
}

impl SourceParticipant {
    #[must_use]
    pub fn user_runtime(name: impl Into<String>, crate_dir: PathBuf) -> Self {
        Self {
            name: name.into(),
            crate_dir,
            kind: SourceParticipantKind::UserRuntime,
        }
    }

    #[must_use]
    pub fn component_driver(name: impl Into<String>, crate_dir: PathBuf) -> Self {
        Self {
            name: name.into(),
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
        participants.push(SourceParticipant::component_driver(
            component.instance.clone(),
            crate_dir,
        ));
    }

    Ok(participants)
}

fn filter_to_user_runtime(
    resolved: &ResolvedRobot,
    participants: &mut Vec<SourceParticipant>,
    runtime_name: &str,
) -> Result<()> {
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
    participants.retain(|participant| {
        participant.kind == SourceParticipantKind::UserRuntime && participant.name == runtime_name
    });
    Ok(())
}

pub fn run_check(
    resolved_platform_image_refs: &[(String, String)],
    tool_participants: &[ToolParticipant],
    source_participants: &[SourceParticipant],
    root_api: &str,
    mut fetch: impl FnMut(&str) -> Result<RawEmitApis>,
    mut fetch_tool: impl FnMut(&Path) -> Result<RawEmitApis>,
    mut build: impl FnMut(&Path) -> Result<RawEmitApis>,
) -> Result<CheckOutcome> {
    let mut missing_images = Vec::new();
    let mut official_runtime_refs = BTreeMap::new();
    let mut participants = Vec::new();

    for (runtime_name, image_ref) in resolved_platform_image_refs {
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
        let artifact_id = raw.artifact.id.clone();
        let participant = graph_check::ParticipantApis::try_from(raw).with_context(|| {
            format!("failed to interpret emit-apis for runtime {runtime_name} ({image_ref})")
        })?;
        official_runtime_refs.insert(runtime_name.clone(), image_ref.clone());
        official_runtime_refs.insert(format!("runtime-{runtime_name}"), image_ref.clone());
        official_runtime_refs.insert(artifact_id, image_ref.clone());
        participants.push(participant);
    }

    for tool in tool_participants {
        let raw = fetch_tool(&tool.binary_path).with_context(|| {
            format!(
                "failed to obtain emit-apis for tool {} ({})",
                tool.name,
                tool.binary_path.display()
            )
        })?;
        let participant = graph_check::ParticipantApis::try_from(raw).with_context(|| {
            format!(
                "failed to interpret emit-apis for tool {} ({})",
                tool.name,
                tool.binary_path.display()
            )
        })?;
        participants.push(participant);
    }

    for participant in source_participants {
        let raw = build(&participant.crate_dir).with_context(|| {
            format!(
                "failed to obtain emit-apis for {} {} ({})",
                participant.kind_label(),
                participant.name,
                participant.crate_dir.display()
            )
        })?;
        let participant = graph_check::ParticipantApis::try_from(raw).with_context(|| {
            format!(
                "failed to interpret emit-apis for {} {} ({})",
                participant.kind_label(),
                participant.name,
                participant.crate_dir.display()
            )
        })?;
        participants.push(participant);
    }

    let report = graph_check::check_graph(&participants, root_api);
    Ok(CheckOutcome {
        missing_images,
        official_runtime_refs,
        report,
    })
}

pub(crate) fn fetch_emit_apis_from_docker(image_ref: &str) -> Result<RawEmitApis> {
    let output = crate::shell::run_stdout("docker", ["run", "--rm", image_ref, "emit-apis"], None)
        .map_err(MissingImageError::new)?;
    serde_json::from_str(&output)
        .with_context(|| format!("docker emit-apis output for {image_ref} was not valid JSON"))
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
        let sources = vec![SourceParticipant::component_driver(
            "left_drive".to_string(),
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
                    Ok(raw(
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
                    Ok(raw(
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
    fn component_driver_wrong_api_version_fails_with_mismatch_problem() -> Result<()> {
        let sources = vec![SourceParticipant::component_driver(
            "left_drive".to_string(),
            PathBuf::from("/fake/project/components/ddsm115"),
        )];

        let outcome = run_check(
            &[],
            &[],
            &sources,
            "y2026_1",
            |_| bail!("no platform images should be fetched"),
            |_| bail!("no tools should be fetched"),
            |_| Ok(raw("ddsm115", "y2026_2", &[])),
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
        let sources = vec![SourceParticipant::component_driver(
            "left_drive".to_string(),
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
            vec![SourceParticipant::component_driver(
                "left_drive".to_string(),
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
                Ok(raw("ddsm115", "y2026_1", &[]))
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
                "artifact": { "id": "drive", "ignored": true },
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

    fn raw(id: &str, api_version: &str, contracts: &[(&str, &str, &str)]) -> RawEmitApis {
        RawEmitApis {
            artifact: RawArtifact { id: id.to_string() },
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
