use std::{
    fmt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use serde::Deserialize;

use crate::AppContext;
use crate::catalog::CATALOG;
use crate::check as graph_check;
use crate::component_driver::component_crate_dir;
use crate::resolver::{
    ResolveOptions, ResolvedComponent, ResolvedRobot, discover_robot_yaml, load_robot, resolve,
};
use crate::utils::{cargo_binary_name, resolve_project_path};

#[derive(Debug, Args)]
pub struct CheckCmd;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RawEmitApis {
    pub artifact: RawArtifact,
    pub api_version: String,
    #[serde(alias = "contracts")]
    pub required_contracts: Vec<RawContract>,
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
        let result = tokio::task::spawn_blocking(move || run(&project_root))
            .await
            .context("check worker failed")??;

        if !result.outcome.missing_images.is_empty() {
            bail!(
                "{}",
                format_missing_images_error(
                    &result.api_version,
                    &result.channel,
                    &result.outcome.missing_images
                )
            );
        }

        if !result.outcome.report.is_ok() {
            bail!("{}", format_report_error(&result.outcome.report));
        }

        println!(
            "ok: {} participants validated against api_version {} (channel {})",
            result.participant_count, result.api_version, result.channel
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CheckRunResult {
    api_version: String,
    channel: String,
    participant_count: usize,
    outcome: CheckOutcome,
}

fn run(project_start: &std::path::Path) -> Result<CheckRunResult> {
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
    let source_participants =
        source_participants_from_resolved(project_root, &resolved, component_crate_dir)?;
    let participant_count = platform_refs.len() + source_participants.len();
    let outcome = run_check(
        &platform_refs,
        &source_participants,
        &resolved.api_version,
        &resolved.channel.to_string(),
        fetch_emit_apis_from_docker,
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

fn source_participants_from_resolved(
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

pub fn run_check(
    resolved_platform_image_refs: &[(String, String)],
    source_participants: &[SourceParticipant],
    root_api: &str,
    _channel: &str,
    mut fetch: impl FnMut(&str) -> Result<RawEmitApis>,
    mut build: impl FnMut(&Path) -> Result<RawEmitApis>,
) -> Result<CheckOutcome> {
    let mut missing_images = Vec::new();
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
        let participant = graph_check::ParticipantApis::try_from(raw).with_context(|| {
            format!("failed to interpret emit-apis for runtime {runtime_name} ({image_ref})")
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
        report,
    })
}

fn fetch_emit_apis_from_docker(image_ref: &str) -> Result<RawEmitApis> {
    let output = crate::shell::run_stdout("docker", ["run", "--rm", image_ref, "emit-apis"], None)
        .map_err(MissingImageError::new)?;
    serde_json::from_str(&output)
        .with_context(|| format!("docker emit-apis output for {image_ref} was not valid JSON"))
}

fn build_emit_apis_from_source(dir: &Path) -> Result<RawEmitApis> {
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
            contracts,
        })
    }
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
            &sources,
            "y2026_1",
            "stable",
            |image_ref| match image_ref {
                "mission:ok" => Ok(raw(
                    "mission",
                    "y2026_1",
                    &[("drive::Target", "drive/target", "publish")],
                )),
                unexpected => bail!("unexpected image {unexpected}"),
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
    fn healthy_graph_passes_with_platform_and_component_driver_source() -> Result<()> {
        let images = vec![("mission".to_string(), "mission:ok".to_string())];
        let sources = vec![SourceParticipant::component_driver(
            "left_drive".to_string(),
            PathBuf::from("/fake/project/components/ddsm115"),
        )];

        let outcome = run_check(
            &images,
            &sources,
            "y2026_1",
            "stable",
            |image_ref| match image_ref {
                "mission:ok" => Ok(raw(
                    "mission",
                    "y2026_1",
                    &[("drive::Target", "drive/target", "publish")],
                )),
                unexpected => bail!("unexpected image {unexpected}"),
            },
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
    fn source_wrong_api_version_fails_with_mismatch_problem() -> Result<()> {
        let sources = vec![SourceParticipant::user_runtime(
            "drive".to_string(),
            PathBuf::from("/fake/project/runtimes/drive"),
        )];

        let outcome = run_check(
            &[],
            &sources,
            "y2026_1",
            "stable",
            |_| bail!("no platform images should be fetched"),
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
            &sources,
            "y2026_1",
            "stable",
            |_| bail!("no platform images should be fetched"),
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
            &sources,
            "y2026_1",
            "stable",
            |_| bail!("no platform images should be fetched"),
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
            &sources,
            "y2026_1",
            "stable",
            |_| bail!("no platform images should be fetched"),
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
            &source_participants,
            "y2026_1",
            "stable",
            |_| bail!("no platform images should be fetched"),
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
                "ghcr.io/phoxal/runtime-drive:y2026_2-stable".to_string(),
            ),
        ];

        let outcome = run_check(
            &images,
            &[],
            "y2026_2",
            "stable",
            |image_ref| match image_ref {
                "mission:ok" => Ok(raw("mission", "y2026_2", &[])),
                "ghcr.io/phoxal/runtime-drive:y2026_2-stable" => {
                    Err(MissingImageError::new(anyhow!("not found")).into())
                }
                unexpected => bail!("unexpected image {unexpected}"),
            },
            |_| bail!("no source runtimes should be built"),
        )?;

        assert_eq!(
            outcome.missing_images,
            vec!["ghcr.io/phoxal/runtime-drive:y2026_2-stable".to_string()]
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
                "required_contracts": [
                    {
                        "family": "drive::Target",
                        "topic": "drive/target",
                        "direction": "subscribe",
                        "ignored": true
                    }
                ]
            }"#,
        )?;
        let participant = graph_check::ParticipantApis::try_from(parsed)?;

        assert_eq!(participant.artifact_id, "drive");
        assert_eq!(participant.api_version, "y2026_1");
        assert_eq!(participant.contracts[0].direction, Direction::Subscribe);
        Ok(())
    }

    fn raw(id: &str, api_version: &str, contracts: &[(&str, &str, &str)]) -> RawEmitApis {
        RawEmitApis {
            artifact: RawArtifact { id: id.to_string() },
            api_version: api_version.to_string(),
            required_contracts: contracts
                .iter()
                .map(|(family, topic, direction)| RawContract {
                    family: (*family).to_string(),
                    topic: (*topic).to_string(),
                    direction: (*direction).to_string(),
                })
                .collect(),
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
