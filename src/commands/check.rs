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
use crate::resolver::{ResolveOptions, discover_robot_yaml, load_robot, resolve};
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
    let source_runtimes = resolved
        .user_runtimes
        .iter()
        .map(|runtime| {
            (
                runtime.name.clone(),
                resolve_project_path(project_root, &runtime.path),
            )
        })
        .collect::<Vec<_>>();
    // Component-driver source checks are next; they need git clone plus component crate layout support.
    let participant_count = platform_refs.len() + source_runtimes.len();
    let outcome = run_check(
        &platform_refs,
        &source_runtimes,
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

pub fn run_check(
    resolved_platform_image_refs: &[(String, String)],
    source_runtimes: &[(String, PathBuf)],
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

    for (runtime_name, crate_dir) in source_runtimes {
        let raw = build(crate_dir).with_context(|| {
            format!(
                "failed to obtain emit-apis for user runtime {runtime_name} ({})",
                crate_dir.display()
            )
        })?;
        let participant = graph_check::ParticipantApis::try_from(raw).with_context(|| {
            format!(
                "failed to interpret emit-apis for user runtime {runtime_name} ({})",
                crate_dir.display()
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
    let crate_dir = dir.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize user runtime source {}",
            dir.display()
        )
    })?;
    crate::shell::run_status("cargo", ["build"], Some(&crate_dir))
        .with_context(|| format!("cargo build failed in {}", crate_dir.display()))?;
    let binary_name = cargo_binary_name(&crate_dir, None)?;
    let binary_path = crate_dir
        .join("target")
        .join("debug")
        .join(format!("{binary_name}{}", std::env::consts::EXE_SUFFIX));
    let executable = binary_path
        .to_str()
        .ok_or_else(|| anyhow!("binary path is not UTF-8: {}", binary_path.display()))?;
    let output = crate::shell::run_stdout(executable, ["emit-apis"], Some(&crate_dir))
        .with_context(|| format!("failed to run {} emit-apis", binary_path.display()))?;
    serde_json::from_str(&output).with_context(|| {
        format!(
            "emit-apis output from user runtime source {} was not valid JSON",
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
            format!("runtime {artifact_id} reports api_version {found}, expected {expected}")
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
    use graph_check::{Direction, Problem};

    #[test]
    fn healthy_graph_passes_with_fake_emit_apis() -> Result<()> {
        let images = vec![("mission".to_string(), "mission:ok".to_string())];
        let sources = vec![(
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
    fn source_wrong_api_version_fails_with_mismatch_problem() -> Result<()> {
        let sources = vec![(
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
    fn source_build_error_is_a_hard_error() {
        let sources = vec![(
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
}
