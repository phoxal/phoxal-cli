use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::AppContext;
use crate::resolver::{ResolvedComponentSource, ResolvedRobot, ResolvedUserRuntimeBuild};
use crate::utils::resolve_project_path;
use crate::{host_paths, shell};

pub(crate) fn pull_platform_images(app: &AppContext, resolved: &ResolvedRobot) -> Result<()> {
    for runtime in &resolved.platform_runtimes {
        // `deploy_ref` is a real digest pin or an honest tag ref — never a
        // fabricated `sha256:` — so this can't attempt to pull a fake digest.
        let image = runtime.deploy_ref();
        app.ui.info(format!("pulling {image}"));
        shell::run_status("docker", ["pull", image.as_str()], None).with_context(|| {
            if runtime.digest_pin().is_some() {
                format!("failed to pull pinned runtime image {image}")
            } else {
                format!(
                    "failed to pull runtime image {image} by tag. The phoxal/framework GHCR \
                     runtime images may not be published for this runtime set yet. Publish the \
                     runtime images, then run `phoxal-cli update --pin-digests` to pin real \
                     digests, or `phoxal-cli update --refresh-releases` to pick up a newer set."
                )
            }
        })?;
    }
    Ok(())
}

pub(crate) fn build_user_runtimes(
    project_root: &Path,
    resolved: &ResolvedRobot,
) -> Result<BTreeMap<String, String>> {
    let mut images = BTreeMap::new();
    for runtime in &resolved.user_runtimes {
        let runtime_dir = resolve_project_path(project_root, &runtime.path);
        shell::run_status(
            "docker",
            docker_build_args(&runtime.image, runtime.build.as_ref()),
            Some(&runtime_dir),
        )?;
        images.insert(runtime.name.clone(), runtime.image.clone());
    }
    Ok(images)
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
    let host_cache_dir = host_paths::cache_dir()?;
    for component in &resolved.components {
        if !component.has_driver {
            continue;
        }
        let driver_dir = match &component.source {
            ResolvedComponentSource::Path { path } => {
                resolve_project_path(project_root, path).join("driver")
            }
            ResolvedComponentSource::Git { commit, .. } => host_cache_dir
                .join("components")
                .join(format!("{}-{commit}", component.source_name))
                .join("driver"),
        };
        if driver_dir.is_dir() {
            shell::run_status("cargo", ["build", "--release"], Some(&driver_dir))?;
        }
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
            docker_build_args("phoxal-local/test/user-runtime/drive:abc", None),
            os_args([
                "build",
                "-t",
                "phoxal-local/test/user-runtime/drive:abc",
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
            docker_build_args("phoxal-local/test/user-runtime/drive:abc", Some(&build)),
            os_args([
                "build",
                "-t",
                "phoxal-local/test/user-runtime/drive:abc",
                "-f",
                "container/Dockerfile.runtime",
                "--target",
                "runtime",
                "container",
            ])
        );
    }

    fn os_args<const N: usize>(values: [&str; N]) -> Vec<OsString> {
        values.into_iter().map(OsString::from).collect()
    }
}
