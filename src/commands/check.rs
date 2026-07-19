use std::{
    fmt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use phoxal::check as graph_check;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::AppContext;
use crate::catalog::ArtifactKind;
use crate::commands::MessageFormat;
use crate::component_driver::component_driver_crate_dir;
use crate::resolver::{
    ResolveOptions, ResolvedComponent, ResolvedComponentSource, ResolvedPlatformRuntime,
    ResolvedRobot, RobotManifestExtras, discover_robot_yaml, load_robot_with_extras, resolve,
    tool_emit_apis_id,
};
use crate::utils::{cargo_binary_name, resolve_project_path};

#[derive(Debug, Args)]
pub struct CheckCmd {
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
    #[arg(
        long,
        value_name = "TRIPLE",
        help = "Resolve official artifacts for this target instead of the host (e.g. aarch64, x86_64, or a full triple). Use it to validate a Linux robot from a non-Linux host."
    )]
    pub target: Option<String>,
    #[arg(long, help = "Promote coherence warnings to a failing check result.")]
    pub strict: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckOptions {
    pub service: Option<String>,
    pub catalog_source: Option<String>,
    pub overlays: Vec<String>,
    pub target: Option<String>,
    pub emit_update_notice: bool,
    pub strict: bool,
}

/// The CLI's own participant-report shape: known artifact identity (never
/// self-reported anymore - a built binary's linker section carries only its
/// contracts, see [`crate::participant_metadata`]) plus the extracted
/// contract list. No `bus_abi` (D1, X-tools slice: dissolved into the
/// version-qualified contract key, `phoxal::check::ParticipantApis` no
/// longer carries it either).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct RawEmitApis {
    pub artifact: RawArtifact,
    #[serde(default = "default_participant_class")]
    pub participant_class: String,
    pub api_version: String,
    pub required_contracts: Vec<crate::participant_metadata::ParticipantMetaContract>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckOutcome {
    pub missing_images: Vec<String>,
    pub report: graph_check::Report,
    pub checked_participants: Vec<graph_check::ParticipantApis>,
    pub contract_surfaces: Vec<graph_check::ParticipantContractSurface>,
}

#[derive(Debug, Clone, Copy)]
pub struct CheckGraphContext<'a> {
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RobotCoherenceDiagnostic {
    pub robot_id: String,
    pub mismatches: Vec<CoherenceMismatchDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RobotContractSurfaces {
    pub robot_id: String,
    pub surfaces: Vec<graph_check::ParticipantContractSurface>,
}

pub(crate) fn robot_contract_surfaces(
    robot_id: &str,
    surfaces: &[graph_check::ParticipantContractSurface],
) -> RobotContractSurfaces {
    RobotContractSurfaces {
        robot_id: robot_id.to_string(),
        surfaces: surfaces.to_vec(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CoherenceMismatchDiagnostic {
    PubSubDisjoint {
        participant_id: String,
        contract: String,
        subscribed: Vec<String>,
        published: Vec<String>,
        remedy: &'static str,
    },
    UnservedAsk {
        participant_id: String,
        contract: String,
        version: String,
        served: Vec<String>,
        remedy: &'static str,
    },
}

const COHERENCE_REMEDY: &str = "align the version, mark a genuinely external consumer edge #[phoxal(external)], or update the lagging artifact";

impl CoherenceMismatchDiagnostic {
    fn from_mismatch(mismatch: &graph_check::CoherenceMismatch) -> Self {
        match mismatch {
            graph_check::CoherenceMismatch::PubSubDisjoint {
                participant_id,
                contract,
                subscribed,
                published,
            } => Self::PubSubDisjoint {
                participant_id: participant_id.clone(),
                contract: contract.clone(),
                subscribed: subscribed.iter().cloned().collect(),
                published: published.iter().cloned().collect(),
                remedy: COHERENCE_REMEDY,
            },
            graph_check::CoherenceMismatch::UnservedAsk {
                participant_id,
                contract,
                version,
                served,
            } => Self::UnservedAsk {
                participant_id: participant_id.clone(),
                contract: contract.clone(),
                version: version.clone(),
                served: served.iter().cloned().collect(),
                remedy: COHERENCE_REMEDY,
            },
        }
    }

    fn human_line(&self) -> String {
        match self {
            Self::PubSubDisjoint {
                participant_id,
                contract,
                subscribed,
                published,
                remedy,
            } => format!(
                "participant {participant_id} subscribes to {contract} at [{}], but the in-set publishers use [{}]; remedy: {remedy}",
                subscribed.join(", "),
                published.join(", ")
            ),
            Self::UnservedAsk {
                participant_id,
                contract,
                version,
                served,
                remedy,
            } => {
                let served = if served.is_empty() {
                    "none".to_string()
                } else {
                    served.join(", ")
                };
                format!(
                    "participant {participant_id} asks {contract} at {version}, but the in-set servers provide [{served}]; remedy: {remedy}"
                )
            }
        }
    }
}

pub(crate) fn evaluate_robot_coherence(
    robot_id: &str,
    surfaces: &[graph_check::ParticipantContractSurface],
) -> RobotCoherenceDiagnostic {
    let report = graph_check::check_coherence(surfaces);
    RobotCoherenceDiagnostic {
        robot_id: robot_id.to_string(),
        mismatches: report
            .mismatches
            .iter()
            .map(CoherenceMismatchDiagnostic::from_mismatch)
            .collect(),
    }
}

pub(crate) fn coherence_for_launch_plan(
    plan: &crate::launch_plan::LaunchPlan,
    graphs: &[RobotContractSurfaces],
) -> Result<Vec<RobotCoherenceDiagnostic>> {
    plan.robots
        .iter()
        .map(|robot| {
            let graph = graphs
                .iter()
                .find(|graph| graph.robot_id == robot.id)
                .ok_or_else(|| anyhow!("robot {} has no checked contract graph", robot.id))?;
            let mut ids = robot
                .participants
                .iter()
                .map(|participant| participant.launch.participant_id.as_str())
                .collect::<std::collections::BTreeSet<_>>();
            ids.extend(plan.site.iter().map(|site| site.id.as_str()));
            let graph_surfaces = graph
                .surfaces
                .iter()
                .filter(|surface| ids.contains(surface.participant_id.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            Ok(evaluate_robot_coherence(&robot.id, &graph_surfaces))
        })
        .collect()
}

fn coherence_is_ok(diagnostics: &[RobotCoherenceDiagnostic]) -> bool {
    diagnostics
        .iter()
        .all(|diagnostic| diagnostic.mismatches.is_empty())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CoherenceVerb {
    Check,
    Deploy,
    Run,
    Simulate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CoherenceDisposition {
    Pass,
    Warning,
    Failure,
}

pub(crate) fn coherence_disposition(
    verb: CoherenceVerb,
    strict: bool,
    diagnostics: &[RobotCoherenceDiagnostic],
) -> CoherenceDisposition {
    if coherence_is_ok(diagnostics) {
        CoherenceDisposition::Pass
    } else if verb == CoherenceVerb::Check && !strict {
        CoherenceDisposition::Warning
    } else {
        CoherenceDisposition::Failure
    }
}

fn format_coherence_error(diagnostics: &[RobotCoherenceDiagnostic]) -> String {
    let mut message = String::from("participant contract coherence check failed:");
    for diagnostic in diagnostics
        .iter()
        .filter(|diagnostic| !diagnostic.mismatches.is_empty())
    {
        message.push_str("\n  robot ");
        message.push_str(&diagnostic.robot_id);
        message.push(':');
        for mismatch in &diagnostic.mismatches {
            message.push_str("\n    - ");
            message.push_str(&mismatch.human_line());
        }
    }
    message
}

pub(crate) fn enforce_coherence(
    verb: CoherenceVerb,
    diagnostics: &[RobotCoherenceDiagnostic],
    message_format: MessageFormat,
) -> Result<()> {
    if coherence_disposition(verb, true, diagnostics) == CoherenceDisposition::Pass {
        return Ok(());
    }
    if message_format == MessageFormat::Json {
        #[derive(Serialize)]
        struct CoherenceFailure<'a> {
            status: &'static str,
            coherence: &'a [RobotCoherenceDiagnostic],
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&CoherenceFailure {
                status: "error",
                coherence: diagnostics,
            })?
        );
        bail!("participant contract coherence check failed")
    }
    bail!("{}", format_coherence_error(diagnostics))
}

impl CheckCmd {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let project_root = app.project.root().to_path_buf();
        let options = CheckOptions {
            service: self.service.clone(),
            catalog_source: app.catalog_source.clone(),
            overlays: self.env.clone(),
            target: self.target.clone(),
            emit_update_notice: true,
            strict: self.strict,
        };
        let ui = app.ui;
        let result = tokio::task::spawn_blocking(move || run(&project_root, options, &ui))
            .await
            .context("check worker failed")??;

        // Keep the human warning before the hard outcome check, but JSON's
        // output contract reserves stderr for no bytes at all.
        if self.message_format == MessageFormat::Human {
            eprintln!(
                "warning: v0 is pre-stable: artifacts built at different times may not interoperate"
            );
        }

        ensure_check_outcome_ok(&result.channel, &result.outcome)?;
        match coherence_disposition(CoherenceVerb::Check, result.strict, &result.coherence) {
            CoherenceDisposition::Pass => {}
            CoherenceDisposition::Warning if self.message_format == MessageFormat::Human => {
                eprintln!("warning: {}", format_coherence_error(&result.coherence));
            }
            CoherenceDisposition::Warning => {}
            CoherenceDisposition::Failure => {
                enforce_coherence(CoherenceVerb::Check, &result.coherence, self.message_format)?;
            }
        }

        let output = CheckOutput {
            status: if coherence_is_ok(&result.coherence) {
                "ok"
            } else {
                "warning"
            },
            channel: result.channel.clone(),
            catalog_snapshot: result.catalog_snapshot.clone(),
            participant_count: result.participant_count,
            coherence: result.coherence.clone(),
        };
        crate::commands::print_message(
            &output,
            || {
                println!(
                    "ok: {} participants validated (channel {})",
                    result.participant_count, result.channel
                );
                if let Some(revision) = &result.catalog_snapshot {
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
    channel: String,
    catalog_snapshot: Option<String>,
    participant_count: usize,
    coherence: Vec<RobotCoherenceDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CheckRunResult {
    channel: String,
    catalog_snapshot: Option<String>,
    participant_count: usize,
    outcome: CheckOutcome,
    coherence: Vec<RobotCoherenceDiagnostic>,
    strict: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformArtifactRef {
    pub name: String,
    pub kind: ArtifactKind,
    pub artifact_ref: String,
    /// The component instance ids launching this artifact, for a
    /// `ComponentDriver` ref only. Empty for every other kind (a normal
    /// graph-scoped singleton participant). A catalog-resolved component
    /// driver is fetched once but launched once per instance that declares
    /// it (`left_drive`/`right_drive` sharing one `phoxal/component-<id>
    /// -driver` package) - mirrors how [`SourceParticipant::component_driver_with_artifact_id`]
    /// keys a path/git-overridden driver's graph membership by instance, not
    /// by artifact id. Must not be empty when `kind == ComponentDriver`.
    pub instances: Vec<String>,
}

impl PlatformArtifactRef {
    fn kind_label(&self) -> &'static str {
        match self.kind {
            ArtifactKind::Service => "official service",
            ArtifactKind::ComponentAssets => "official component assets",
            ArtifactKind::ComponentDriver => "official driver",
            ArtifactKind::Tool => "official tool",
            ArtifactKind::Simulator => "official simulator",
            ArtifactKind::Infrastructure => "official infrastructure",
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
    let catalog = crate::commands::catalog_or_vendored(
        crate::catalog::load_pinned_catalog(
            crate::catalog::CatalogLoadOptions {
                cli_source: options.catalog_source.clone(),
                robot_source: manifest_extras.catalog_source.as_ref().map(|source| {
                    if source.is_absolute() {
                        source.clone()
                    } else {
                        project_root.join(source)
                    }
                }),
                offline: false,
            },
            crate::catalog::selection_channel(robot.artifacts.channel),
            ui.mode(),
        ),
        ui.mode(),
    )?;
    // `check` resolves live git component refs so component drivers can be
    // located and staged. A path-only / official-only graph needs no component
    // network; a git component pinned to a commit SHA resolves offline; a
    // tag/branch ref is resolved live via `git ls-remote` with an actionable
    // error if the network is unavailable.
    let target_triple = options
        .target
        .as_deref()
        .map(crate::resolver::resolve_target_triple)
        .transpose()?;
    let resolved = resolve(
        &robot,
        project_root,
        catalog.as_ref(),
        ResolveOptions {
            refresh_channel_head: false,
            emit_update_notice: options.emit_update_notice,
            resolve_source_commits: true,
            resolve_component_asset_commits: false,
            official_target_triple: target_triple.clone(),
            tool_target_triple: target_triple,
            output_mode: ui.mode(),
        },
    )?;
    let descriptors = crate::native_artifacts::descriptors_for(&resolved, false, false)?;
    crate::native_artifacts::prepare_descriptors_with_preflight(&descriptors, Some(ui))?;
    let platform_refs = check_artifact_refs_from_resolved(&resolved);
    ensure_catalog_availability(&resolved)?;
    let tool_participants = tool_participants_from_resolved(&resolved)?;
    let all_source_participants =
        source_participants_from_resolved(project_root, &resolved, component_driver_crate_dir)?;
    if let Some(service_name) = options.service.as_deref() {
        ensure_user_service_exists(&resolved, service_name)?;
    }
    // `--service <name>` used to scope the (expensive) build to just the
    // named service, reusing disk-cached `emit-apis` for every other source
    // participant. That disk cache is gone (docs: no cross-invocation
    // caching - every source participant is rebuilt live every run), so
    // every source participant always builds now; `--service` still narrows
    // which official platform refs are checked (below).
    let source_participants = all_source_participants.as_slice();
    let platform_refs = if options.service.is_some() {
        &[][..]
    } else {
        platform_refs.as_slice()
    };
    let participant_count =
        platform_refs.len() + tool_participants.len() + source_participants.len();
    let mut official_by_ref = resolved
        .platform_runtimes
        .iter()
        .map(|runtime| (runtime.artifact_ref().to_string(), runtime))
        .collect::<std::collections::BTreeMap<_, _>>();
    official_by_ref.extend(component_driver_runtimes_by_ref(&resolved));
    let tools_by_ref = resolved
        .tools
        .iter()
        .map(|tool| (tool.asset.clone(), tool))
        .collect::<std::collections::BTreeMap<_, _>>();
    let outcome = run_check_with_context(
        platform_refs,
        &tool_participants,
        source_participants,
        CheckGraphContext {
            manifest_extras: &manifest_extras,
        },
        |artifact_ref| {
            if let Some(runtime) = official_by_ref.get(artifact_ref) {
                return extract_emit_apis_from_staged_runtime(runtime);
            }
            if let Some(tool) = tools_by_ref.get(artifact_ref) {
                return extract_emit_apis_from_staged_tool(tool);
            }
            Err(anyhow!(
                "resolved official artifact {artifact_ref} is not in the catalog"
            ))
        },
        fetch_emit_apis_from_tool,
        |participant| build_emit_apis_from_source_for_check(participant, ui),
    )?;

    let coherence = vec![evaluate_robot_coherence(
        &resolved.robot.robot.id,
        &outcome.contract_surfaces,
    )];
    Ok(CheckRunResult {
        channel: resolved.channel.to_string(),
        catalog_snapshot: resolved.catalog_snapshot,
        participant_count,
        outcome,
        coherence,
        strict: options.strict,
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
    pub fn user_service(name: impl Into<String>, crate_dir: PathBuf) -> Self {
        let name = name.into();
        Self {
            expected_artifact_id: name.clone(),
            name,
            crate_dir,
            kind: SourceParticipantKind::UserService,
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

/// A source participant's role plus whether it has a known official/catalog
/// identity it locally overrides. Deliberately kept as its own enum rather
/// than collapsed into the shared `participant_kind::ParticipantKind`: every
/// `SourceParticipant` already carries a `crate_dir`, so it is inherently
/// "local" in the supervisor's sense - the real orthogonal bit this domain
/// needs is "does an official/catalog identity exist for this name" (see
/// `official`), not "is it local". `UserService` has no catalog counterpart
/// at all (a robot developer's own service); `OfficialService` is a known
/// official service whose source the robot developer is locally overriding;
/// `Tool`/`Simulator` are always the latter shape (a source override of a
/// known official artifact - see `kind_label`); `ComponentDriver` has no
/// such axis. Use [`Self::shared_kind`] to bridge into the shared enum for
/// call sites (`supervisor`, `watch`) that only care about the role split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceParticipantKind {
    UserService,
    OfficialService,
    ComponentDriver,
    Tool,
    Simulator,
}

impl SourceParticipantKind {
    #[must_use]
    pub const fn shared_kind(self) -> crate::participant_kind::ParticipantKind {
        use crate::participant_kind::ParticipantKind;
        match self {
            Self::UserService | Self::OfficialService => ParticipantKind::Service,
            Self::ComponentDriver => ParticipantKind::Driver,
            Self::Tool => ParticipantKind::Tool,
            Self::Simulator => ParticipantKind::Simulator,
        }
    }

    /// Whether this source participant has a known official/catalog identity
    /// it is locally overriding, vs one invented purely by the user with no
    /// catalog counterpart at all (only `UserService`). See the type docs for
    /// why this is named `official`, not `local`.
    #[must_use]
    pub const fn official(self) -> bool {
        !matches!(self, Self::UserService)
    }
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
        .filter(|tool| tool.kind == ArtifactKind::Tool)
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
            instances: Vec::new(),
        })
        .collect()
}

/// One `PlatformArtifactRef` per distinct Catalog-sourced `component_driver`
/// package, `instances` listing every component instance that shares it
/// (`left_drive`/`right_drive` both resolving
/// `phoxal/component-ddsm115`). A Path/Git-sourced driver is a source
/// participant instead (see `source_participants_from_resolved`) and is not
/// included here. Reused by every command that validates the graph like a
/// service (`check`, `run`, `deploy`); `simulate` also fetches through this
/// same function but discards a driver participant from its final launch set
/// after validating its contracts (drivers are sim-substituted, never
/// launched).
pub(crate) fn component_driver_platform_refs_from_resolved(
    resolved: &ResolvedRobot,
) -> Vec<PlatformArtifactRef> {
    struct CatalogDriverRef {
        name: String,
        artifact_ref: String,
        instances: Vec<String>,
    }

    let mut by_package = std::collections::BTreeMap::<String, CatalogDriverRef>::new();
    for component in &resolved.components {
        let Some(driver) = &component.driver else {
            continue;
        };
        if !matches!(driver.source, ResolvedComponentSource::Catalog) {
            continue;
        }
        let Some(runtime) = &driver.catalog_runtime else {
            continue;
        };
        by_package
            .entry(driver.package.clone())
            .or_insert_with(|| CatalogDriverRef {
                name: runtime.name.clone(),
                artifact_ref: runtime.artifact_ref().to_string(),
                instances: Vec::new(),
            })
            .instances
            .push(component.instance.clone());
    }
    by_package
        .into_values()
        .map(|driver_ref| PlatformArtifactRef {
            name: driver_ref.name,
            kind: ArtifactKind::ComponentDriver,
            artifact_ref: driver_ref.artifact_ref,
            instances: driver_ref.instances,
        })
        .collect()
}

/// Every distinct Catalog-sourced component driver's `catalog_runtime`, keyed
/// by its `artifact_ref` - the same shape as the `official_by_ref` map every
/// caller already builds from `resolved.platform_runtimes` for the shared
/// `extract_emit_apis_from_staged_runtime` closure. Callers merge this in so
/// one fetch closure resolves services, simulators, AND catalog component
/// drivers identically.
pub(crate) fn component_driver_runtimes_by_ref(
    resolved: &ResolvedRobot,
) -> std::collections::BTreeMap<String, &ResolvedPlatformRuntime> {
    resolved
        .components
        .iter()
        .filter_map(|component| component.driver.as_ref())
        .filter(|driver| matches!(driver.source, ResolvedComponentSource::Catalog))
        .filter_map(|driver| driver.catalog_runtime.as_ref())
        .map(|runtime| (runtime.artifact_ref().to_string(), runtime))
        .collect()
}

pub(crate) fn check_artifact_refs_from_resolved(
    resolved: &ResolvedRobot,
) -> Vec<PlatformArtifactRef> {
    let mut refs = platform_artifact_refs_from_resolved(resolved);
    refs.extend(component_driver_platform_refs_from_resolved(resolved));
    refs.extend(
        resolved
            .tools
            .iter()
            .filter(|tool| tool.kind == ArtifactKind::Tool)
            .filter(|tool| tool.path_override.is_none())
            .filter(|tool| tool_env_override(tool).is_none())
            .map(|tool| PlatformArtifactRef {
                name: tool.name.clone(),
                kind: tool.kind,
                artifact_ref: tool.asset.clone(),
                instances: Vec::new(),
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

    // A Catalog-sourced driver is a first-class catalog artifact, not a
    // build-from-source participant - it becomes a `PlatformArtifactRef`
    // instead (see `component_driver_platform_refs_from_resolved`), fetched
    // and validated like a service. Only a Path/Git (fork/dev-override)
    // driver builds from source here.
    for component in resolved.components.iter().filter(|component| {
        component
            .driver
            .as_ref()
            .is_some_and(|driver| !matches!(driver.source, ResolvedComponentSource::Catalog))
    }) {
        let crate_dir = if let Some(path) = component.driver_path_override() {
            path.to_path_buf()
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

    for tool in resolved
        .tools
        .iter()
        .filter(|tool| tool.kind == ArtifactKind::Tool)
        .filter_map(|tool| {
            tool.path_override.as_ref().map(|path| {
                SourceParticipant::tool(
                    tool.name.clone(),
                    tool_emit_apis_id(&tool.name).to_string(),
                    path.clone(),
                )
            })
        })
    {
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
            bail!("user service '{service_name}' is not defined in services");
        }
        bail!("user service '{service_name}' is not defined in services; available: {available}");
    }
    Ok(())
}

fn ensure_catalog_availability(resolved: &ResolvedRobot) -> Result<()> {
    let unavailable = resolved
        .platform_runtimes
        .iter()
        .filter(|runtime| runtime.source_path().is_none())
        .filter(|runtime| !runtime.published)
        .collect::<Vec<_>>();
    if unavailable.is_empty() {
        return Ok(());
    }

    let mut message = format!(
        "NotYetAvailable: {} is not deployable on {}",
        resolved.robot.robot.id, resolved.target
    );
    if let Some(revision) = &resolved.catalog_snapshot {
        message.push_str("\n\ncatalog revision: ");
        message.push_str(revision);
    }
    message.push_str("\n\nRequired artifacts not released:");
    for runtime in unavailable {
        message.push_str("\n  - ");
        message.push_str(&runtime.package);
        message.push_str(" is missing for ");
        message.push_str(&resolved.target);
        if !runtime.published_triples.is_empty() {
            message.push_str("; published triples: ");
            message.push_str(&runtime.published_triples.join(", "));
        }
    }
    message.push_str(
        "\n\nFix: wait for the listed official artifacts to publish, or pin artifacts.pins.<package> to an exact version/sha256 whose changed contracts you do not need.",
    );
    bail!("{message}")
}

pub fn run_check(
    resolved_platform_image_refs: &[(String, String)],
    tool_participants: &[ToolParticipant],
    source_participants: &[SourceParticipant],
    fetch: impl FnMut(&str) -> Result<RawEmitApis>,
    fetch_tool: impl FnMut(&ToolParticipant) -> Result<RawEmitApis>,
    build: impl FnMut(&SourceParticipant) -> Result<RawEmitApis>,
) -> Result<CheckOutcome> {
    let manifest_extras = RobotManifestExtras::default();
    let platform_artifact_refs = service_platform_artifact_refs(resolved_platform_image_refs);
    run_check_with_context(
        &platform_artifact_refs,
        tool_participants,
        source_participants,
        CheckGraphContext {
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
    fetch_tool: impl FnMut(&ToolParticipant) -> Result<RawEmitApis>,
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
            instances: Vec::new(),
        })
        .collect()
}

pub fn run_check_with_deployed_user_service_images(
    inputs: CheckParticipants<'_>,
    context: CheckGraphContext<'_>,
    mut fetch: impl FnMut(&str) -> Result<RawEmitApis>,
    mut fetch_tool: impl FnMut(&ToolParticipant) -> Result<RawEmitApis>,
    mut build: impl FnMut(&SourceParticipant) -> Result<RawEmitApis>,
) -> Result<CheckOutcome> {
    let mut missing_images = Vec::new();
    let mut participants = Vec::new();
    let mut contract_surfaces = Vec::new();
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
        let expected_artifact_id = if artifact.kind == ArtifactKind::Tool {
            tool_emit_apis_id(&artifact.name)
        } else {
            &artifact.name
        };
        validate_artifact_identity(
            artifact.kind_label(),
            expected_artifact_id,
            artifact.kind.emit_apis_kind(),
            &raw,
        )?;
        let participant =
            graph_check::ParticipantApis::try_from(raw.clone()).with_context(|| {
                format!(
                    "failed to interpret emit-apis for {} {} ({image_ref})",
                    artifact.kind_label(),
                    artifact.name
                )
            })?;
        if artifact.instances.is_empty() {
            contract_surfaces.push(contract_surface(&raw, artifact.name.clone()));
            participants.push(participant);
        } else {
            // A catalog component driver is fetched once but launched once
            // per instance that declares it - key each instance's graph
            // membership by its own id, exactly like a path/git-overridden
            // driver source participant does (see
            // `SourceParticipantKind::ComponentDriver` below).
            for instance in &artifact.instances {
                let mut instance_participant = participant.clone();
                instance_participant.participant_id = instance.clone();
                instance_participant.scope =
                    graph_check::ParticipantScope::ComponentInstance(instance.clone());
                contract_surfaces.push(contract_surface(&raw, instance.clone()));
                participants.push(instance_participant);
            }
        }
    }

    for service in inputs.user_service_images {
        let raw = fetch(&service.image_ref).with_context(|| {
            format!(
                "failed to obtain emit-apis for user service {} ({})",
                service.name, service.image_ref
            )
        })?;
        validate_service_artifact_identity("user service", &service.name, &raw)?;
        let participant =
            graph_check::ParticipantApis::try_from(raw.clone()).with_context(|| {
                format!(
                    "failed to interpret emit-apis for user service {} ({})",
                    service.name, service.image_ref
                )
            })?;
        contract_surfaces.push(contract_surface(&raw, service.name.clone()));
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
        let raw = fetch_tool(tool).with_context(|| {
            format!(
                "failed to obtain emit-apis for tool {} ({})",
                tool.name,
                tool.binary_path.display()
            )
        })?;
        let expected_id = tool_emit_apis_id(&tool.name);
        validate_artifact_identity("tool", expected_id, "tool", &raw)?;
        let participant =
            graph_check::ParticipantApis::try_from(raw.clone()).with_context(|| {
                format!(
                    "failed to interpret emit-apis for tool {} ({})",
                    tool.name,
                    tool.binary_path.display()
                )
            })?;
        contract_surfaces.push(contract_surface(&raw, tool.name.clone()));
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
        let mut participant_apis = graph_check::ParticipantApis::try_from(raw.clone())
            .with_context(|| {
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
        let surface_participant_id = if participant.kind == SourceParticipantKind::Tool {
            participant.name.clone()
        } else {
            participant_apis.participant_id.clone()
        };
        contract_surfaces.push(contract_surface(&raw, surface_participant_id));
        participants.push(participant_apis);
    }

    let mut report = graph_check::check_graph(&participants);
    report.problems.extend(config_problems);
    Ok(CheckOutcome {
        missing_images,
        report,
        checked_participants: participants,
        contract_surfaces,
    })
}

pub(crate) fn contract_surface(
    raw: &RawEmitApis,
    participant_id: String,
) -> graph_check::ParticipantContractSurface {
    graph_check::ParticipantContractSurface {
        participant_id,
        contracts: raw
            .required_contracts
            .iter()
            .map(
                |contract| crate::participant_metadata::ParticipantMetaContract {
                    role: contract.role.clone(),
                    version: contract.version.clone(),
                    contract: contract.contract.clone(),
                    external: contract.external,
                },
            )
            .collect(),
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
    let errors = validate_json_schema(schema, &config, &format!("services.{service_id}.config"));
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

pub(crate) fn extract_emit_apis_from_staged_runtime(
    runtime: &ResolvedPlatformRuntime,
) -> Result<RawEmitApis> {
    #[cfg(test)]
    if runtime
        .url
        .as_deref()
        .is_some_and(|url| url.starts_with("https://example.invalid/"))
    {
        return Ok(raw_emit_apis_from_extracted_metadata(
            runtime.kind.emit_apis_kind(),
            &runtime.name,
            crate::participant_metadata::ParticipantMeta {
                participant_api: "fixture".to_string(),
                contracts: Vec::new(),
                config_schema: serde_json::json!({ "type": "null" }),
            },
        ));
    }
    let binary = crate::native_artifacts::stage_runtime(
        None,
        runtime,
        crate::native_artifacts::ProvisioningMode::MissingOnly,
    )?
    .ok_or_else(|| anyhow!("{} has no staged binary", runtime.package))?;
    let meta = crate::participant_metadata::extract_participant_metadata(&binary)
        .with_context(|| format!("failed to extract API metadata from {}", binary.display()))?;
    Ok(raw_emit_apis_from_extracted_metadata(
        runtime.kind.emit_apis_kind(),
        &runtime.name,
        meta,
    ))
}

pub(crate) fn extract_emit_apis_from_staged_tool(
    tool: &crate::resolver::ResolvedTool,
) -> Result<RawEmitApis> {
    #[cfg(test)]
    if !tool.published
        || tool
            .url
            .as_deref()
            .is_some_and(|url| url.starts_with("https://example.invalid/"))
    {
        return Ok(raw_emit_apis_from_extracted_metadata(
            "tool",
            crate::resolver::tool_emit_apis_id(&tool.name),
            crate::participant_metadata::ParticipantMeta {
                participant_api: "fixture".to_string(),
                contracts: Vec::new(),
                config_schema: serde_json::json!({ "type": "null" }),
            },
        ));
    }
    let binary = crate::native_artifacts::stage_tool(
        None,
        tool,
        crate::native_artifacts::ProvisioningMode::MissingOnly,
    )?
    .ok_or_else(|| anyhow!("{} has no staged binary", tool.package))?;
    let meta = crate::participant_metadata::extract_participant_metadata(&binary)
        .with_context(|| format!("failed to extract API metadata from {}", binary.display()))?;
    Ok(raw_emit_apis_from_extracted_metadata(
        "tool",
        crate::resolver::tool_emit_apis_id(&tool.name),
        meta,
    ))
}

/// Every native tool (`tool-router`, `tool-joypad`) is privileged (host/root
/// access); every other kind is a checked participant. Neither the catalog
/// nor a binary's extracted metadata carries `participant_class` anymore, so
/// the kind -> class mapping (always fixed) is derived here instead of read
/// off either source.
fn default_participant_class_for_kind(artifact_kind: &str) -> String {
    if artifact_kind == "tool" {
        "privileged".to_string()
    } else {
        default_participant_class()
    }
}

/// Fetches a native tool binary's contract report by extracting its
/// compiled-in `#[derive(phoxal::Api)]` metadata section directly from the
/// built artifact file - never by executing it (the `emit-apis` runtime
/// subcommand this used to run is gone). A binary's own linker section
/// carries only its contracts, not its artifact identity (`kind`/`id`) or a
/// artifact identity, so the identity is supplied from what is already known
/// about `tool`; contracts and the config schema both come from the section.
pub(crate) fn fetch_emit_apis_from_tool(tool: &ToolParticipant) -> Result<RawEmitApis> {
    let meta = crate::participant_metadata::extract_participant_metadata(&tool.binary_path)
        .with_context(|| {
            format!(
                "failed to extract API metadata from {}",
                tool.binary_path.display()
            )
        })?;
    Ok(raw_emit_apis_from_extracted_metadata(
        "tool",
        crate::resolver::tool_emit_apis_id(&tool.name),
        meta,
    ))
}

/// Builds a [`RawEmitApis`] from a binary's extracted [`ParticipantMeta`] plus
/// already-known artifact identity - the shared tail of
/// [`fetch_emit_apis_from_tool`] and [`build_emit_apis_by_building`].
///
/// [`ParticipantMeta`]: crate::participant_metadata::ParticipantMeta
fn raw_emit_apis_from_extracted_metadata(
    artifact_kind: &str,
    artifact_id: &str,
    meta: crate::participant_metadata::ParticipantMeta,
) -> RawEmitApis {
    RawEmitApis {
        artifact: RawArtifact {
            kind: artifact_kind.to_string(),
            id: artifact_id.to_string(),
        },
        participant_class: default_participant_class_for_kind(artifact_kind),
        // No single API version to report anymore (D1): a participant's
        // `Api` may mix contracts from several versions freely, so there
        // is no one dated value left to put here.
        api_version: String::new(),
        required_contracts: meta.contracts,
        config_schema: Some(meta.config_schema),
    }
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

/// Build a source participant's contract report by actually compiling it -
/// never cached. Custom / `path:` / git-sourced participants are
/// re-evaluated every invocation (docs: the old `cache/emit-apis/` disk cache
/// is gone; official artifacts already skip this entirely, reading contracts
/// straight off the catalog - see [`extract_emit_apis_from_staged_runtime`]).
/// The build no longer runs the compiled binary (the `emit-apis` runtime
/// subcommand this used to execute is gone): it extracts the participant's
/// compiled-in `#[derive(phoxal::Api)]` metadata section straight from the
/// built artifact file, exactly like [`fetch_emit_apis_from_tool`].
pub(crate) fn build_emit_apis_from_source(participant: &SourceParticipant) -> Result<RawEmitApis> {
    build_emit_apis_from_source_with_diagnostics(participant, build_emit_apis_by_building, None)
}

fn build_emit_apis_from_source_for_check(
    participant: &SourceParticipant,
    ui: &crate::Ui,
) -> Result<RawEmitApis> {
    build_emit_apis_from_source_with_diagnostics(participant, build_emit_apis_by_building, Some(ui))
}

/// Core of [`build_emit_apis_from_source`], parameterized over the (expensive)
/// builder so tests can exercise it against a fake build closure instead of
/// spawning a real `cargo build`.
fn build_emit_apis_from_source_with_diagnostics(
    participant: &SourceParticipant,
    mut build_by_building: impl FnMut(&SourceParticipant) -> Result<RawEmitApis>,
    ui: Option<&crate::Ui>,
) -> Result<RawEmitApis> {
    let raw = build_by_building(participant)?;
    report_source_emit_apis_progress(
        ui,
        format!(
            "built emit-apis for {} {}",
            participant.kind_label(),
            participant.name
        ),
    );
    Ok(raw)
}

fn report_source_emit_apis_progress(ui: Option<&crate::Ui>, message: String) {
    if let Some(ui) = ui {
        ui.info(message);
    }
}

/// The expected `artifact.kind` label for a [`SourceParticipant`]'s kind -
/// shared between [`build_emit_apis_by_building`] (which now supplies this
/// identity itself, since extraction never self-reports it) and
/// [`validate_source_artifact_identity`] (which still checks a fake/injected
/// report against it in tests).
fn expected_kind_for_source_participant(kind: SourceParticipantKind) -> &'static str {
    kind.shared_kind().label()
}

fn build_emit_apis_by_building(participant: &SourceParticipant) -> Result<RawEmitApis> {
    let crate_dir = participant.crate_dir.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize source crate {}",
            participant.crate_dir.display()
        )
    })?;
    let binary_name = cargo_binary_name(&crate_dir, None)?;
    let binary_path = build_and_locate_binary(&crate_dir, &binary_name)?;
    let meta = crate::participant_metadata::extract_participant_metadata(&binary_path)
        .with_context(|| {
            format!(
                "failed to extract API metadata from {}",
                binary_path.display()
            )
        })?;
    Ok(raw_emit_apis_from_extracted_metadata(
        expected_kind_for_source_participant(participant.kind),
        &participant.expected_artifact_id,
        meta,
    ))
}

/// Builds `binary_name` in `crate_dir` and locates its resulting executable
/// path via cargo's own `--message-format=json` build log, rather than
/// guessing `<dir>/target/debug/<bin>` by hand: a crate that is a workspace
/// member (e.g. a `phoxal/framework` `component/<name>` driver) compiles into
/// the *workspace-root* `target/`, not `<crate_dir>/target/`, so a fixed path
/// would miss it. Cargo's own artifact messages are workspace-aware
/// regardless of layout.
fn build_and_locate_binary(crate_dir: &Path, binary_name: &str) -> Result<PathBuf> {
    // `run_output` fully captures the child's stdout/stderr (nothing is
    // inherited), so an animated spinner here never collides with cargo's
    // own live compiler output - unlike `run::build_source_binary`, whose
    // `cargo build` inherits the terminal so its errors stream live and gets
    // a static themed line instead (see that function's doc comment).
    //
    // This sits behind `build_emit_apis_by_building`, which is itself passed
    // around as a bare fn pointer matching a shared closure signature (the
    // `build_by_building` parameter of `build_emit_apis_from_source_with_diagnostics`,
    // and the `run_check_with_context` callback used identically by
    // `run`/`deploy`/`simulate`/`watch`) - adding a `mode` parameter here
    // would have to ripple through that whole shared contract. Recomputing
    // fresh from the environment is the explicit, non-global fallback
    // (`OutputMode::from_env`'s docs) for exactly this case.
    let progress = crate::progress::spinner(
        format!("building `{binary_name}` in {}", crate_dir.display()),
        crate::output_mode::OutputMode::from_env(),
    );
    let result = crate::shell::run_output(
        "cargo",
        [
            "build",
            "--quiet",
            "--message-format=json",
            "--bin",
            binary_name,
        ],
        Some(crate_dir),
    )
    .with_context(|| format!("failed to spawn `cargo build --bin {binary_name}`"));
    let output = match result {
        Ok(output) => {
            progress.finish_and_clear();
            output
        }
        Err(error) => {
            progress.abandon_with_message(format!("failed to build `{binary_name}`: {error:#}"));
            return Err(error);
        }
    };
    if !output.status.success() {
        bail!(
            "failed to build `{binary_name}` in {}\nstdout:\n{}\nstderr:\n{}",
            crate_dir.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stdout = String::from_utf8(output.stdout)
        .context("cargo build --message-format=json wrote non-UTF8 stdout")?;
    for line in stdout.lines() {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if message.get("reason").and_then(Value::as_str) != Some("compiler-artifact") {
            continue;
        }
        let Some(target) = message.get("target") else {
            continue;
        };
        let is_bin = target
            .get("kind")
            .and_then(Value::as_array)
            .is_some_and(|kinds| kinds.iter().any(|kind| kind.as_str() == Some("bin")));
        if !is_bin || target.get("name").and_then(Value::as_str) != Some(binary_name) {
            continue;
        }
        if let Some(executable) = message.get("executable").and_then(Value::as_str) {
            return Ok(PathBuf::from(executable));
        }
    }
    bail!(
        "cargo build for `{binary_name}` in {} did not report an executable path",
        crate_dir.display()
    )
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
    validate_artifact_identity(
        participant.kind_label(),
        participant.expected_artifact_id.as_str(),
        expected_kind_for_source_participant(participant.kind),
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
        // `role` is dropped here (D1): `phoxal::check::Contract` is
        // `{family}` only - name identity alone decides compatibility, so
        // there is nothing left for the graph checker to gate per-role.
        // `role` remains in the binary metadata representation for callers
        // that need to inspect participant intent.
        let contracts = raw
            .required_contracts
            .into_iter()
            .map(|contract| graph_check::Contract {
                family: format!("{}::{}", contract.version, contract.contract),
            })
            .collect::<Vec<_>>();

        Ok(Self {
            // Default the participant id to the artifact id; callers that launch
            // one artifact per instance (component drivers) override it with the
            // concrete instance id below.
            participant_id: artifact_id.clone(),
            artifact_id,
            participant_kind,
            participant_class,
            api_version: raw.api_version,
            config_schema: raw.config_schema,
            scope: graph_check::ParticipantScope::Graph,
            contracts,
        })
    }
}

pub(crate) fn ensure_check_outcome_ok(channel: &str, outcome: &CheckOutcome) -> Result<()> {
    if !outcome.missing_images.is_empty() {
        bail!(
            "{}",
            format_missing_images_error(channel, &outcome.missing_images)
        );
    }

    if !outcome.report.is_ok() {
        bail!("{}", format_report_error(&outcome.report));
    }

    Ok(())
}

fn format_missing_images_error(channel: &str, missing_images: &[String]) -> String {
    let mut message = format!("required official artifacts are not available on channel {channel}");
    message.push_str("\n\nMissing official artifacts:");
    for image_ref in missing_images {
        message.push_str("\n  - ");
        message.push_str(image_ref);
    }
    message.push_str("\n\nFix:");
    message.push_str("\n  - refresh or override the generated artifact catalog with `phoxal-cli --catalog <path> check`");
    message.push_str("\n  - or wait until Phoxal publishes the missing official artifacts on the ");
    message.push_str(channel);
    message.push_str(" channel");
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
        graph_check::Problem::InvalidConfig { runtime_id, errors } => {
            format!(
                "invalid config for user service {runtime_id}: {}",
                errors.join("; ")
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
    use crate::resolver::{ResolvedComponentPackage, ResolvedComponentSource};
    use graph_check::{ParticipantClass, Problem};
    use phoxal::model::robot::v0::Robot;
    use std::collections::BTreeMap;

    fn coherence_surface(
        participant_id: &str,
        contracts: &[(&str, &str, &str)],
    ) -> graph_check::ParticipantContractSurface {
        graph_check::ParticipantContractSurface {
            participant_id: participant_id.to_string(),
            contracts: contracts
                .iter()
                .map(|(role, version, contract)| {
                    crate::participant_metadata::ParticipantMetaContract {
                        role: (*role).to_string(),
                        version: (*version).to_string(),
                        contract: (*contract).to_string(),
                        external: false,
                    }
                })
                .collect(),
        }
    }

    fn assert_severity_matrix(diagnostics: &[RobotCoherenceDiagnostic], coherent: bool) {
        let check = if coherent {
            CoherenceDisposition::Pass
        } else {
            CoherenceDisposition::Warning
        };
        let hard = if coherent {
            CoherenceDisposition::Pass
        } else {
            CoherenceDisposition::Failure
        };
        assert_eq!(
            coherence_disposition(CoherenceVerb::Check, false, diagnostics),
            check
        );
        assert_eq!(
            coherence_disposition(CoherenceVerb::Check, true, diagnostics),
            hard
        );
        for verb in [
            CoherenceVerb::Deploy,
            CoherenceVerb::Run,
            CoherenceVerb::Simulate,
        ] {
            assert_eq!(coherence_disposition(verb, false, diagnostics), hard);
        }
    }

    #[test]
    fn coherent_contract_set_passes_every_verb() {
        let surfaces = vec![
            coherence_surface("producer", &[("publish", "v1", "drive::Target")]),
            coherence_surface("consumer", &[("subscribe", "v1", "drive::Target")]),
            coherence_surface("server", &[("serve", "v1", "map::Get")]),
            coherence_surface("client", &[("ask", "v1", "map::Get")]),
        ];
        let diagnostics = vec![evaluate_robot_coherence("robot-a", &surfaces)];
        assert_severity_matrix(&diagnostics, true);
    }

    #[test]
    fn pub_sub_disjoint_warns_for_check_and_fails_strict_and_launch_verbs() {
        let surfaces = vec![
            coherence_surface("producer", &[("publish", "v1", "drive::Target")]),
            coherence_surface("consumer", &[("subscribe", "v2", "drive::Target")]),
        ];
        let diagnostics = vec![evaluate_robot_coherence("robot-a", &surfaces)];
        assert!(matches!(
            diagnostics[0].mismatches.as_slice(),
            [CoherenceMismatchDiagnostic::PubSubDisjoint { .. }]
        ));
        assert_severity_matrix(&diagnostics, false);
    }

    #[test]
    fn unserved_ask_warns_for_check_and_fails_strict_and_launch_verbs() {
        let surfaces = vec![
            coherence_surface("server", &[("serve", "v1", "map::Get")]),
            coherence_surface("client", &[("ask", "v2", "map::Get")]),
        ];
        let diagnostics = vec![evaluate_robot_coherence("robot-a", &surfaces)];
        assert!(matches!(
            diagnostics[0].mismatches.as_slice(),
            [CoherenceMismatchDiagnostic::UnservedAsk { .. }]
        ));
        assert_severity_matrix(&diagnostics, false);
    }

    #[test]
    fn robot_graphs_are_checked_independently_not_pooled() {
        let robot_a = vec![
            coherence_surface("a-producer", &[("publish", "v1", "drive::Target")]),
            coherence_surface("a-consumer", &[("subscribe", "v2", "drive::Target")]),
        ];
        let robot_b = vec![coherence_surface(
            "b-producer",
            &[("publish", "v2", "drive::Target")],
        )];

        let diagnostics = [
            evaluate_robot_coherence("robot-a", &robot_a),
            evaluate_robot_coherence("robot-b", &robot_b),
        ];
        assert_eq!(diagnostics[0].mismatches.len(), 1);
        assert!(diagnostics[1].mismatches.is_empty());

        let pooled = robot_a
            .into_iter()
            .chain(robot_b)
            .collect::<Vec<graph_check::ParticipantContractSurface>>();
        assert!(
            evaluate_robot_coherence("incorrect-pool", &pooled)
                .mismatches
                .is_empty()
        );
    }

    fn fixture_component_package(
        package: &str,
        kind: crate::catalog::ArtifactKind,
        path: &str,
    ) -> ResolvedComponentPackage {
        ResolvedComponentPackage {
            package: package.to_string(),
            kind,
            source: ResolvedComponentSource::Path {
                path: PathBuf::from(path),
            },
            path_override: None,
            catalog_runtime: None,
        }
    }

    /// A Catalog-sourced component package with a populated `catalog_runtime`,
    /// the shape `resolve_component_package` produces once a matching release
    /// asset exists.
    fn fixture_catalog_component_package(
        package: &str,
        kind: crate::catalog::ArtifactKind,
        component_name: &str,
    ) -> ResolvedComponentPackage {
        ResolvedComponentPackage {
            package: package.to_string(),
            kind,
            source: ResolvedComponentSource::Catalog,
            path_override: None,
            catalog_runtime: Some(ResolvedPlatformRuntime {
                name: component_name.to_string(),
                package: package.to_string(),
                kind,
                version: "0.1.0".to_string(),
                artifact_ref: format!("{}-driver-v0.1.0.tar.zst", component_name),
                sha256: Some("a".repeat(64)),
                url: Some("https://example.invalid/component.tar.zst".to_string()),
                size: Some(1),
                published: true,
                published_triples: Vec::new(),
                path_override: None,
                channel: crate::catalog::SelectionChannel::Stable,
                target: Some("aarch64-unknown-linux-gnu".to_string()),
            }),
        }
    }

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
                "mission:ok" => Ok(raw("mission", "v1", &["drive::Target"])),
                unexpected => bail!("unexpected image {unexpected}"),
            },
            |_| bail!("no tools should be fetched"),
            |participant| {
                let dir = participant.crate_dir.as_path();
                if dir == Path::new("/fake/project/runtimes/drive") {
                    Ok(raw("drive", "v1", &["drive::Target"]))
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
                "mission:ok" => Ok(raw("mission", "v1", &["drive::Target"])),
                unexpected => bail!("unexpected image {unexpected}"),
            },
            |_| bail!("no tools should be fetched"),
            |participant| {
                let dir = participant.crate_dir.as_path();
                if dir == Path::new("/fake/project/components/ddsm115") {
                    Ok(raw_kind("driver", "ddsm115", "v1", &["drive::Target"]))
                } else {
                    bail!("unexpected source dir {}", dir.display())
                }
            },
        )?;

        assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
        Ok(())
    }

    #[test]
    fn privileged_tools_share_a_contract_family_with_no_agreement_gate() -> Result<()> {
        // D1: a privileged tool and a checked source participant reporting
        // different roles for the same `family` is not a mismatch - there is
        // no `schema_id` axis left to disagree on; name identity alone
        // decides compatibility.
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
            |tool| {
                let path = tool.binary_path.as_path();
                if path == Path::new("/fake/cache/joypad") {
                    Ok(raw_kind_with_role(
                        "tool",
                        "joypad",
                        "v1",
                        &[("drive::Target", "subscribe")],
                        "privileged",
                    ))
                } else {
                    bail!("unexpected tool path {}", path.display())
                }
            },
            |participant| {
                let dir = participant.crate_dir.as_path();
                if dir == Path::new("/fake/project/runtimes/drive") {
                    Ok(raw_with_role(
                        "drive",
                        "v1",
                        &[("drive::Target", "publish")],
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
            |tool| {
                let path = tool.binary_path.as_path();
                if path == Path::new("/fake/cache/joypad") {
                    Ok(raw_kind_class(
                        "tool",
                        "joypad",
                        "v1",
                        &["drive::Target", "odometry::State"],
                        "privileged",
                    ))
                } else {
                    bail!("unexpected tool path {}", path.display())
                }
            },
            |_| bail!("no source services should be built"),
        )?;

        assert!(outcome.report.problems.is_empty());
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
                manifest_extras: &extras,
            },
            |image_ref| {
                fetched_images.push(image_ref.to_string());
                Ok(raw("avoid", "v1", &[]))
            },
            |_| bail!("no tools should be fetched"),
            |participant| {
                let dir = participant.crate_dir.as_path();
                built_sources.push(dir.to_path_buf());
                Ok(raw_kind("driver", "ddsm115", "v1", &[]))
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
    fn source_and_platform_sharing_a_contract_family_is_a_healthy_graph() -> Result<()> {
        // D1: a platform publisher and a source subscriber sharing
        // `drive::Target` is healthy regardless of role - name identity
        // alone decides compatibility, there is no wire-shape agreement axis
        // left to gate on.
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
                "mission:ok" => Ok(raw_with_role(
                    "mission",
                    "v1",
                    &[("drive::Target", "publish")],
                )),
                unexpected => bail!("unexpected image {unexpected}"),
            },
            |_| bail!("no tools should be fetched"),
            |_| {
                Ok(raw_with_role(
                    "drive",
                    "v1",
                    &[("drive::Target", "subscribe")],
                ))
            },
        )?;

        assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
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
            |_| Ok(raw("surprise", "v1", &[])),
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
                "drive:swapped" => Ok(raw("mission", "v1", &[])),
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
            kind: ArtifactKind::ComponentDriver,
            artifact_ref: "driver-bno085:swapped".to_string(),
            instances: vec!["imu".to_string()],
        }];
        let extras = RobotManifestExtras::default();

        let error = run_check_with_context(
            &artifacts,
            &[],
            &[],
            CheckGraphContext {
                manifest_extras: &extras,
            },
            |artifact_ref| match artifact_ref {
                "driver-bno085:swapped" => Ok(raw_kind("service", "bno085", "v1", &[])),
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
            |tool| {
                let path = tool.binary_path.as_path();
                if path == Path::new("/fake/cache/joypad") {
                    Ok(raw_kind_class(
                        "tool",
                        "simulator_webots_controller",
                        "v1",
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
            |tool| {
                let path = tool.binary_path.as_path();
                if path == Path::new("/fake/cache/joypad") {
                    Ok(raw_kind_class("tool", "joypad", "v1", &[], "privileged"))
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
            |tool| {
                let path = tool.binary_path.as_path();
                if path == Path::new("/fake/cache/joypad") {
                    Ok(raw_kind_class("runtime", "joypad", "v1", &[], "privileged"))
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
            |_| Ok(raw_kind_class("driver", "ddsm115", "v1", &[], "checked")),
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
            |_| Ok(raw_kind_class("runtime", "ddsm115", "v1", &[], "checked")),
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
            |tool| {
                let path = tool.binary_path.as_path();
                if path == Path::new("/fake/cache/joypad") {
                    Ok(raw_kind_class(
                        "nonsense",
                        "joypad",
                        "v1",
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
    fn every_source_participant_always_builds_no_scoping_no_cache() -> Result<()> {
        // The old `check --service <name>` build-scoping ("UseCached" siblings
        // served from a disk cache) is gone: every source participant is
        // rebuilt live on every `check` invocation, scoped or not. This proves
        // `run_check` invokes the build closure for every source participant,
        // not just a named one.
        let sources = vec![
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

        let mut built = Vec::new();
        let outcome = run_check(
            &[],
            &[],
            &sources,
            |_| bail!("no platform images should be fetched"),
            |_| bail!("no tools should be fetched"),
            |participant| {
                let dir = participant.crate_dir.as_path();
                built.push(dir.to_path_buf());
                if dir == Path::new("/fake/project/runtimes/bad") {
                    Ok(raw("bad", "v1", &[]))
                } else if dir == Path::new("/fake/project/runtimes/other") {
                    Ok(raw("other", "v1", &[]))
                } else if dir == Path::new("/fake/project/components/ddsm115") {
                    Ok(raw_kind("driver", "ddsm115", "v1", &[]))
                } else {
                    bail!("unexpected source participant: {}", dir.display())
                }
            },
        )?;

        assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
        assert_eq!(
            built,
            vec![
                PathBuf::from("/fake/project/runtimes/bad"),
                PathBuf::from("/fake/project/runtimes/other"),
                PathBuf::from("/fake/project/components/ddsm115"),
            ],
            "every source participant must build, every invocation - no scoping, no cache"
        );
        Ok(())
    }

    #[test]
    fn component_driver_with_no_producer_is_a_legal_graph() -> Result<()> {
        // A component driver subscribing to a contract with no producer in the
        // graph is legal under the relaxed graph check.
        let sources = vec![
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

        let outcome = run_check(
            &[],
            &[],
            &sources,
            |_| bail!("no platform images should be fetched"),
            |_| bail!("no tools should be fetched"),
            |participant| {
                let dir = participant.crate_dir.as_path();
                if dir == Path::new("/fake/project/runtimes/other") {
                    Ok(raw("other", "v1", &[]))
                } else if dir == Path::new("/fake/project/components/ddsm115") {
                    Ok(raw_kind("driver", "ddsm115", "v1", &["drive::Target"]))
                } else {
                    bail!("unexpected source dir {}", dir.display())
                }
            },
        )?;

        assert!(outcome.report.problems.is_empty());
        Ok(())
    }

    #[test]
    fn user_service_with_no_producer_is_a_legal_graph() -> Result<()> {
        let sources = vec![
            SourceParticipant::user_service(
                "bad".to_string(),
                PathBuf::from("/fake/project/runtimes/bad"),
            ),
            SourceParticipant::user_service(
                "other".to_string(),
                PathBuf::from("/fake/project/runtimes/other"),
            ),
        ];

        let outcome = run_check(
            &[],
            &[],
            &sources,
            |_| bail!("no platform images should be fetched"),
            |_| bail!("no tools should be fetched"),
            |participant| {
                let dir = participant.crate_dir.as_path();
                if dir == Path::new("/fake/project/runtimes/bad") {
                    Ok(raw("bad", "v1", &["drive::Target"]))
                } else if dir == Path::new("/fake/project/runtimes/other") {
                    Ok(raw("other", "v1", &[]))
                } else {
                    bail!("unexpected source dir {}", dir.display())
                }
            },
        )?;

        assert!(outcome.report.problems.is_empty());
        Ok(())
    }

    #[test]
    fn build_emit_apis_from_source_never_caches_across_calls() -> Result<()> {
        // The old `cache/emit-apis/` disk cache is gone: two back-to-back calls
        // for the SAME crate dir each invoke the (fake) build closure - nothing
        // is remembered between calls.
        let temp = tempfile::tempdir()?;
        let crate_dir = fixture_crate_dir(&temp, "sibling");
        let participant = SourceParticipant::user_service("sibling", crate_dir.clone());

        let mut build_count = 0;
        let first = build_emit_apis_from_source_with_diagnostics(
            &participant,
            |_| {
                build_count += 1;
                Ok(raw("sibling", "v1", &[]))
            },
            None,
        )?;
        let second = build_emit_apis_from_source_with_diagnostics(
            &participant,
            |_| {
                build_count += 1;
                Ok(raw("sibling", "v1", &[]))
            },
            None,
        )?;

        assert_eq!(build_count, 2, "every call must rebuild, nothing is cached");
        assert_eq!(first, second);
        Ok(())
    }

    #[test]
    fn component_driver_and_platform_sharing_a_contract_family_is_a_healthy_graph() -> Result<()> {
        // D1: a component driver (subscribing `drive::Target`) and a platform
        // publisher sharing the family is healthy regardless of role. The
        // driver still appears under its concrete instance id (`left_drive`),
        // not the shared driver artifact (`ddsm115`), so multiple instances
        // of one driver stay distinct.
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
                "mission:ok" => Ok(raw_with_role(
                    "mission",
                    "v1",
                    &[("drive::Target", "publish")],
                )),
                unexpected => bail!("unexpected image {unexpected}"),
            },
            |_| bail!("no tools should be fetched"),
            |_| {
                Ok(raw_kind_with_role(
                    "driver",
                    "ddsm115",
                    "v1",
                    &[("drive::Target", "subscribe")],
                    "checked",
                ))
            },
        )?;

        assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
        let participant_ids = outcome
            .checked_participants
            .iter()
            .map(|participant| participant.participant_id.as_str())
            .collect::<Vec<_>>();
        assert!(participant_ids.contains(&"left_drive"));
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
                assets: Some(fixture_component_package(
                    "phoxal/component-ddsm115",
                    crate::catalog::ArtifactKind::ComponentAssets,
                    "components/ddsm115",
                )),
                driver: Some(fixture_component_package(
                    "phoxal/component-ddsm115",
                    crate::catalog::ArtifactKind::ComponentDriver,
                    "components/ddsm115",
                )),
                has_driver: true,
            },
            ResolvedComponent {
                instance: "caster".to_string(),
                source_name: "passive_caster".to_string(),
                assets: Some(fixture_component_package(
                    "phoxal/component-passive_caster",
                    crate::catalog::ArtifactKind::ComponentAssets,
                    "components/passive_caster",
                )),
                driver: None,
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
                Ok(raw_kind("driver", "ddsm115", "v1", &[]))
            },
        )?;

        assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
        assert_eq!(built, vec![temp.path().join("component-crates/left_drive")]);
        Ok(())
    }

    #[test]
    fn catalog_component_driver_becomes_a_platform_ref_not_a_source_participant() -> Result<()> {
        // A Catalog-sourced driver is a first-class catalog artifact - it
        // must NOT enter `source_participants_from_resolved` (which would
        // later bail trying to build/locate a nonexistent local crate dir).
        let temp = tempfile::tempdir()?;
        let resolved = resolved_with_components(vec![ResolvedComponent {
            instance: "left_drive".to_string(),
            source_name: "ddsm115".to_string(),
            assets: Some(fixture_catalog_component_package(
                "phoxal/component-ddsm115",
                crate::catalog::ArtifactKind::ComponentAssets,
                "ddsm115",
            )),
            driver: Some(fixture_catalog_component_package(
                "phoxal/component-ddsm115",
                crate::catalog::ArtifactKind::ComponentDriver,
                "ddsm115",
            )),
            has_driver: true,
        }])?;

        let source_participants = source_participants_from_resolved(
            temp.path(),
            &resolved,
            |component, _project_root| {
                panic!(
                    "a Catalog-sourced driver for '{}' must never reach the source-crate locator",
                    component.instance
                )
            },
        )?;
        assert!(
            source_participants.is_empty(),
            "catalog driver must not become a source participant: {source_participants:?}"
        );

        let platform_refs = component_driver_platform_refs_from_resolved(&resolved);
        assert_eq!(platform_refs.len(), 1);
        assert_eq!(
            platform_refs[0].kind,
            crate::catalog::ArtifactKind::ComponentDriver
        );
        assert_eq!(platform_refs[0].name, "ddsm115");
        assert_eq!(platform_refs[0].instances, vec!["left_drive".to_string()]);

        Ok(())
    }

    #[test]
    fn n_instances_of_one_catalog_driver_fetch_once_and_validate_as_n_graph_participants()
    -> Result<()> {
        // Two instances (`left_drive`/`right_drive`) share one catalog
        // driver package: the fetch closure must be called exactly once
        // (proving the driver is fetched once, not per instance), yet both
        // instances must appear as distinct, correctly-scoped graph
        // participants - exactly like two Path/Git-overridden driver
        // instances already do.
        let temp = tempfile::tempdir()?;
        let resolved = resolved_with_components(vec![
            ResolvedComponent {
                instance: "left_drive".to_string(),
                source_name: "ddsm115".to_string(),
                assets: Some(fixture_catalog_component_package(
                    "phoxal/component-ddsm115",
                    crate::catalog::ArtifactKind::ComponentAssets,
                    "ddsm115",
                )),
                driver: Some(fixture_catalog_component_package(
                    "phoxal/component-ddsm115",
                    crate::catalog::ArtifactKind::ComponentDriver,
                    "ddsm115",
                )),
                has_driver: true,
            },
            ResolvedComponent {
                instance: "right_drive".to_string(),
                source_name: "ddsm115".to_string(),
                assets: Some(fixture_catalog_component_package(
                    "phoxal/component-ddsm115",
                    crate::catalog::ArtifactKind::ComponentAssets,
                    "ddsm115",
                )),
                driver: Some(fixture_catalog_component_package(
                    "phoxal/component-ddsm115",
                    crate::catalog::ArtifactKind::ComponentDriver,
                    "ddsm115",
                )),
                has_driver: true,
            },
        ])?;

        let platform_refs = component_driver_platform_refs_from_resolved(&resolved);
        assert_eq!(
            platform_refs.len(),
            1,
            "one shared package must yield one platform ref, not one per instance"
        );
        let mut instances = platform_refs[0].instances.clone();
        instances.sort();
        assert_eq!(
            instances,
            vec!["left_drive".to_string(), "right_drive".to_string()]
        );

        let source_participants = source_participants_from_resolved(
            temp.path(),
            &resolved,
            |_component, _project_root| panic!("catalog drivers never reach the source locator"),
        )?;
        assert!(source_participants.is_empty());

        let mut fetch_calls = 0;
        let outcome = run_check_with_context(
            &platform_refs,
            &[],
            &source_participants,
            CheckGraphContext {
                manifest_extras: &RobotManifestExtras::default(),
            },
            |artifact_ref| {
                fetch_calls += 1;
                assert_eq!(artifact_ref, "ddsm115-driver-v0.1.0.tar.zst");
                Ok(raw_kind("driver", "ddsm115", "v1", &[]))
            },
            |_| bail!("no tools should be fetched"),
            |_| bail!("no source participants should be built"),
        )?;

        assert_eq!(
            fetch_calls, 1,
            "the shared driver must be fetched exactly once"
        );
        assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
        let mut participant_ids = outcome
            .checked_participants
            .iter()
            .map(|participant| participant.participant_id.clone())
            .collect::<Vec<_>>();
        participant_ids.sort();
        assert_eq!(
            participant_ids,
            vec!["left_drive".to_string(), "right_drive".to_string()],
            "each instance must be a distinct graph node keyed by its own instance id"
        );
        for participant in &outcome.checked_participants {
            assert_eq!(participant.artifact_id, "ddsm115");
            assert!(matches!(
                &participant.scope,
                graph_check::ParticipantScope::ComponentInstance(instance)
                    if *instance == participant.participant_id
            ));
        }

        Ok(())
    }

    #[test]
    fn driverless_catalog_component_stages_assets_only_and_is_not_a_check_participant() -> Result<()>
    {
        // Component assets contribute no contracts and are never a check
        // participant, catalog-sourced or not; a driverless instance yields
        // no source participant and no platform ref.
        let temp = tempfile::tempdir()?;
        let resolved = resolved_with_components(vec![ResolvedComponent {
            instance: "caster".to_string(),
            source_name: "passive_caster".to_string(),
            assets: Some(fixture_catalog_component_package(
                "phoxal/component-passive_caster",
                crate::catalog::ArtifactKind::ComponentAssets,
                "passive_caster",
            )),
            driver: None,
            has_driver: false,
        }])?;

        let source_participants = source_participants_from_resolved(
            temp.path(),
            &resolved,
            |_component, _project_root| panic!("a driverless component has no driver to locate"),
        )?;
        assert!(source_participants.is_empty());
        assert!(component_driver_platform_refs_from_resolved(&resolved).is_empty());

        Ok(())
    }

    #[test]
    fn path_overridden_service_enters_check_through_source_emit_apis() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut resolved = resolved_with_components(Vec::new())?;
        resolved.platform_runtimes.push(ResolvedPlatformRuntime {
            name: "drive".to_string(),
            package: "phoxal/service-drive".to_string(),
            kind: crate::catalog::ArtifactKind::Service,
            version: "0.1.0".to_string(),
            artifact_ref: "path:framework/service/drive".to_string(),
            sha256: None,
            url: None,
            size: None,
            published: true,
            published_triples: Vec::new(),
            path_override: Some(temp.path().join("framework/service/drive")),
            channel: crate::catalog::SelectionChannel::Stable,
            target: Some("aarch64-unknown-linux-gnu".to_string()),
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

        let extras = RobotManifestExtras::default();
        let outcome = run_check_with_context(
            &platform_refs,
            &[],
            &source_participants,
            CheckGraphContext {
                manifest_extras: &extras,
            },
            |_| bail!("path-overridden service should not read catalog metadata"),
            |_| bail!("no tools in this fixture"),
            |participant| {
                assert_eq!(participant.kind, SourceParticipantKind::OfficialService);
                Ok(raw_kind("service", "drive", "v1", &[]))
            },
        )?;
        assert!(outcome.is_ok(), "unexpected outcome: {outcome:?}");
        Ok(())
    }

    #[test]
    fn missing_image_is_reported_after_other_images_are_checked() -> Result<()> {
        let images = vec![
            ("mission".to_string(), "mission:ok".to_string()),
            ("drive".to_string(), "service-drive:v1-stable".to_string()),
        ];

        let outcome = run_check(
            &images,
            &[],
            &[],
            |image_ref| match image_ref {
                "mission:ok" => Ok(raw("mission", "v1", &[])),
                "service-drive:v1-stable" => {
                    Err(MissingImageError::new(anyhow!("not found")).into())
                }
                unexpected => bail!("unexpected image {unexpected}"),
            },
            |_| bail!("no tools should be fetched"),
            |_| bail!("no source services should be built"),
        )?;

        assert_eq!(
            outcome.missing_images,
            vec!["service-drive:v1-stable".to_string()]
        );
        assert!(!outcome.is_ok());
        Ok(())
    }

    #[test]
    fn raw_emit_apis_accepts_required_contracts_json() -> Result<()> {
        let parsed: RawEmitApis = serde_json::from_str(
            r#"{
                "artifact": { "kind": "service", "id": "drive", "ignored": true },
                "api_version": "v1",
                "required_contracts": [
                    {
                        "role": "publish",
                        "version": "v1",
                        "contract": "drive::Target",
                        "external": false
                    }
                ],
                "config_schema": { "type": "object" }
            }"#,
        )?;
        let participant = graph_check::ParticipantApis::try_from(parsed)?;

        assert_eq!(participant.artifact_id, "drive");
        assert_eq!(participant.participant_class, ParticipantClass::Checked);
        assert_eq!(participant.api_version, "v1");
        assert_eq!(
            participant
                .config_schema
                .as_ref()
                .and_then(|schema| schema.get("type"))
                .and_then(Value::as_str),
            Some("object")
        );
        assert_eq!(participant.contracts[0].family, "v1::drive::Target");
        Ok(())
    }

    #[test]
    fn raw_emit_apis_threads_privileged_participant_class() -> Result<()> {
        let parsed: RawEmitApis = serde_json::from_str(
            r#"{
                "artifact": { "kind": "tool", "id": "joypad" },
                "participant_class": "privileged",
                "api_version": "v1",
                "required_contracts": []
            }"#,
        )?;
        let participant = graph_check::ParticipantApis::try_from(parsed)?;

        assert_eq!(participant.participant_class, ParticipantClass::Privileged);
        Ok(())
    }

    #[test]
    fn raw_emit_apis_unknown_participant_class_defaults_to_checked() -> Result<()> {
        let mut raw = raw("drive", "v1", &[]);
        raw.participant_class = "future".to_string();
        let participant = graph_check::ParticipantApis::try_from(raw)?;

        assert_eq!(participant.participant_class, ParticipantClass::Checked);
        Ok(())
    }

    #[test]
    fn user_service_config_is_validated_against_emitted_schema() -> Result<()> {
        let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("api-fixture");
        let sources = vec![SourceParticipant::user_service(
            "avoid".to_string(),
            fixture_dir,
        )];
        let emitted = build_emit_apis_by_building(&sources[0])?;
        assert_eq!(
            emitted.config_schema,
            Some(serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "title": "Config",
                "type": "object",
                "properties": { "gain": { "type": "number", "format": "double" } },
                "required": ["gain"]
            }))
        );

        let check_config = |config: Value| -> Result<CheckOutcome> {
            let extras = RobotManifestExtras {
                user_runtimes: BTreeMap::from([(
                    "avoid".to_string(),
                    crate::resolver::UserRuntimeManifestExtras {
                        config: Some(config),
                    },
                )]),
                ..RobotManifestExtras::default()
            };

            run_check_with_context(
                &[],
                &[],
                &sources,
                CheckGraphContext {
                    manifest_extras: &extras,
                },
                |_| bail!("no platform images should be fetched"),
                |_| bail!("no tools should be fetched"),
                |_| Ok(emitted.clone()),
            )
        };

        let missing = check_config(serde_json::json!({}))?;
        assert!(matches!(
            missing
                .report
                .problems
                .iter()
                .find(|problem| matches!(problem, Problem::InvalidConfig { .. })),
            Some(Problem::InvalidConfig { runtime_id, errors })
                if runtime_id == "avoid"
                    && errors.iter().any(|error| error.contains("gain"))
        ));

        let mistyped = check_config(serde_json::json!({ "gain": "fast" }))?;
        assert!(matches!(
            mistyped
                .report
                .problems
                .iter()
                .find(|problem| matches!(problem, Problem::InvalidConfig { .. })),
            Some(Problem::InvalidConfig { runtime_id, errors })
                if runtime_id == "avoid"
                    && errors.iter().any(|error| error.contains("gain"))
        ));

        let valid = check_config(serde_json::json!({ "gain": 1.5 }))?;
        assert!(
            valid
                .report
                .problems
                .iter()
                .all(|problem| !matches!(problem, Problem::InvalidConfig { .. })),
            "{:?}",
            valid.report.problems
        );
        Ok(())
    }

    #[test]
    fn absent_user_service_config_validates_as_null() -> Result<()> {
        let sources = vec![SourceParticipant::user_service(
            "optional".to_string(),
            PathBuf::from("/fake/project/runtimes/optional"),
        )];
        let extras = RobotManifestExtras::default();

        let outcome = run_check_with_context(
            &[],
            &[],
            &sources,
            CheckGraphContext {
                manifest_extras: &extras,
            },
            |_| bail!("no platform images should be fetched"),
            |_| bail!("no tools should be fetched"),
            |_| {
                let mut raw = raw("optional", "v1", &[]);
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

        let outcome = run_check_with_context(
            &[],
            &[],
            &sources,
            CheckGraphContext {
                manifest_extras: &extras,
            },
            |_| bail!("no platform images should be fetched"),
            |_| bail!("no tools should be fetched"),
            |_| {
                let mut raw = raw("required", "v1", &[]);
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

        let outcome = run_check_with_context(
            &[],
            &[],
            &sources,
            CheckGraphContext {
                manifest_extras: &extras,
            },
            |_| bail!("no platform images should be fetched"),
            |_| bail!("no tools should be fetched"),
            |_| {
                let mut raw = raw("avoid", "v1", &[]);
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

    /// Writes a marker file so the tempdir hashes distinctly under
    /// `hash_tree` (an empty directory always hashes to the SHA-256 of empty
    /// input, so two unrelated empty fixture crates would otherwise collide
    /// on the same source-tree hash and thus the same cache path).
    fn fixture_crate_dir(temp: &tempfile::TempDir, marker: &str) -> PathBuf {
        std::fs::write(temp.path().join("Cargo.toml"), marker).expect("write fixture marker");
        temp.path().to_path_buf()
    }

    fn raw(id: &str, api_version: &str, contracts: &[&str]) -> RawEmitApis {
        raw_kind("service", id, api_version, contracts)
    }

    /// Like `raw`, but each contract carries an explicit `role` (D1: the
    /// wire-shape-agreement axis `schema_id` used to gate is gone - two
    /// participants naming the same `family` are compatible by construction
    /// regardless of role, so this only exists for tests that want to spell
    /// out a specific publish/subscribe/serve/ask role).
    fn raw_with_role(id: &str, api_version: &str, contracts: &[(&str, &str)]) -> RawEmitApis {
        raw_kind_with_role("service", id, api_version, contracts, "checked")
    }

    fn raw_kind_with_role(
        kind: &str,
        id: &str,
        api_version: &str,
        contracts: &[(&str, &str)],
        participant_class: &str,
    ) -> RawEmitApis {
        RawEmitApis {
            artifact: RawArtifact {
                kind: kind.to_string(),
                id: id.to_string(),
            },
            participant_class: participant_class.to_string(),
            api_version: api_version.to_string(),
            required_contracts: contracts
                .iter()
                .map(
                    |(family, role)| crate::participant_metadata::ParticipantMetaContract {
                        role: (*role).to_string(),
                        version: family
                            .split_once("::")
                            .map_or(api_version, |(version, _)| version)
                            .to_string(),
                        contract: family
                            .split_once("::")
                            .map_or(*family, |(_, contract)| contract)
                            .to_string(),
                        external: false,
                    },
                )
                .collect(),
            config_schema: None,
        }
    }

    fn raw_kind(kind: &str, id: &str, api_version: &str, contracts: &[&str]) -> RawEmitApis {
        raw_kind_class(kind, id, api_version, contracts, "checked")
    }

    fn raw_kind_class(
        kind: &str,
        id: &str,
        api_version: &str,
        contracts: &[&str],
        participant_class: &str,
    ) -> RawEmitApis {
        RawEmitApis {
            artifact: RawArtifact {
                kind: kind.to_string(),
                id: id.to_string(),
            },
            participant_class: participant_class.to_string(),
            api_version: api_version.to_string(),
            required_contracts: contracts
                .iter()
                .map(
                    |family| crate::participant_metadata::ParticipantMetaContract {
                        // A single default role: nothing in these fixtures cares
                        // about role identity (D1: only `family` decides
                        // compatibility), so every contract shares one.
                        role: "publish".to_string(),
                        version: family
                            .split_once("::")
                            .map_or(api_version, |(version, _)| version)
                            .to_string(),
                        contract: family
                            .split_once("::")
                            .map_or(*family, |(_, contract)| contract)
                            .to_string(),
                        external: false,
                    },
                )
                .collect(),
            config_schema: None,
        }
    }

    fn resolved_with_components(components: Vec<ResolvedComponent>) -> Result<ResolvedRobot> {
        Ok(ResolvedRobot {
            robot: Robot::parse_from_string(MINIMAL_ROBOT)?,
            channel: crate::catalog::SelectionChannel::Stable,
            target: crate::resolver::host_target_triple(),
            catalog_snapshot: None,
            platform_runtimes: Vec::new(),
            simulators: Vec::new(),
            user_runtimes: Vec::new(),
            components,
            tools: Vec::new(),
            path_overrides: Vec::new(),
        })
    }

    const MINIMAL_ROBOT: &str = r#"schema: robot/v0
robot:
  id: testbot
  namespace: test
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: differential
    left_actuators: [left_drive.motor]
    right_actuators: [right_drive.motor]
    left_encoders: [left_drive.encoder]
    right_encoders: [right_drive.encoder]
    wheel_radius_m: 0.1
    wheel_base_m: 0.5
  components: {}
artifacts:
  channel: stable
"#;
}
