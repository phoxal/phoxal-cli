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
use crate::catalog::ArtifactKind;
use crate::commands::MessageFormat;
use crate::component_driver::component_crate_dir;
use crate::resolver::{
    ResolveOptions, ResolvedComponent, ResolvedPlatformRuntime, ResolvedRobot, RobotManifestExtras,
    discover_robot_yaml, load_robot_with_extras, resolve, tool_emit_apis_id,
};
use crate::utils::{cargo_binary_name, resolve_project_path};

#[derive(Debug, Args)]
pub struct CheckCmd {
    #[arg(
        long,
        help = "Refresh native artifacts and host tools before running emit-apis."
    )]
    pub pull: bool,
    #[arg(
        long,
        value_name = "NAME",
        help = "Only build/check the named user service crate after resolving the full project."
    )]
    pub service: Option<String>,
    #[arg(
        long,
        value_enum,
        default_value_t = MessageFormat::Human,
        help = "Output format for the check result."
    )]
    pub message_format: MessageFormat,
    #[arg(
        long = "env",
        value_name = "ENV",
        help = "Apply a robot.<env>.yaml overlay before checking (repeatable). Path pins are only legal through overlays."
    )]
    pub env: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckOptions {
    pub pull: bool,
    pub service: Option<String>,
    pub catalog_source: Option<String>,
    pub overlays: Vec<String>,
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
    pub checked_participants: Vec<graph_check::ParticipantApis>,
}

#[derive(Debug, Clone, Copy)]
pub struct CheckGraphContext<'a> {
    pub robot_graph: &'a graph_check::RobotGraph,
    pub manifest_extras: &'a RobotManifestExtras,
}

#[derive(Debug, Clone, Copy)]
pub struct CheckParticipants<'a> {
    pub platform_artifact_refs: &'a [PlatformArtifactRef],
    pub user_service_images: &'a [UserServiceImageParticipant],
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
            service: self.service.clone(),
            catalog_source: app.catalog_source.clone(),
            overlays: self.env.clone(),
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
            "warning: v0 is pre-stable: artifacts built at different times may not interoperate"
        );

        ensure_check_outcome_ok(&result.target_generation, &result.channel, &result.outcome)?;

        let output = CheckOutput {
            status: "ok",
            target_generation: result.target_generation.clone(),
            channel: result.channel.clone(),
            catalog_revision: result.catalog_revision.clone(),
            participant_count: result.participant_count,
            warning_count: result.outcome.report.warnings.len(),
        };
        crate::commands::print_message(
            &output,
            || {
                println!(
                    "ok: {} participants validated against target generation {} (channel {})",
                    result.participant_count, result.target_generation, result.channel
                );
                if let Some(revision) = &result.catalog_revision {
                    println!("catalog revision: {revision}");
                }
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
    target_generation: String,
    channel: String,
    catalog_revision: Option<String>,
    participant_count: usize,
    warning_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CheckRunResult {
    target_generation: String,
    channel: String,
    catalog_revision: Option<String>,
    participant_count: usize,
    outcome: CheckOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformArtifactRef {
    pub name: String,
    pub kind: ArtifactKind,
    pub artifact_ref: String,
}

impl PlatformArtifactRef {
    fn kind_label(&self) -> &'static str {
        match self.kind {
            ArtifactKind::Service => "official service",
            ArtifactKind::Driver => "official driver",
            ArtifactKind::Tool => "official tool",
            ArtifactKind::Simulator => "official simulator",
        }
    }
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
    let loaded = if options.overlays.is_empty() {
        load_robot_with_extras(&robot_path)?
    } else {
        crate::resolver::load_robot_with_extras_and_overlays(&robot_path, &options.overlays)?
    };
    let robot = loaded.robot;
    let manifest_extras = loaded.extras;
    let catalog = crate::catalog::load_catalog(crate::catalog::CatalogLoadOptions {
        refresh: options.pull,
        cli_source: options.catalog_source.clone(),
        robot_source: manifest_extras.catalog_source.as_ref().map(|source| {
            if source.is_absolute() {
                source.clone()
            } else {
                project_root.join(source)
            }
        }),
    })?;
    // `check` resolves live git component refs so component drivers can be
    // located and staged. A path-only / official-only graph needs no component
    // network; a git component pinned to a commit SHA resolves offline; a
    // tag/branch ref is resolved live via `git ls-remote` with an actionable
    // error if the network is unavailable.
    let resolved = resolve(
        &robot,
        project_root,
        catalog.as_ref(),
        ResolveOptions {
            resolve_source_commits: true,
            ..ResolveOptions::default()
        },
    )?;
    let platform_refs = check_artifact_refs_from_resolved(&resolved);
    if options.pull {
        let count = crate::native_artifacts::stage_resolved_artifacts(
            Some(ui),
            &resolved,
            crate::native_artifacts::ProvisioningMode::Refresh,
        )?;
        ui.info(format!(
            "refreshed the artifact catalog and {count} native artifact(s)"
        ));
    }
    ensure_catalog_availability(&resolved)?;
    let tool_participants = tool_participants_from_resolved(&resolved)?;
    let all_source_participants =
        source_participants_from_resolved(project_root, &resolved, component_crate_dir)?;
    if let Some(catalog) = catalog.as_ref() {
        ensure_no_promoted_preview_features(&all_source_participants, catalog)?;
    }
    if let Some(service_name) = options.service.as_deref() {
        ensure_user_service_exists(&resolved, service_name)?;
    }
    let source_participants =
        source_participants_for_service(&all_source_participants, options.service.as_deref());
    let platform_refs = if options.service.is_some() {
        &[][..]
    } else {
        platform_refs.as_slice()
    };
    let participant_count =
        platform_refs.len() + tool_participants.len() + source_participants.len();
    let robot_graph = robot_graph_from_resolved(&resolved);
    let official_by_ref = resolved
        .platform_runtimes
        .iter()
        .map(|runtime| (runtime.artifact_ref().to_string(), runtime))
        .collect::<std::collections::BTreeMap<_, _>>();
    let tools_by_ref = resolved
        .tools
        .iter()
        .map(|tool| (tool.asset.clone(), tool))
        .collect::<std::collections::BTreeMap<_, _>>();
    let outcome = run_check_with_context(
        platform_refs,
        &tool_participants,
        &source_participants,
        CheckGraphContext {
            robot_graph: &robot_graph,
            manifest_extras: &manifest_extras,
        },
        |artifact_ref| {
            if let Some(runtime) = official_by_ref.get(artifact_ref) {
                return fetch_emit_apis_from_native_artifact(runtime);
            }
            if let Some(tool) = tools_by_ref.get(artifact_ref) {
                return fetch_emit_apis_from_native_tool(tool);
            }
            Err(anyhow!(
                "resolved official artifact {artifact_ref} is not in the catalog"
            ))
        },
        fetch_emit_apis_from_tool,
        build_emit_apis_from_source,
    )?;

    Ok(CheckRunResult {
        target_generation: resolved.target_generation,
        channel: resolved.channel.to_string(),
        catalog_revision: resolved.catalog_revision,
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
    /// reuse already-emitted metadata. `check --service <name>` scopes the
    /// build to the named user service (`Build`) while keeping every other
    /// participant in the graph via cached metadata (`UseCached`).
    pub build_mode: SourceBuildMode,
}

impl SourceParticipant {
    #[must_use]
    pub fn user_service(name: impl Into<String>, crate_dir: PathBuf) -> Self {
        let name = name.into();
        Self {
            expected_artifact_id: name.clone(),
            name,
            crate_dir,
            kind: SourceParticipantKind::UserService,
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

    #[must_use]
    pub fn official_service(
        name: impl Into<String>,
        expected_artifact_id: impl Into<String>,
        crate_dir: PathBuf,
    ) -> Self {
        Self {
            name: name.into(),
            expected_artifact_id: expected_artifact_id.into(),
            crate_dir,
            kind: SourceParticipantKind::OfficialService,
            build_mode: SourceBuildMode::Build,
        }
    }

    #[must_use]
    pub fn tool(
        name: impl Into<String>,
        expected_artifact_id: impl Into<String>,
        crate_dir: PathBuf,
    ) -> Self {
        Self {
            name: name.into(),
            expected_artifact_id: expected_artifact_id.into(),
            crate_dir,
            kind: SourceParticipantKind::Tool,
            build_mode: SourceBuildMode::Build,
        }
    }

    #[must_use]
    pub fn simulator(
        name: impl Into<String>,
        expected_artifact_id: impl Into<String>,
        crate_dir: PathBuf,
    ) -> Self {
        Self {
            name: name.into(),
            expected_artifact_id: expected_artifact_id.into(),
            crate_dir,
            kind: SourceParticipantKind::Simulator,
            build_mode: SourceBuildMode::Build,
        }
    }

    pub(crate) fn kind_label(&self) -> &'static str {
        match self.kind {
            SourceParticipantKind::UserService => "user service",
            SourceParticipantKind::OfficialService => "path-overridden official service",
            SourceParticipantKind::ComponentDriver => "component driver",
            SourceParticipantKind::Tool => "path-overridden tool",
            SourceParticipantKind::Simulator => "path-overridden simulator",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceParticipantKind {
    UserService,
    OfficialService,
    ComponentDriver,
    Tool,
    Simulator,
}

/// How `check` obtains a source participant's `emit-apis` metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceBuildMode {
    /// Build the crate and run `emit-apis` on the fresh binary (expensive).
    Build,
    /// Reuse already-emitted metadata instead of rebuilding. Used for the
    /// participants outside a `check --service <name>` build scope so the full
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
pub struct UserServiceImageParticipant {
    pub name: String,
    pub image_ref: String,
}

pub(crate) fn tool_participants_from_resolved(
    resolved: &ResolvedRobot,
) -> Result<Vec<ToolParticipant>> {
    resolved
        .tools
        .iter()
        .filter_map(|tool| {
            if tool.path_override.is_some() {
                None
            } else {
                tool_env_override(tool).map(|path| {
                    Ok(ToolParticipant {
                        name: tool.name.clone(),
                        binary_path: path,
                    })
                })
            }
        })
        .collect()
}

pub(crate) fn platform_artifact_refs_from_resolved(
    resolved: &ResolvedRobot,
) -> Vec<PlatformArtifactRef> {
    resolved
        .platform_runtimes
        .iter()
        .filter(|runtime| runtime.source_path().is_none())
        .map(|runtime| PlatformArtifactRef {
            name: runtime.name.clone(),
            kind: runtime.kind,
            artifact_ref: runtime.artifact_ref().to_string(),
        })
        .collect()
}

fn check_artifact_refs_from_resolved(resolved: &ResolvedRobot) -> Vec<PlatformArtifactRef> {
    let mut refs = platform_artifact_refs_from_resolved(resolved);
    refs.extend(
        resolved
            .tools
            .iter()
            .filter(|tool| tool.path_override.is_none())
            .filter(|tool| tool_env_override(tool).is_none())
            .map(|tool| PlatformArtifactRef {
                name: tool_emit_apis_id(&tool.name).to_string(),
                kind: ArtifactKind::Tool,
                artifact_ref: tool.asset.clone(),
            }),
    );
    refs
}

pub(crate) fn source_participants_from_resolved(
    project_root: &Path,
    resolved: &ResolvedRobot,
    mut locate_component_crate: impl FnMut(&ResolvedComponent, &Path) -> Result<PathBuf>,
) -> Result<Vec<SourceParticipant>> {
    let mut participants = resolved
        .platform_runtimes
        .iter()
        .filter_map(|runtime| {
            runtime.source_path().map(|path| {
                SourceParticipant::official_service(
                    runtime.name.clone(),
                    runtime.name.clone(),
                    path.to_path_buf(),
                )
            })
        })
        .collect::<Vec<_>>();

    participants.extend(resolved.user_runtimes.iter().map(|runtime| {
        SourceParticipant::user_service(
            runtime.name.clone(),
            resolve_project_path(project_root, &runtime.path),
        )
    }));

    for component in resolved
        .components
        .iter()
        .filter(|component| component.has_driver)
    {
        let crate_dir = if let Some(path) = &component.driver_path_override {
            path.clone()
        } else {
            locate_component_crate(component, project_root).with_context(|| {
                format!(
                    "failed to locate component driver {} source",
                    component.instance
                )
            })?
        };
        participants.push(SourceParticipant::component_driver_with_artifact_id(
            component.instance.clone(),
            component.source_name.clone(),
            crate_dir,
        ));
    }

    for tool in resolved.tools.iter().filter_map(|tool| {
        tool.path_override.as_ref().map(|path| {
            SourceParticipant::tool(
                tool.name.clone(),
                tool_emit_apis_id(&tool.name).to_string(),
                path.clone(),
            )
        })
    }) {
        participants.push(tool);
    }

    for simulator in resolved
        .path_overrides
        .iter()
        .filter(|override_| override_.kind == crate::resolver::ResolvedPathOverrideKind::Simulator)
        .map(|override_| {
            SourceParticipant::simulator(
                override_.artifact_name.clone(),
                override_.artifact_name.clone(),
                override_.path.clone(),
            )
        })
    {
        participants.push(simulator);
    }

    Ok(participants)
}

fn ensure_user_service_exists(resolved: &ResolvedRobot, service_name: &str) -> Result<()> {
    if !resolved
        .user_runtimes
        .iter()
        .any(|runtime| runtime.name == service_name)
    {
        let available = resolved
            .user_runtimes
            .iter()
            .map(|runtime| runtime.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        if available.is_empty() {
            bail!("user service '{service_name}' is not defined in user_participants");
        }
        bail!(
            "user service '{service_name}' is not defined in user_participants; available: {available}"
        );
    }
    Ok(())
}

fn ensure_catalog_availability(resolved: &ResolvedRobot) -> Result<()> {
    let unavailable = resolved
        .platform_runtimes
        .iter()
        .filter(|runtime| runtime.source_path().is_none())
        .filter(|runtime| !runtime.changed_contracts.is_empty())
        .filter(|runtime| runtime.target_status != Some(crate::catalog::ArtifactStatus::Released))
        .collect::<Vec<_>>();
    if unavailable.is_empty() {
        return Ok(());
    }

    let mut message = format!(
        "NotYetAvailable: {} is not deployable for target generation {} on {}",
        resolved.robot.identity.id, resolved.target_generation, resolved.target
    );
    if let Some(revision) = &resolved.catalog_revision {
        message.push_str("\n\ncatalog revision: ");
        message.push_str(revision);
    }
    message.push_str("\n\nRequired changed-contract artifacts not released:");
    for runtime in unavailable {
        let status = runtime
            .target_status
            .map(|status| status.to_string())
            .unwrap_or_else(|| "missing".to_string());
        message.push_str("\n  - ");
        message.push_str(&runtime.artifact_id);
        message.push_str(" (");
        message.push_str(&runtime.changed_contracts.join(", "));
        message.push_str(") is ");
        message.push_str(&status);
        message.push_str(" for ");
        message.push_str(&resolved.target);
        if !runtime.per_triple_status.is_empty() {
            let triples = runtime
                .per_triple_status
                .iter()
                .map(|(triple, status)| format!("{triple}: {status}"))
                .collect::<Vec<_>>()
                .join(", ");
            message.push_str("; per-triple status: ");
            message.push_str(&triples);
        }
    }
    message.push_str(
        "\n\nFix: wait for the listed official artifacts to publish, or pin phoxal_artifacts.generation to an older generation whose changed contracts you do not need.",
    );
    bail!("{message}")
}

fn ensure_no_promoted_preview_features(
    participants: &[SourceParticipant],
    catalog: &crate::catalog::CatalogRevision,
) -> Result<()> {
    let mut promoted = Vec::new();
    for participant in participants {
        let manifest_path = participant.crate_dir.join("Cargo.toml");
        if manifest_path.is_file() {
            promoted.extend(crate::catalog::promoted_preview_features(
                &manifest_path,
                catalog,
            )?);
        }
    }
    if promoted.is_empty() {
        return Ok(());
    }

    let mut message = String::from("PromotedGeneration: remove stale preview generation features");
    for feature in promoted {
        message.push_str("\n  - ");
        message.push_str(&feature.manifest_path.display().to_string());
        message.push(':');
        message.push_str(&feature.line_number.to_string());
        message.push_str(" targets promoted generation ");
        message.push_str(&feature.generation);
        message.push_str("; delete this line or remove the `preview-");
        message.push_str(&feature.generation);
        message.push_str("` feature: ");
        message.push_str(&feature.line);
    }
    bail!("{message}")
}

/// Apply a `check --service <name>` build scope to the source participants.
///
/// Every participant stays in the returned set so the full graph is still
/// validated for topology/api consistency. When `service_name` is `Some`, only
/// the named user service is marked `Build`; every other source participant is
/// marked `UseCached` so the expensive build is scoped to the one crate the
/// user asked about, while broken or unrelated crates still contribute their
/// already-emitted metadata to the graph. With no scope, everything builds.
fn source_participants_for_service(
    source_participants: &[SourceParticipant],
    service_name: Option<&str>,
) -> Vec<SourceParticipant> {
    source_participants
        .iter()
        .map(|participant| {
            let mut participant = participant.clone();
            participant.build_mode = match service_name {
                Some(name)
                    if participant.kind == SourceParticipantKind::UserService
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

pub(crate) fn source_participants_building_only_crate(
    source_participants: &[SourceParticipant],
    crate_dir: &Path,
) -> Vec<SourceParticipant> {
    let target = comparable_crate_dir(crate_dir);
    let mut rebuilt_target = false;
    source_participants
        .iter()
        .map(|participant| {
            let mut participant = participant.clone();
            let same_crate = comparable_crate_dir(&participant.crate_dir) == target;
            participant.build_mode = if same_crate && !rebuilt_target {
                rebuilt_target = true;
                SourceBuildMode::Build
            } else {
                SourceBuildMode::UseCached
            };
            participant
        })
        .collect()
}

fn comparable_crate_dir(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
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
    let platform_artifact_refs = service_platform_artifact_refs(resolved_platform_image_refs);
    run_check_with_context(
        &platform_artifact_refs,
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
    resolved_platform_artifact_refs: &[PlatformArtifactRef],
    tool_participants: &[ToolParticipant],
    source_participants: &[SourceParticipant],
    context: CheckGraphContext<'_>,
    fetch: impl FnMut(&str) -> Result<RawEmitApis>,
    fetch_tool: impl FnMut(&Path) -> Result<RawEmitApis>,
    build: impl FnMut(&SourceParticipant) -> Result<RawEmitApis>,
) -> Result<CheckOutcome> {
    run_check_with_deployed_user_service_images(
        CheckParticipants {
            platform_artifact_refs: resolved_platform_artifact_refs,
            user_service_images: &[],
            tool_participants,
            source_participants,
        },
        context,
        fetch,
        fetch_tool,
        build,
    )
}

fn service_platform_artifact_refs(
    resolved_platform_image_refs: &[(String, String)],
) -> Vec<PlatformArtifactRef> {
    resolved_platform_image_refs
        .iter()
        .map(|(name, artifact_ref)| PlatformArtifactRef {
            name: name.clone(),
            kind: ArtifactKind::Service,
            artifact_ref: artifact_ref.clone(),
        })
        .collect()
}

pub fn run_check_with_deployed_user_service_images(
    inputs: CheckParticipants<'_>,
    context: CheckGraphContext<'_>,
    mut fetch: impl FnMut(&str) -> Result<RawEmitApis>,
    mut fetch_tool: impl FnMut(&Path) -> Result<RawEmitApis>,
    mut build: impl FnMut(&SourceParticipant) -> Result<RawEmitApis>,
) -> Result<CheckOutcome> {
    let mut missing_images = Vec::new();
    let mut participants = Vec::new();
    let mut config_problems = Vec::new();

    for artifact in inputs.platform_artifact_refs {
        let image_ref = &artifact.artifact_ref;
        let raw = match fetch(image_ref) {
            Ok(raw) => raw,
            Err(error) if error.downcast_ref::<MissingImageError>().is_some() => {
                missing_images.push(image_ref.clone());
                continue;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to obtain emit-apis for {} {} ({image_ref})",
                        artifact.kind_label(),
                        artifact.name
                    )
                });
            }
        };
        validate_artifact_identity(
            artifact.kind_label(),
            &artifact.name,
            artifact.kind.emit_apis_kind(),
            &raw,
        )?;
        let participant = graph_check::ParticipantApis::try_from(raw).with_context(|| {
            format!(
                "failed to interpret emit-apis for {} {} ({image_ref})",
                artifact.kind_label(),
                artifact.name
            )
        })?;
        participants.push(participant);
    }

    for service in inputs.user_service_images {
        let raw = fetch(&service.image_ref).with_context(|| {
            format!(
                "failed to obtain emit-apis for user service {} ({})",
                service.name, service.image_ref
            )
        })?;
        validate_service_artifact_identity("user service", &service.name, &raw)?;
        let participant = graph_check::ParticipantApis::try_from(raw).with_context(|| {
            format!(
                "failed to interpret emit-apis for user service {} ({})",
                service.name, service.image_ref
            )
        })?;
        if let Some(problem) = validate_user_service_config(
            &service.name,
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
        let expected_id = tool_emit_apis_id(&tool.name);
        validate_artifact_identity("tool", expected_id, "tool", &raw)?;
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
        } else if participant.kind == SourceParticipantKind::UserService
            && let Some(problem) = validate_user_service_config(
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
        checked_participants: participants,
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

fn validate_user_service_config(
    service_id: &str,
    schema: Option<&Value>,
    manifest_extras: &RobotManifestExtras,
) -> Option<graph_check::Problem> {
    let schema = schema?;
    // An absent manifest config is `null`, not `{}`: a no-config service's
    // emitted schema requires null (so absent passes), while a service with a
    // real object schema still fails correctly as config-required-but-missing.
    let config = manifest_extras
        .user_runtime_config(service_id)
        .cloned()
        .unwrap_or(Value::Null);
    let errors = validate_json_schema(
        schema,
        &config,
        &format!("user_participants.{service_id}.config"),
    );
    if errors.is_empty() {
        None
    } else {
        Some(graph_check::Problem::InvalidConfig {
            runtime_id: service_id.to_string(),
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

pub(crate) fn fetch_emit_apis_from_native_artifact(
    runtime: &ResolvedPlatformRuntime,
) -> Result<RawEmitApis> {
    let descriptor = crate::native_artifacts::NativeArtifactDescriptor::from_runtime(runtime)?
        .ok_or_else(|| {
            anyhow!(
                "{} {} has no packaged emit-apis metadata for {}",
                runtime.kind,
                runtime.name,
                runtime.artifact_ref()
            )
        })?;
    let json = crate::native_artifacts::read_packaged_emit_apis(&descriptor)?;
    serde_json::from_str(&json).with_context(|| {
        format!(
            "packaged emit-apis for {} {} ({}) was not valid JSON",
            runtime.kind,
            runtime.name,
            runtime.artifact_ref()
        )
    })
}

pub(crate) fn fetch_emit_apis_from_native_tool(
    tool: &crate::resolver::ResolvedTool,
) -> Result<RawEmitApis> {
    let descriptor = crate::native_artifacts::NativeArtifactDescriptor::from_tool(tool)?
        .ok_or_else(|| anyhow!("tool {} has no packaged emit-apis metadata", tool.name))?;
    let json = crate::native_artifacts::read_packaged_emit_apis(&descriptor)?;
    serde_json::from_str(&json).with_context(|| {
        format!(
            "packaged emit-apis for tool {} ({}) was not valid JSON",
            tool.name, tool.asset
        )
    })
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

fn tool_env_override(tool: &crate::resolver::ResolvedTool) -> Option<PathBuf> {
    env_path_override("PHOXAL_ARTIFACT", &tool.name)
        .or_else(|| env_path_override("PHOXAL_TOOL", &tool.name))
        .or_else(|| {
            std::env::var_os("PHOXAL_ARTIFACT_DIR")
                .map(PathBuf::from)
                .map(|dir| dir.join(&tool.binary_name))
                .filter(|path| path.is_file())
        })
        .or_else(|| {
            std::env::var_os("PHOXAL_TOOL_DIR")
                .map(PathBuf::from)
                .and_then(|dir| {
                    [tool.name.as_str(), tool.binary_name.as_str()]
                        .into_iter()
                        .map(|name| dir.join(name))
                        .find(|path| path.is_file())
                })
        })
}

fn env_path_override(prefix: &str, id: &str) -> Option<PathBuf> {
    let key = format!("{prefix}_{}_PATH", env_key(id));
    std::env::var_os(key)
        .map(PathBuf::from)
        .filter(|path| path.is_file())
}

fn env_key(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

pub(crate) fn build_emit_apis_from_source(participant: &SourceParticipant) -> Result<RawEmitApis> {
    match participant.build_mode {
        SourceBuildMode::Build => {
            let raw = build_emit_apis_by_building(&participant.crate_dir)?;
            // Cache the freshly-emitted metadata keyed by the source tree so a
            // later `check --service <other>` can reuse it without rebuilding.
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
                    "no cached emit-apis for {} {} ({}); run `phoxal check` once (no --service) \
                     to build every participant and populate the cache, then re-run \
                     `phoxal check --service <name>`",
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
    // that is a workspace member (e.g. a phoxal/framework `component/<name>` driver)
    // compiles into the *workspace-root* `target/`, not `<crate_dir>/target/`, so a fixed
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
/// `check --service <name>` reads this for the participants it does not rebuild.
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

fn validate_service_artifact_identity(
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
        SourceParticipantKind::UserService | SourceParticipantKind::OfficialService => "service",
        SourceParticipantKind::ComponentDriver => "driver",
        SourceParticipantKind::Tool => "tool",
        SourceParticipantKind::Simulator => "simulator",
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
            "{label} emit-apis artifact.kind '{}' does not match the expected kind '{}'",
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
        let participant_kind = graph_check::ParticipantKind::parse(&raw.artifact.kind);
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
            participant_kind,
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
    target_generation: &str,
    channel: &str,
    missing_images: &[String],
) -> String {
    let mut message =
        format!("target generation {target_generation} is not available on channel {channel}");
    message.push_str("\n\nMissing official artifacts:");
    for image_ref in missing_images {
        message.push_str("\n  - ");
        message.push_str(image_ref);
    }
    message.push_str("\n\nFix:");
    message.push_str("\n  - refresh or override the generated artifact catalog with `phoxal-cli --catalog <path> check`");
    message.push_str("\n  - or wait until Phoxal publishes the complete ");
    message.push_str(target_generation);
    message.push('-');
    message.push_str(channel);
    message.push_str(" official artifact set");
    message
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
        graph_check::Problem::SubstitutionNotAllowed {
            mode,
            component_instance,
            provider_participant_id,
        } => {
            format!(
                "substitution for component {component_instance} by {provider_participant_id} is not allowed in {} plans; substitutions are sim-only",
                mode.as_str()
            )
        }
        graph_check::Problem::SubstitutionProviderMissing {
            component_instance,
            provider_participant_id,
        } => {
            format!(
                "NoSimulatorProvider: sim plan substitutes component {component_instance} with {provider_participant_id}, but no checked simulator-kind participant is present in the plan. Add phoxal-simulator-webots from the artifact catalog, or add a simulator-webots path override for local simulator development."
            )
        }
        graph_check::Problem::SubstitutionProviderWrongKind {
            component_instance,
            provider_participant_id,
            provider_kind,
        } => {
            format!(
                "substitution provider {provider_participant_id} for component {component_instance} has kind '{}', but substitutions require artifact.kind 'simulator'",
                provider_kind.as_str()
            )
        }
        graph_check::Problem::SubstitutionProviderNotChecked {
            component_instance,
            provider_participant_id,
        } => {
            format!(
                "substitution provider {provider_participant_id} for component {component_instance} is privileged and cannot satisfy checked topology"
            )
        }
        graph_check::Problem::IncompleteSubstitution {
            component_instance,
            provider_participant_id,
            missing_contracts,
        } => {
            let missing = missing_contracts
                .iter()
                .map(format_contract)
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "simulator substitution for component {component_instance} by {provider_participant_id} is incomplete; missing contracts: {missing}"
            )
        }
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
                "invalid config for user service {runtime_id}: {}",
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

fn format_contract(contract: &graph_check::Contract) -> String {
    format!(
        "{} ({}, {}, schema_id {})",
        contract.family,
        contract.topic,
        format_direction(contract.direction),
        contract.schema_id
    )
}

pub(crate) const fn format_direction(direction: graph_check::Direction) -> &'static str {
    match direction {
        graph_check::Direction::Publish => "publish",
        graph_check::Direction::Subscribe => "subscribe",
        graph_check::Direction::QueryRequest => "query_request",
        graph_check::Direction::QueryResponse => "query_response",
        graph_check::Direction::ServerRequest => "server_request",
        graph_check::Direction::ServerResponse => "server_response",
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
        formatter.write_str("official artifact could not be obtained")
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
        let sources = vec![SourceParticipant::user_service(
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
        let sources = vec![SourceParticipant::user_service(
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
            |_| bail!("no source services should be built"),
        )?;

        assert!(outcome.report.problems.is_empty());
        assert!(outcome.report.warnings.is_empty());
        Ok(())
    }

    #[test]
    fn deployed_user_service_images_are_checked_from_image_refs() -> Result<()> {
        let user_images = vec![UserServiceImageParticipant {
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
        let outcome = run_check_with_deployed_user_service_images(
            CheckParticipants {
                platform_artifact_refs: &[],
                user_service_images: &user_images,
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
        let sources = vec![SourceParticipant::user_service(
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
    fn user_service_artifact_id_must_match_manifest_key() {
        let sources = vec![SourceParticipant::user_service(
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
        .expect_err("mismatched user service artifact id should abort check");

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
            |_| bail!("no source services should be built"),
        )
        .expect_err("swapped official service artifact should abort check");

        let message = error.to_string();
        assert!(
            message.contains("official service emit-apis artifact.id 'mission'")
                && message.contains("expected artifact id 'drive'"),
            "{message}"
        );
    }

    #[test]
    fn official_driver_artifact_identity_uses_driver_label() {
        let artifacts = vec![PlatformArtifactRef {
            name: "bno085".to_string(),
            kind: ArtifactKind::Driver,
            artifact_ref: "driver-bno085:swapped".to_string(),
        }];
        let robot_graph = graph_check::RobotGraph::default();
        let extras = RobotManifestExtras::default();

        let error = run_check_with_context(
            &artifacts,
            &[],
            &[],
            CheckGraphContext {
                robot_graph: &robot_graph,
                manifest_extras: &extras,
            },
            |artifact_ref| match artifact_ref {
                "driver-bno085:swapped" => Ok(raw_kind("service", "bno085", "y2026_1", &[])),
                unexpected => bail!("unexpected artifact {unexpected}"),
            },
            |_| bail!("no tools should be fetched"),
            |_| bail!("no source services should be built"),
        )
        .expect_err("wrong official driver kind should abort check");

        let message = error.to_string();
        assert!(
            message.contains("official driver emit-apis artifact.kind 'service'")
                && message.contains("expected kind 'driver'"),
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
            |_| bail!("no source services should be built"),
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
    fn tool_artifact_kind_true_kind_is_accepted() -> Result<()> {
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
                        &[],
                        "privileged",
                    ))
                } else {
                    bail!("unexpected tool path {}", path.display())
                }
            },
            |_| bail!("no source services should be built"),
        )?;

        assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
        Ok(())
    }

    #[test]
    fn tool_artifact_kind_legacy_runtime_is_rejected() {
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
            |_| bail!("no source services should be built"),
        )
        .expect_err("tool binary reporting legacy runtime kind should abort check");

        let message = error.to_string();
        assert!(
            message.contains(
                "tool emit-apis artifact.kind 'runtime' does not match the expected kind 'tool'"
            ),
            "{message}"
        );
    }

    #[test]
    fn component_driver_artifact_kind_true_kind_is_accepted() -> Result<()> {
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
                    "driver",
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
    fn component_driver_artifact_kind_legacy_runtime_is_rejected() {
        let sources = vec![SourceParticipant::component_driver_with_artifact_id(
            "left_motor",
            "ddsm115",
            PathBuf::from("/fake/project/components/ddsm115"),
        )];

        let error = run_check(
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
        )
        .expect_err("component driver reporting legacy runtime kind should abort check");

        let message = error.to_string();
        assert!(
            message.contains(
                "component driver emit-apis artifact.kind 'runtime' does not match the expected kind 'driver'"
            ),
            "{message}"
        );
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
            |_| bail!("no source services should be built"),
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
    fn scoped_service_check_only_builds_the_named_service() -> Result<()> {
        // `check --service other` keeps every source participant in the graph
        // (so topology is still validated) but scopes the expensive BUILD to the
        // named user service; every other participant is marked `UseCached`.
        let all_sources = vec![
            SourceParticipant::user_service(
                "bad".to_string(),
                PathBuf::from("/fake/project/runtimes/bad"),
            ),
            SourceParticipant::user_service(
                "other".to_string(),
                PathBuf::from("/fake/project/runtimes/other"),
            ),
            SourceParticipant::component_driver_with_artifact_id(
                "left_drive".to_string(),
                "ddsm115".to_string(),
                PathBuf::from("/fake/project/components/ddsm115"),
            ),
        ];
        let sources = source_participants_for_service(&all_sources, Some("other"));

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
        // Only the named service crate is actually built.
        assert_eq!(built, vec![PathBuf::from("/fake/project/runtimes/other")]);
        Ok(())
    }

    #[test]
    fn scoped_service_check_ignores_unrelated_build_failures_but_keeps_topology() -> Result<()> {
        // `check --service other`: an unrelated user service that fails to BUILD
        // must NOT fail the scoped check (its metadata comes from cache), yet a
        // topology problem contributed by another participant is still detected.
        let all_sources = vec![
            SourceParticipant::user_service(
                "bad".to_string(),
                PathBuf::from("/fake/project/runtimes/bad"),
            ),
            SourceParticipant::user_service(
                "other".to_string(),
                PathBuf::from("/fake/project/runtimes/other"),
            ),
        ];
        let sources = source_participants_for_service(&all_sources, Some("other"));

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
                    // Cached metadata for the unrelated `bad` service: it would
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
    fn scoped_service_check_detects_component_driver_topology_problems() -> Result<()> {
        let all_sources = vec![
            SourceParticipant::user_service(
                "other".to_string(),
                PathBuf::from("/fake/project/runtimes/other"),
            ),
            SourceParticipant::component_driver_with_artifact_id(
                "left_drive".to_string(),
                "ddsm115".to_string(),
                PathBuf::from("/fake/project/components/ddsm115"),
            ),
        ];
        let sources = source_participants_for_service(&all_sources, Some("other"));

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
    fn scoped_service_check_detects_other_user_service_topology_problems() -> Result<()> {
        let all_sources = vec![
            SourceParticipant::user_service(
                "bad".to_string(),
                PathBuf::from("/fake/project/runtimes/bad"),
            ),
            SourceParticipant::user_service(
                "other".to_string(),
                PathBuf::from("/fake/project/runtimes/other"),
            ),
        ];
        let sources = source_participants_for_service(&all_sources, Some("other"));

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
        let sources = vec![SourceParticipant::user_service(
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
            message.contains("failed to obtain emit-apis for user service drive"),
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
                driver_path_override: None,
            },
            ResolvedComponent {
                instance: "caster".to_string(),
                source_name: "passive_caster".to_string(),
                source: ResolvedComponentSource::Path {
                    path: PathBuf::from("components/passive_caster"),
                },
                has_driver: false,
                driver_path_override: None,
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
    fn path_overridden_service_enters_check_through_source_emit_apis() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut resolved = resolved_with_components(Vec::new())?;
        resolved.platform_runtimes.push(ResolvedPlatformRuntime {
            name: "drive".to_string(),
            artifact_id: "service-drive".to_string(),
            kind: crate::catalog::ArtifactKind::Service,
            generation: "y2026_1".to_string(),
            version: "0.1.0".to_string(),
            artifact_ref: "path:framework/service/drive".to_string(),
            sha256: None,
            metadata: None,
            target_status: Some(crate::catalog::ArtifactStatus::Released),
            per_triple_status: BTreeMap::new(),
            changed_contracts: Vec::new(),
            contract_uses: Vec::new(),
            path_override: Some(temp.path().join("framework/service/drive")),
        });

        let platform_refs = platform_artifact_refs_from_resolved(&resolved);
        assert!(platform_refs.is_empty());

        let source_participants =
            source_participants_from_resolved(temp.path(), &resolved, |_component, _root| {
                bail!("no components in this fixture")
            })?;
        assert_eq!(
            source_participants,
            vec![SourceParticipant::official_service(
                "drive",
                "drive",
                temp.path().join("framework/service/drive"),
            )]
        );

        let robot_graph = robot_graph_from_resolved(&resolved);
        let extras = RobotManifestExtras::default();
        let outcome = run_check_with_context(
            &platform_refs,
            &[],
            &source_participants,
            CheckGraphContext {
                robot_graph: &robot_graph,
                manifest_extras: &extras,
            },
            |_| bail!("path-overridden service should not read catalog metadata"),
            |_| bail!("no tools in this fixture"),
            |participant| {
                assert_eq!(participant.kind, SourceParticipantKind::OfficialService);
                Ok(raw_kind("service", "drive", "y2026_1", &[]))
            },
        )?;
        assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
        Ok(())
    }

    #[test]
    fn missing_image_is_reported_after_other_images_are_checked() -> Result<()> {
        let images = vec![
            ("mission".to_string(), "mission:ok".to_string()),
            (
                "drive".to_string(),
                "service-drive:y2026_1-stable".to_string(),
            ),
        ];

        let outcome = run_check(
            &images,
            &[],
            &[],
            |image_ref| match image_ref {
                "mission:ok" => Ok(raw("mission", "y2026_1", &[])),
                "service-drive:y2026_1-stable" => {
                    Err(MissingImageError::new(anyhow!("not found")).into())
                }
                unexpected => bail!("unexpected image {unexpected}"),
            },
            |_| bail!("no tools should be fetched"),
            |_| bail!("no source services should be built"),
        )?;

        assert_eq!(
            outcome.missing_images,
            vec!["service-drive:y2026_1-stable".to_string()]
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
    fn user_service_config_is_validated_against_emitted_schema() -> Result<()> {
        let sources = vec![SourceParticipant::user_service(
            "avoid".to_string(),
            PathBuf::from("/fake/project/runtimes/avoid"),
        )];
        let extras = RobotManifestExtras {
            user_runtimes: BTreeMap::from([(
                "avoid".to_string(),
                crate::resolver::UserRuntimeManifestExtras {
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
    fn absent_user_service_config_validates_as_null() -> Result<()> {
        let sources = vec![SourceParticipant::user_service(
            "optional".to_string(),
            PathBuf::from("/fake/project/runtimes/optional"),
        )];
        let extras = RobotManifestExtras::default();
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
                let mut raw = raw("optional", "y2026_1", &[]);
                raw.config_schema = Some(serde_json::json!({ "type": "null" }));
                Ok(raw)
            },
        )?;

        assert!(
            outcome
                .report
                .problems
                .iter()
                .all(|problem| !matches!(problem, Problem::InvalidConfig { .. })),
            "{:?}",
            outcome.report.problems
        );
        Ok(())
    }

    #[test]
    fn absent_user_service_config_still_fails_required_object_schema() -> Result<()> {
        let sources = vec![SourceParticipant::user_service(
            "required".to_string(),
            PathBuf::from("/fake/project/runtimes/required"),
        )];
        let extras = RobotManifestExtras::default();
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
                let mut raw = raw("required", "y2026_1", &[]);
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

        assert!(matches!(
            outcome
                .report
                .problems
                .iter()
                .find(|problem| matches!(problem, Problem::InvalidConfig { .. })),
            Some(Problem::InvalidConfig { runtime_id, errors })
                if runtime_id == "required"
                    && errors.iter().any(|error| error.contains("null"))
        ));
        Ok(())
    }

    #[test]
    fn user_service_config_uses_full_json_schema_keywords() -> Result<()> {
        let sources = vec![SourceParticipant::user_service(
            "avoid".to_string(),
            PathBuf::from("/fake/project/runtimes/avoid"),
        )];
        let extras = RobotManifestExtras {
            user_runtimes: BTreeMap::from([(
                "avoid".to_string(),
                crate::resolver::UserRuntimeManifestExtras {
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
            target_generation: "y2026_1".to_string(),
            channel: Channel::Stable,
            target: crate::resolver::host_target_triple(),
            catalog_revision: None,
            platform_runtimes: Vec::new(),
            simulators: Vec::new(),
            user_runtimes: Vec::new(),
            components,
            tools: Vec::new(),
            path_overrides: Vec::new(),
        })
    }

    const MINIMAL_ROBOT: &str = r#"schema: v0
api_version: y2026_1

identity:
  id: testbot
  namespace: test

structure: structure.urdf

phoxal_artifacts:
  channel: stable
phoxal_participants: {}

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
