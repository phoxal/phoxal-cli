use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use serde::Deserialize;

use crate::AppContext;
use crate::catalog::CATALOG;
use crate::commands::check;
use crate::component_driver::component_crate_dir;
use crate::compose::LaunchClock;
use crate::resolver::{
    ResolveOptions, ResolvedRobot, ResolvedUserRuntime, RobotManifestExtras, discover_robot_yaml,
    load_robot_with_extras, resolve,
};
use crate::utils::resolve_project_path;

#[derive(Debug, Args)]
pub struct Deploy {
    #[command(subcommand)]
    pub command: DeploySubcommand,
}

#[derive(Debug, Subcommand)]
pub enum DeploySubcommand {
    #[command(about = "Build a digest-pinned deployment artifact.")]
    Build(Build),
}

#[derive(Debug, Args)]
pub struct Build {
    #[arg(long, value_enum, default_value_t = DeployTarget::Compose)]
    pub target: DeployTarget,
    #[arg(long, value_name = "PATH")]
    pub output: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DeployTarget {
    Compose,
    Balena,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildOptions {
    pub target: DeployTarget,
    pub output: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildSummary {
    pub output_path: PathBuf,
    pub bundle_dir: PathBuf,
    pub platform_runtime_count: usize,
    pub user_runtime_count: usize,
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
            output: self.output.clone(),
        };
        let summary = tokio::task::spawn_blocking(move || run(&project_root, options))
            .await
            .context("deploy build worker failed")??;

        println!("wrote deployment bundle: {}", summary.bundle_dir.display());
        println!("wrote compose artifact: {}", summary.output_path.display());
        Ok(())
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
    let loaded = load_robot_with_extras(&robot_path)?;
    let resolved = resolve(
        &loaded.robot,
        project_root,
        &CATALOG,
        ResolveOptions {
            resolve_external_artifacts: true,
            ..ResolveOptions::default()
        },
    )?;

    run_pinned_graph_check(project_root, &resolved)?;
    let user_runtime_images =
        resolve_production_user_runtime_images(&resolved, &loaded.extras, |image_ref| {
            crate::resolver::resolve_image_digest(image_ref)
        })?;

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
    ensure_all_compose_image_refs_are_digest_pinned(&compose)?;

    crate::run_view::assemble(project_root, &resolved, &bundle_dir)?;
    fs::create_dir_all(&output_dir)
        .with_context(|| format!("failed to create {}", output_dir.display()))?;
    fs::write(&output_path, compose)
        .with_context(|| format!("failed to write {}", output_path.display()))?;

    Ok(BuildSummary {
        output_path,
        bundle_dir,
        platform_runtime_count: resolved.platform_runtimes.len(),
        user_runtime_count: resolved.user_runtimes.len(),
    })
}

fn run_pinned_graph_check(project_root: &Path, resolved: &ResolvedRobot) -> Result<()> {
    let platform_refs = resolved
        .platform_runtimes
        .iter()
        .map(|runtime| (runtime.name.clone(), runtime.deploy_ref()))
        .collect::<Vec<_>>();
    let source_participants =
        check::source_participants_from_resolved(project_root, resolved, component_crate_dir)?;
    let outcome = check::run_check(
        &platform_refs,
        &source_participants,
        &resolved.api_version,
        &resolved.channel.to_string(),
        check::fetch_emit_apis_from_docker,
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
    image_ref.starts_with("phoxal-local/")
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

pub(crate) fn ensure_all_compose_image_refs_are_digest_pinned(compose: &str) -> Result<()> {
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
        if !is_digest_pinned_image_ref(&service.image) {
            bail!(
                "deployment artifact contains unpinned image ref for service {service_name}: {}",
                service.image
            );
        }
    }
    Ok(())
}

fn is_digest_pinned_image_ref(image_ref: &str) -> bool {
    image_ref
        .rsplit_once('@')
        .is_some_and(|(_, digest)| digest.starts_with("sha256:"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal::model::robot::RobotV1 as Robot;
    use phoxal::model::robot::v1::Channel;

    use crate::resolver::{ResolvedPlatformRuntime, UserRuntimeManifestExtras};

    #[test]
    fn compose_image_assertion_requires_every_service_to_use_digest() -> Result<()> {
        let valid = r#"
services:
  router:
    image: eclipse/zenoh:1.9.0@sha256:router
  drive:
    image: ghcr.io/phoxal/runtime-drive:y2026_1-stable@sha256:drive
"#;
        ensure_all_compose_image_refs_are_digest_pinned(valid)?;

        let invalid = r#"
services:
  drive:
    image: ghcr.io/phoxal/runtime-drive:y2026_1-stable
"#;
        let error = ensure_all_compose_image_refs_are_digest_pinned(invalid)
            .expect_err("floating tags should fail deploy artifact validation");
        assert!(
            error
                .to_string()
                .contains("deployment artifact contains unpinned image ref for service drive"),
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
                "phoxal-local/testbot/user-runtime/avoid:abc",
                Ok("ghcr.io/acme/avoid@sha256:pinned"),
            ),
            (
                "manifest tag",
                Some("ghcr.io/acme/avoid:v1"),
                "phoxal-local/testbot/user-runtime/avoid:abc",
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
                "phoxal-local/testbot/user-runtime/avoid:abc",
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
