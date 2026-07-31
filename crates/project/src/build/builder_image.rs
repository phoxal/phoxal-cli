//! Deterministic preparation of the single managed compilation image.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};

use super::container::{
    ContainerEngine, EngineInvocation, proxy_args, run_engine, run_engine_output,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedImage {
    /// The reference passed to the eventual engine `run` invocation.
    pub reference: String,
}

fn valid_base_image(image: &str) -> bool {
    !image.is_empty()
        && !image.starts_with('-')
        && image
            .bytes()
            .all(|byte| !byte.is_ascii_whitespace() && !byte.is_ascii_control())
}

/// Deterministic Dockerfile bytes for a sorted, deduplicated requirement set.
#[must_use]
pub fn render_dockerfile(base_image: &str, packages: &BTreeSet<String>) -> String {
    let packages = packages
        .iter()
        // The shared parser excludes apostrophes, so explicit single quotes
        // make every selector visibly shell-quoted without another escape path.
        .map(|package| format!("'{package}'"))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "FROM {base_image}\n\nRUN apt-get update -qq \\\n    && apt-get install -y --no-install-recommends -- {packages} \\\n    && rm -rf /var/lib/apt/lists/*\n"
    )
}

/// Stable engine tag derived only from the exact Dockerfile bytes.
#[must_use]
pub fn image_tag(dockerfile: &str) -> String {
    let digest = hex::encode(Sha256::digest(dockerfile.as_bytes()));
    format!("phoxal-builder:{}", &digest[..12])
}

fn build_invocation(
    engine: ContainerEngine,
    platform: Option<&str>,
    tag: &str,
    directory: &Path,
    env: impl IntoIterator<Item = (String, String)>,
) -> EngineInvocation {
    let mut args = vec!["build".to_string()];
    if let Some(platform) = platform {
        args.extend(["--platform".to_string(), platform.to_string()]);
    }
    args.extend(proxy_args("--build-arg", env));
    args.extend([
        "-t".to_string(),
        tag.to_string(),
        directory.display().to_string(),
    ]);
    EngineInvocation {
        program: engine.program().to_string(),
        args,
    }
}

fn inspect_invocation(engine: ContainerEngine, image: &str) -> EngineInvocation {
    EngineInvocation {
        program: engine.program().to_string(),
        args: vec![
            "image".to_string(),
            "inspect".to_string(),
            "--format".to_string(),
            "{{.Architecture}}".to_string(),
            image.to_string(),
        ],
    }
}

fn expected_architecture(platform: Option<&str>) -> Option<&str> {
    match platform {
        Some("linux/arm64") => Some("arm64"),
        Some("linux/amd64") => Some("amd64"),
        _ => None,
    }
}

fn inspect_image_architecture(engine: ContainerEngine, image: &str) -> Result<String> {
    let invocation = inspect_invocation(engine, image);
    let output = run_engine_output(&invocation)?;
    Ok(String::from_utf8(output.stdout)
        .context("image inspection returned a non-UTF-8 architecture")?
        .trim()
        .to_string())
}

/// Select the base image directly or the Dockerfile-derived managed tag.
#[must_use]
pub fn reference_for(base_image: &str, packages: &BTreeSet<String>) -> String {
    if packages.is_empty() {
        base_image.to_string()
    } else {
        image_tag(&render_dockerfile(base_image, packages))
    }
}

/// Ensure the one derived builder image exists, using only engine-native cache.
pub fn prepare_builder_image(
    engine: ContainerEngine,
    base_image: &str,
    platform: Option<&str>,
    packages: &BTreeSet<String>,
    offline: bool,
    ui: &dyn crate::Reporter,
) -> Result<PreparedImage> {
    ensure!(
        valid_base_image(base_image),
        "invalid builder image reference {base_image:?}: image references cannot begin with `-` or contain whitespace or control bytes"
    );

    let reference = reference_for(base_image, packages);

    if offline {
        ui.info(format!("checking cached builder image {reference}"));
        let actual_architecture = inspect_image_architecture(engine, &reference).with_context(|| {
            format!(
                "offline build requires builder image {reference}; rerun the same command without `--offline` once to obtain it"
            )
        })?;
        if let Some(expected) = expected_architecture(platform) {
            ensure!(
                actual_architecture == expected,
                "offline builder image {reference} has architecture {actual_architecture:?}, expected {expected:?} for {}; rerun without `--offline` to rebuild the tag for this platform",
                platform.unwrap_or_default()
            );
        }
    } else if !packages.is_empty() {
        let context = tempfile::Builder::new()
            .prefix("phoxal-builder-image-")
            .tempdir()
            .context("failed to create a temporary builder image context")?;
        let dockerfile = render_dockerfile(base_image, packages);
        std::fs::write(context.path().join("Dockerfile"), dockerfile)
            .context("failed to write the temporary builder Dockerfile")?;
        ui.info(format!("preparing builder image {reference}"));
        run_engine(
            ui,
            &build_invocation(
                engine,
                platform,
                &reference,
                context.path(),
                std::env::vars(),
            ),
        )
        .with_context(|| format!("failed to prepare builder image {reference}"))?;
    }

    Ok(PreparedImage { reference })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packages(values: &[&str]) -> BTreeSet<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn dockerfile_bytes_are_deterministic_for_any_input_order() {
        let first = packages(&["pkg-config", "libudev-dev"]);
        let second = packages(&["libudev-dev", "pkg-config"]);
        let rendered = render_dockerfile("rust:1.88-bookworm", &first);
        assert_eq!(rendered, render_dockerfile("rust:1.88-bookworm", &second));
        assert_eq!(
            rendered,
            "FROM rust:1.88-bookworm\n\nRUN apt-get update -qq \\\n    && apt-get install -y --no-install-recommends -- 'libudev-dev' 'pkg-config' \\\n    && rm -rf /var/lib/apt/lists/*\n"
        );
    }

    #[test]
    fn the_tag_is_stable_for_equal_dockerfiles_and_distinct_otherwise() {
        let source = render_dockerfile("rust:1.88-bookworm", &packages(&["pkg-config"]));
        assert_eq!(image_tag(&source), image_tag(&source));
        assert_ne!(
            image_tag(&source),
            image_tag(&render_dockerfile(
                "rust:1.88-bullseye",
                &packages(&["pkg-config"])
            ))
        );
        assert_eq!(image_tag(&source).len(), "phoxal-builder:".len() + 12);
    }

    #[test]
    fn an_empty_set_uses_the_base_image_directly() {
        assert_eq!(
            reference_for("rust:1.88-bookworm", &packages(&[])),
            "rust:1.88-bookworm"
        );
        assert!(
            reference_for("rust:1.88-bookworm", &packages(&["pkg-config"]))
                .starts_with("phoxal-builder:")
        );
        assert!(!valid_base_image("-malformed"));
    }

    #[test]
    fn the_build_invocation_carries_platform_tag_and_proxy_args() {
        let invocation = build_invocation(
            ContainerEngine::Podman,
            Some("linux/arm64"),
            "phoxal-builder:abc",
            Path::new("/tmp/context"),
            [("HTTPS_PROXY".to_string(), "http://proxy".to_string())],
        );
        assert_eq!(invocation.program, "podman");
        assert_eq!(
            invocation.args,
            [
                "build",
                "--platform",
                "linux/arm64",
                "--build-arg",
                "HTTPS_PROXY=http://proxy",
                "-t",
                "phoxal-builder:abc",
                "/tmp/context"
            ]
        );
    }

    #[test]
    fn offline_renders_an_inspect_not_a_build() {
        let invocation = inspect_invocation(ContainerEngine::Docker, "phoxal-builder:abc");
        assert_eq!(invocation.program, "docker");
        assert_eq!(
            invocation.args,
            [
                "image",
                "inspect",
                "--format",
                "{{.Architecture}}",
                "phoxal-builder:abc"
            ]
        );
        assert_eq!(expected_architecture(Some("linux/arm64")), Some("arm64"));
        assert_eq!(expected_architecture(Some("linux/amd64")), Some("amd64"));
    }
}
