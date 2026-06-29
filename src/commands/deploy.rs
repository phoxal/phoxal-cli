use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

use crate::AppContext;
use crate::catalog::CATALOG;
use crate::commands::MessageFormat;
use crate::commands::check;
use crate::component_driver::component_crate_dir;
use crate::compose::LaunchClock;
use crate::resolver::{
    ResolveOptions, ResolvedRobot, ResolvedUserRuntime, RobotManifestExtras, discover_robot_yaml,
    load_robot_with_extras_and_overlays, resolve,
};
use crate::utils::resolve_project_path;

#[derive(Debug, Args)]
pub struct Deploy {
    #[command(subcommand)]
    pub command: DeploySubcommand,
}

#[derive(Debug, Subcommand)]
pub enum DeploySubcommand {
    #[command(about = "Build an immutable deployment artifact.")]
    Build(Build),
}

#[derive(Debug, Args)]
pub struct Build {
    #[arg(long, value_enum, default_value_t = DeployTarget::Compose)]
    pub target: DeployTarget,
    #[arg(
        long,
        value_name = "ENV",
        help = "Apply robot.<env>.yaml overlay before building the deployment artifact."
    )]
    pub env: Vec<String>,
    #[arg(long, value_name = "PATH")]
    pub output: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = MessageFormat::Human)]
    pub message_format: MessageFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum DeployTarget {
    Compose,
    #[value(hide = true)]
    Balena,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildOptions {
    pub target: DeployTarget,
    pub env: Vec<String>,
    pub output: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuildSummary {
    pub output_path: PathBuf,
    pub bundle_dir: PathBuf,
    pub metadata_path: PathBuf,
    pub platform_runtime_count: usize,
    pub user_runtime_count: usize,
    pub applied_overlays: Vec<String>,
}

impl Deploy {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        match &self.command {
            DeploySubcommand::Build(command) => command.run(app).await,
        }
    }
}

impl Build {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let project_root = app.project.root().to_path_buf();
        let options = BuildOptions {
            target: self.target,
            env: self.env.clone(),
            output: self.output.clone(),
        };
        let summary = tokio::task::spawn_blocking(move || run(&project_root, options))
            .await
            .context("deploy build worker failed")??;

        crate::commands::print_message(
            &summary,
            || {
                if !summary.applied_overlays.is_empty() {
                    println!("applied overlays: {}", summary.applied_overlays.join(", "));
                }
                println!("wrote deployment bundle: {}", summary.bundle_dir.display());
                println!("wrote deploy metadata: {}", summary.metadata_path.display());
                println!("wrote compose artifact: {}", summary.output_path.display());
                Ok(())
            },
            self.message_format,
        )
    }
}

pub fn run(project_start: &Path, options: BuildOptions) -> Result<BuildSummary> {
    if options.target == DeployTarget::Balena {
        bail!("deploy build --target balena is not yet supported");
    }

    let robot_path = discover_robot_yaml(project_start)
        .with_context(|| format!("failed to find robot.yaml from {}", project_start.display()))?;
    let project_root = robot_path
        .parent()
        .context("robot.yaml did not have a parent directory")?;
    let loaded = load_robot_with_extras_and_overlays(&robot_path, &options.env)?;
    let resolved = resolve(
        &loaded.robot,
        project_root,
        &CATALOG,
        ResolveOptions {
            resolve_external_artifacts: true,
            ..ResolveOptions::default()
        },
    )?;

    let user_runtime_images = match options.target {
        DeployTarget::Compose => {
            crate::local_build::build_user_runtimes_immutable(project_root, &resolved)?
        }
        DeployTarget::Balena => {
            resolve_production_user_runtime_images(&resolved, &loaded.extras, |image_ref| {
                crate::resolver::resolve_image_digest(image_ref)
            })?
        }
    };
    run_pinned_graph_check(
        project_root,
        &resolved,
        &loaded.extras,
        &user_runtime_images,
    )?;

    let output_path = options.output.map_or_else(
        || {
            project_root
                .join(".phoxal")
                .join("deploy")
                .join("compose.yaml")
        },
        |path| resolve_project_path(project_root, &path),
    );
    let output_dir = output_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| project_root.to_path_buf());
    let bundle_dir = output_dir.join("bundle");

    let compose = crate::compose::generate(
        &resolved,
        &CATALOG,
        &bundle_dir,
        &user_runtime_images,
        &[],
        &loaded.extras,
        LaunchClock::Real,
    )?;
    ensure_all_compose_image_refs_are_immutable(&compose)?;

    crate::run_view::assemble(project_root, &resolved, &bundle_dir)?;
    let metadata_path = bundle_dir.join("deploy-metadata.json");
    fs::create_dir_all(&output_dir)
        .with_context(|| format!("failed to create {}", output_dir.display()))?;
    write_deploy_metadata(&metadata_path, &robot_path, &options.env)?;
    fs::write(&output_path, compose)
        .with_context(|| format!("failed to write {}", output_path.display()))?;

    Ok(BuildSummary {
        output_path,
        bundle_dir,
        metadata_path,
        platform_runtime_count: resolved.platform_runtimes.len(),
        user_runtime_count: resolved.user_runtimes.len(),
        applied_overlays: options.env,
    })
}

fn run_pinned_graph_check(
    project_root: &Path,
    resolved: &ResolvedRobot,
    manifest_extras: &RobotManifestExtras,
    user_runtime_images: &BTreeMap<String, String>,
) -> Result<()> {
    let platform_refs = resolved
        .platform_runtimes
        .iter()
        .map(|runtime| (runtime.name.clone(), runtime.deploy_ref()))
        .collect::<Vec<_>>();
    let tool_names = resolved
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    crate::tool_provisioning::ensure_tool_binaries(&crate::Ui::new(), resolved, tool_names)?;
    let tool_participants = check::tool_participants_from_resolved(resolved)?;
    let source_participants =
        check::source_participants_from_resolved(project_root, resolved, component_crate_dir)?
            .into_iter()
            .filter(|participant| participant.kind == check::SourceParticipantKind::ComponentDriver)
            .collect::<Vec<_>>();
    let user_runtime_image_participants = resolved
        .user_runtimes
        .iter()
        .map(|runtime| {
            let image_ref = user_runtime_images.get(&runtime.name).with_context(|| {
                format!("missing built image ref for user runtime {}", runtime.name)
            })?;
            Ok(check::UserRuntimeImageParticipant {
                name: runtime.name.clone(),
                image_ref: image_ref.clone(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let robot_graph = check::robot_graph_from_resolved(resolved);
    let outcome = check::run_check_with_deployed_user_runtime_images(
        check::CheckParticipants {
            platform_image_refs: &platform_refs,
            user_runtime_images: &user_runtime_image_participants,
            tool_participants: &tool_participants,
            source_participants: &source_participants,
        },
        check::CheckGraphContext {
            root_api: &resolved.api_version,
            robot_graph: &robot_graph,
            manifest_extras,
        },
        check::fetch_emit_apis_from_docker,
        check::fetch_emit_apis_from_tool,
        check::build_emit_apis_from_source,
    )?;
    check::ensure_check_outcome_ok(
        &resolved.api_version,
        &resolved.channel.to_string(),
        &outcome,
    )
}

pub(crate) fn resolve_production_user_runtime_images(
    resolved: &ResolvedRobot,
    manifest_extras: &RobotManifestExtras,
    mut resolve_digest: impl FnMut(&str) -> Result<String>,
) -> Result<BTreeMap<String, String>> {
    let mut images = BTreeMap::new();
    for runtime in &resolved.user_runtimes {
        let image_ref = production_user_runtime_image_ref(runtime, manifest_extras)?;
        let pinned = pin_image_ref(image_ref, &mut resolve_digest)
            .with_context(|| format!("failed to pin image for user runtime {}", runtime.name))?;
        images.insert(runtime.name.clone(), pinned);
    }
    Ok(images)
}

fn production_user_runtime_image_ref<'a>(
    runtime: &'a ResolvedUserRuntime,
    manifest_extras: &'a RobotManifestExtras,
) -> Result<&'a str> {
    if let Some(image) = manifest_extras.user_runtime_image(&runtime.name) {
        return Ok(image);
    }
    if !is_local_user_runtime_image(&runtime.image) {
        return Ok(&runtime.image);
    }
    bail!(
        "production deploy needs a pullable image for user runtime {}",
        runtime.name
    )
}

fn is_local_user_runtime_image(image_ref: &str) -> bool {
    image_ref.starts_with("phoxal.local/")
}

pub(crate) fn pin_image_ref(
    image_ref: &str,
    mut resolve_digest: impl FnMut(&str) -> Result<String>,
) -> Result<String> {
    if is_digest_pinned_image_ref(image_ref) {
        return Ok(image_ref.to_string());
    }
    let digest = resolve_digest(image_ref)?;
    Ok(format!("{image_ref}@{digest}"))
}

pub(crate) fn ensure_all_compose_image_refs_are_immutable(compose: &str) -> Result<()> {
    #[derive(Deserialize)]
    struct ComposeImages {
        services: BTreeMap<String, ComposeImageService>,
    }

    #[derive(Deserialize)]
    struct ComposeImageService {
        image: String,
    }

    let compose: ComposeImages =
        serde_yaml::from_str(compose).context("failed to parse generated compose artifact")?;
    for (service_name, service) in compose.services {
        if !is_immutable_image_ref(&service.image) {
            bail!(
                "deployment artifact contains mutable image ref for service {service_name}: {}",
                service.image
            );
        }
    }
    Ok(())
}

fn is_immutable_image_ref(image_ref: &str) -> bool {
    is_digest_pinned_image_ref(image_ref) || crate::local_build::is_sha256_image_id(image_ref)
}

fn is_digest_pinned_image_ref(image_ref: &str) -> bool {
    image_ref
        .rsplit_once('@')
        .is_some_and(|(_, digest)| digest.starts_with("sha256:"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct DeployMetadata {
    base_robot: HashedInput,
    overlays: Vec<OverlayMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct HashedInput {
    path: PathBuf,
    sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct OverlayMetadata {
    name: String,
    path: PathBuf,
    sha256: String,
}

fn write_deploy_metadata(path: &Path, robot_path: &Path, overlays: &[String]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let metadata = DeployMetadata {
        base_robot: HashedInput {
            path: robot_path.to_path_buf(),
            sha256: sha256_file(robot_path)?,
        },
        overlays: overlays
            .iter()
            .map(|name| {
                let overlay_path = robot_path.with_file_name(format!("robot.{name}.yaml"));
                Ok(OverlayMetadata {
                    name: name.clone(),
                    path: overlay_path.clone(),
                    sha256: sha256_file(&overlay_path)?,
                })
            })
            .collect::<Result<Vec<_>>>()?,
    };
    fs::write(path, serde_json::to_string_pretty(&metadata)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};

    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal::model::robot::RobotV1 as Robot;
    use phoxal::model::robot::v1::Channel;

    use crate::resolver::{ResolvedPlatformRuntime, UserRuntimeManifestExtras};

    #[test]
    fn compose_image_assertion_requires_every_service_to_use_immutable_ref() -> Result<()> {
        let valid = r#"
services:
  router:
    image: eclipse/zenoh:1.9.0@sha256:router
  drive:
    image: ghcr.io/phoxal/runtime-drive:y2026_1-stable@sha256:drive
  user-avoid:
    image: sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
"#;
        ensure_all_compose_image_refs_are_immutable(valid)?;

        let invalid = r#"
services:
  drive:
    image: ghcr.io/phoxal/runtime-drive:y2026_1-stable
"#;
        let error = ensure_all_compose_image_refs_are_immutable(invalid)
            .expect_err("floating tags should fail deploy artifact validation");
        assert!(
            error
                .to_string()
                .contains("deployment artifact contains mutable image ref for service drive"),
            "{error:#}"
        );

        Ok(())
    }

    #[test]
    fn pin_image_ref_leaves_digest_refs_and_resolves_tags() -> Result<()> {
        let digest = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert_eq!(
            pin_image_ref("ghcr.io/acme/avoid@sha256:already", |_| {
                bail!("already pinned refs should not resolve")
            })?,
            "ghcr.io/acme/avoid@sha256:already"
        );
        assert_eq!(
            pin_image_ref("ghcr.io/acme/avoid:v1", |image_ref| {
                assert_eq!(image_ref, "ghcr.io/acme/avoid:v1");
                Ok(digest.to_string())
            })?,
            format!("ghcr.io/acme/avoid:v1@{digest}")
        );
        Ok(())
    }

    #[test]
    fn production_user_runtime_images_require_pullable_refs_and_pin_them() -> Result<()> {
        let digest = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let cases = [
            (
                "manifest digest",
                Some("ghcr.io/acme/avoid@sha256:pinned"),
                "phoxal.local/testbot/user-runtime/avoid:dev",
                Ok("ghcr.io/acme/avoid@sha256:pinned"),
            ),
            (
                "manifest tag",
                Some("ghcr.io/acme/avoid:v1"),
                "phoxal.local/testbot/user-runtime/avoid:dev",
                Ok(
                    "ghcr.io/acme/avoid:v1@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                ),
            ),
            (
                "future resolved image",
                None,
                "ghcr.io/acme/avoid@sha256:pinned",
                Ok("ghcr.io/acme/avoid@sha256:pinned"),
            ),
            (
                "local sim image only",
                None,
                "phoxal.local/testbot/user-runtime/avoid:dev",
                Err("production deploy needs a pullable image for user runtime avoid"),
            ),
        ];

        for (name, manifest_image, resolved_image, expected) in cases {
            let resolved = resolved_robot(resolved_image)?;
            let extras = manifest_extras(manifest_image);
            let result =
                resolve_production_user_runtime_images(&resolved, &extras, |_| Ok(digest.into()));

            match expected {
                Ok(expected_image) => {
                    let images = result.with_context(|| format!("case {name} failed"))?;
                    assert_eq!(
                        images.get("avoid").map(String::as_str),
                        Some(expected_image)
                    );
                }
                Err(expected_message) => {
                    let error = result.expect_err("case should fail");
                    assert!(
                        error.to_string().contains(expected_message),
                        "case {name}: {error:#}"
                    );
                }
            }
        }

        Ok(())
    }

    fn resolved_robot(user_image: &str) -> Result<ResolvedRobot> {
        Ok(ResolvedRobot {
            robot: Robot::parse_from_string(MINIMAL_ROBOT)?,
            api_version: "y2026_1".to_string(),
            channel: Channel::Stable,
            platform_runtimes: vec![ResolvedPlatformRuntime {
                name: "asset".to_string(),
                image_ref: "ghcr.io/phoxal/runtime-asset:y2026_1-stable".to_string(),
                pin: crate::resolver::ImagePin::Digest(
                    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                        .to_string(),
                ),
            }],
            user_runtimes: vec![ResolvedUserRuntime {
                name: "avoid".to_string(),
                path: PathBuf::from("runtimes/avoid"),
                framework: "y2026_1".to_string(),
                build: None,
                source_hash: "abc123".to_string(),
                image: user_image.to_string(),
            }],
            components: Vec::new(),
            tools: Vec::new(),
        })
    }

    fn manifest_extras(image: Option<&str>) -> RobotManifestExtras {
        RobotManifestExtras {
            user_runtimes: image
                .map(|image| {
                    BTreeMap::from([(
                        "avoid".to_string(),
                        UserRuntimeManifestExtras {
                            image: Some(image.to_string()),
                            config: None,
                        },
                    )])
                })
                .unwrap_or_default(),
        }
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
