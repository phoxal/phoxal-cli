use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::component_driver::component_crate_dir;
use crate::resolver::{ResolvedRobot, ResolvedUserRuntimeBuild};
use crate::shell;
use crate::utils::resolve_project_path;

pub(crate) fn ensure_platform_images(
    app: &crate::AppContext,
    resolved: &ResolvedRobot,
    refresh: bool,
) -> Result<()> {
    let refs = resolved
        .platform_runtimes
        .iter()
        .map(|runtime| (runtime.name.clone(), runtime.deploy_ref()))
        .collect::<Vec<_>>();
    for (runtime_name, image) in refs {
        if !refresh && image_exists_locally(&image)? {
            continue;
        }
        app.ui.info(format!("pulling {image}"));
        pull_platform_image_refs(&[(runtime_name, image)])?;
    }
    Ok(())
}

pub(crate) fn pull_platform_image_refs(runtime_refs: &[(String, String)]) -> Result<()> {
    for (runtime_name, image) in runtime_refs {
        // `deploy_ref` is a real digest pin or an honest tag ref — never a
        // fabricated `sha256:` — so this can't attempt to pull a fake digest.
        shell::run_status("docker", ["pull", image.as_str()], None).with_context(|| {
            if image.contains("@sha256:") {
                format!("failed to pull pinned runtime image {image}")
            } else {
                format!(
                    "failed to pull runtime image {runtime_name} ({image}) by tag. The phoxal/framework GHCR \
                     runtime images may not be published for this API/channel yet. Publish the \
                     runtime images, then run `phoxal-cli update --pin-digests` to pin real \
                     digests."
                )
            }
        })?;
    }
    Ok(())
}

fn image_exists_locally(image_ref: &str) -> Result<bool> {
    let output = shell::run_output(
        "docker",
        ["image", "inspect", image_ref, "--format", "{{.Id}}"],
        None,
    )
    .with_context(|| format!("failed to inspect local image {image_ref}"))?;
    Ok(output.status.success())
}

pub(crate) fn build_user_runtimes(
    project_root: &Path,
    resolved: &ResolvedRobot,
) -> Result<BTreeMap<String, String>> {
    build_user_runtimes_with_ref_mode(project_root, resolved, UserRuntimeImageRefMode::Tag)
}

pub(crate) fn build_user_runtimes_immutable(
    project_root: &Path,
    resolved: &ResolvedRobot,
) -> Result<BTreeMap<String, String>> {
    build_user_runtimes_with_ref_mode(project_root, resolved, UserRuntimeImageRefMode::Immutable)
}

#[derive(Debug, Clone, Copy)]
enum UserRuntimeImageRefMode {
    Tag,
    Immutable,
}

fn build_user_runtimes_with_ref_mode(
    project_root: &Path,
    resolved: &ResolvedRobot,
    mode: UserRuntimeImageRefMode,
) -> Result<BTreeMap<String, String>> {
    let mut images = BTreeMap::new();
    for runtime in &resolved.user_runtimes {
        let runtime_dir = resolve_project_path(project_root, &runtime.path);
        shell::run_status(
            "docker",
            docker_build_args(&runtime.image, runtime.build.as_ref()),
            Some(&runtime_dir),
        )?;
        let image_ref = match mode {
            UserRuntimeImageRefMode::Tag => runtime.image.clone(),
            UserRuntimeImageRefMode::Immutable => immutable_local_image_ref(&runtime.image)
                .with_context(|| {
                    format!(
                        "failed to resolve immutable image id for user runtime {}",
                        runtime.name
                    )
                })?,
        };
        images.insert(runtime.name.clone(), image_ref);
    }
    Ok(images)
}

fn immutable_local_image_ref(image_ref: &str) -> Result<String> {
    let output = shell::run_stdout(
        "docker",
        ["image", "inspect", image_ref, "--format", "{{.Id}}"],
        None,
    )
    .with_context(|| format!("failed to inspect built image {image_ref}"))?;
    let image_id = output.trim();
    if !is_sha256_image_id(image_id) {
        bail!("docker reported invalid image id for {image_ref}: {image_id}");
    }
    Ok(image_id.to_string())
}

pub(crate) fn is_sha256_image_id(image_ref: &str) -> bool {
    image_ref
        .strip_prefix("sha256:")
        .is_some_and(|digest| digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()))
}

pub(crate) fn docker_build_args(
    image: &str,
    build: Option<&ResolvedUserRuntimeBuild>,
) -> Vec<OsString> {
    let context = build
        .map(|build| build.context.clone())
        .unwrap_or_else(|| PathBuf::from("."));
    let mut args = vec![
        OsString::from("build"),
        OsString::from("-t"),
        OsString::from(image),
    ];
    if let Some(build) = build {
        if let Some(dockerfile) = &build.dockerfile {
            args.push(OsString::from("-f"));
            args.push(build.context.join(dockerfile).into_os_string());
        }
        if let Some(target) = &build.target {
            args.push(OsString::from("--target"));
            args.push(OsString::from(target));
        }
    }
    args.push(context.into_os_string());
    args
}

pub(crate) fn build_component_drivers(project_root: &Path, resolved: &ResolvedRobot) -> Result<()> {
    for component in &resolved.components {
        if !component.has_driver {
            continue;
        }
        let driver_dir = component_crate_dir(component, project_root).with_context(|| {
            format!(
                "failed to locate component driver {} source",
                component.instance
            )
        })?;
        shell::run_status("cargo", ["build", "--release"], Some(&driver_dir)).with_context(
            || {
                format!(
                    "failed to build component driver {} in {}",
                    component.instance,
                    driver_dir.display()
                )
            },
        )?;
    }
    Ok(())
}

pub(crate) fn collect_files_under(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_absolute_files(root, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_absolute_files(path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_absolute_files(&path, files)?;
        } else if path.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_build_args_use_default_context_without_recipe() {
        assert_eq!(
            docker_build_args("phoxal.local/test/user-runtime/drive:dev", None),
            os_args([
                "build",
                "-t",
                "phoxal.local/test/user-runtime/drive:dev",
                "."
            ])
        );
    }

    #[test]
    fn docker_build_args_apply_recipe_context_dockerfile_and_target() {
        let build = ResolvedUserRuntimeBuild {
            context: PathBuf::from("container"),
            dockerfile: Some(PathBuf::from("Dockerfile.runtime")),
            target: Some("runtime".to_string()),
        };

        assert_eq!(
            docker_build_args("phoxal.local/test/user-runtime/drive:dev", Some(&build)),
            os_args([
                "build",
                "-t",
                "phoxal.local/test/user-runtime/drive:dev",
                "-f",
                "container/Dockerfile.runtime",
                "--target",
                "runtime",
                "container",
            ])
        );
    }

    #[test]
    fn sha256_image_ids_are_recognized_as_immutable_refs() {
        assert!(is_sha256_image_id(
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        ));
        assert!(!is_sha256_image_id("phoxal.local/test/runtime:dev"));
        assert!(!is_sha256_image_id("sha256:not-a-real-image-id"));
    }

    fn os_args<const N: usize>(values: [&str; N]) -> Vec<OsString> {
        values.into_iter().map(OsString::from).collect()
    }
}
