//! `phoxal build` - stage a runtime layout for a target and archive it as a
//! deterministic `build.phoxal` (#936).
//!
//! `build` refreshes staging exactly as `run` would - through the one shared
//! [`refresh_staging`](crate::run::refresh_staging) entry - but for the selected
//! target rather than the host, validates the staged layout through the shared
//! loader (against the declared target architecture, no execution), and archives
//! the staged layout deterministically. The bundle matches the staged layout
//! byte for byte; it is not a second format.
//!
//! `--builder` selects *where compilation happens*, never a different output:
//!
//! - `local` (default) compiles on this host with `cargo build --target`;
//! - `container` compiles inside a per-target toolchain image, then reuses the
//!   identical host-side staging + archive;
//! - `ssh://user@host` is the remote builder, which lands in phase 11 (#930).
//!
//! Every backend produces the identical deterministic `build.phoxal`.

pub(crate) mod container;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;

use crate::AppContext;
use crate::run::{RunOptions, StagingBuild};
use crate::supervisor::{ProjectLock, ProjectLockIdentity, ProjectOperation};
use container::{
    ContainerBuildSpec, ContainerEngine, EngineRunner, ProcessEngineRunner, default_builder_image,
    host_cargo_caches, platform_for_triple, require_platform_for_triple, vendored_artifacts,
};
use phoxal_cli_core::check::participant_metadata::architecture_for_triple;
use phoxal_cli_core::project::launch_plan::{LaunchMode, runtime_layout_dir};
use phoxal_cli_core::project::layout::LayoutInspection;

#[derive(Debug, Args)]
pub struct Build {
    #[arg(
        value_name = "PROJECT",
        help = "Project path. Defaults to the discovered project."
    )]
    pub project: Option<PathBuf>,
    #[arg(
        long,
        value_name = "TRIPLE",
        help = "Rust target triple to build for (e.g. aarch64-unknown-linux-gnu). Defaults to the builder's native triple: the host for `local`, and the host-architecture linux-gnu triple for `container`."
    )]
    pub target: Option<String>,
    #[arg(
        long,
        default_value = "local",
        value_name = "local|container|ssh://user@host",
        help = "Where compilation happens: `local` (host), `container` (toolchain image), or `ssh://user@host` (remote, phase 11)."
    )]
    pub builder: String,
    #[arg(
        long,
        value_name = "PATH",
        help = "Write the bundle here. Defaults to <project>/.phoxal/build/<triple>.build.phoxal."
    )]
    pub output: Option<PathBuf>,
    #[arg(
        long,
        value_enum,
        default_value_t = ContainerEngine::Docker,
        help = "Container engine for `--builder container`."
    )]
    pub container_engine: ContainerEngine,
    #[arg(
        long,
        value_name = "REF",
        help = "Override the container toolchain image. Defaults to the pinned official rust:1.88-bookworm image (native compilation for the target's platform). For older-glibc devices (e.g. jetson L4T r36) pass rust:1.88-bullseye."
    )]
    pub builder_image: Option<String>,
}

/// Where compilation happens, parsed from `--builder`. One enum-like argument -
/// fragmented boolean modes (`--docker`, `--cross`, `--remote HOST`) are not
/// accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BuilderKind {
    Local,
    Container,
    Ssh(String),
}

fn parse_builder(value: &str) -> Result<BuilderKind> {
    match value {
        "local" => Ok(BuilderKind::Local),
        "container" => Ok(BuilderKind::Container),
        other if other.starts_with("ssh://") => {
            let target = other.trim_start_matches("ssh://");
            if target.is_empty() {
                bail!("`--builder ssh://` needs a user@host, e.g. ssh://user@jetson-nano-orin");
            }
            Ok(BuilderKind::Ssh(target.to_string()))
        }
        other => bail!(
            "unknown --builder `{other}`; expected `local`, `container`, or `ssh://user@host`"
        ),
    }
}

/// Resolve a user-supplied `--target` selector to a full Rust target triple,
/// rejecting OCI/platform vocabulary (`linux/arm64`) - Rust target triples are
/// the only platform vocabulary the CLI accepts.
fn resolve_target(selector: &str) -> Result<String> {
    if selector.contains('/') {
        bail!(
            "`--target {selector}` looks like OCI platform vocabulary; use a Rust target triple \
             instead (e.g. aarch64-unknown-linux-gnu), not `linux/arm64`"
        );
    }
    crate::resolver::resolve_target_triple(selector)
}

/// The resolved build backend and its target triple: the pure decision every
/// invocation makes before touching the project, so the `--builder`/`--target`
/// interplay (container requires a target, ssh is phase 11, local defaults to
/// the host) is unit-testable without an `AppContext`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Backend {
    Local { target: String },
    Container { target: String },
}

/// The host-architecture linux-gnu triple - the container builder's "native
/// triple" default when no `--target` is given (2026-07-24 decision). The
/// container compiles natively for the host's CPU architecture under Linux; the
/// artifacts target Linux devices, never the macOS/Windows host itself.
fn host_linux_gnu_triple() -> String {
    let arch = std::env::consts::ARCH;
    format!("{arch}-unknown-linux-gnu")
}

/// Decide the backend from a parsed `--builder` and an already-resolved explicit
/// `--target`. `local` defaults its target to the host triple; `container`
/// defaults to the host-architecture linux-gnu triple (native compilation inside
/// the image); `ssh://` is rejected as a phase-11 feature.
fn select_backend(builder: BuilderKind, explicit_target: Option<String>) -> Result<Backend> {
    match builder {
        BuilderKind::Local => Ok(Backend::Local {
            target: explicit_target.unwrap_or_else(crate::resolver::host_target_triple),
        }),
        BuilderKind::Container => Ok(Backend::Container {
            // No `--target`: build for the host architecture under Linux inside
            // the container - the builder's native triple.
            target: explicit_target.unwrap_or_else(host_linux_gnu_triple),
        }),
        BuilderKind::Ssh(host) => bail!(
            "remote builders land in phase 11 (#930); `--builder ssh://{host}` is not available \
             yet. Use `--builder container --target <TRIPLE>` to cross-build locally, or run \
             `phoxal build` on {host} directly."
        ),
    }
}

impl Build {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let builder = parse_builder(&self.builder)?;
        let explicit_target = self.target.as_deref().map(resolve_target).transpose()?;
        let backend = select_backend(builder, explicit_target)?;

        let project_root =
            crate::commands::resident::resolve_target(self.project.as_deref(), app.project.root())?
                .project;

        match backend {
            Backend::Local { target } => self.build_local(app, &project_root, &target),
            Backend::Container { target } => {
                let runner = ProcessEngineRunner { ui: &app.ui };
                self.build_container(app, &project_root, &target, &runner)
            }
        }
    }

    /// Build on this host with `cargo build --target`, then stage, validate, and
    /// archive. This is the fully-implemented v0 backend.
    fn build_local(&self, app: &AppContext, project_root: &Path, target: &str) -> Result<()> {
        let staging = StagingBuild::local(Some(target.to_string()));
        self.stage_validate_archive(app, project_root, target, &staging)
    }

    /// Build inside a toolchain image, then reuse the identical host-side staging
    /// + validation + archive with the container-built binaries.
    fn build_container(
        &self,
        app: &AppContext,
        project_root: &Path,
        target: &str,
        runner: &dyn EngineRunner,
    ) -> Result<()> {
        let snapshot = tempfile::Builder::new()
            .prefix("phoxal-build-snapshot-")
            .tempdir()
            .context("failed to create a source snapshot directory")?;
        snapshot_source(project_root, snapshot.path())?;

        // The container build enforces `--locked`, so the snapshot must carry a
        // committed Cargo.lock. `snapshot_source` copies the tracked working tree
        // via `git ls-files`, so a missing lockfile means it was never committed.
        if !snapshot.path().join("Cargo.lock").is_file() {
            bail!(
                "`--builder container` compiles with `--locked`, but the source snapshot has no \
                 Cargo.lock (it is not committed to the git working tree). Run `cargo \
                 generate-lockfile` and commit Cargo.lock, then retry."
            );
        }

        // The default image is the pinned official rust image with the container
        // platform derived from the target arch; a custom `--builder-image` owns
        // its own toolchain, so we only pass `--platform` when the arch is one we
        // can map.
        let (image, platform) = match &self.builder_image {
            Some(custom) => (custom.clone(), platform_for_triple(target).map(str::to_string)),
            None => (
                default_builder_image().to_string(),
                Some(require_platform_for_triple(target)?.to_string()),
            ),
        };
        let (cargo_registry, cargo_git) = host_cargo_caches();
        let spec = ContainerBuildSpec {
            engine: self.container_engine,
            image,
            platform,
            target: target.to_string(),
            snapshot: snapshot.path().to_path_buf(),
            cargo_registry,
            cargo_git,
            artifacts: vendored_artifacts(project_root),
        };
        app.ui.info(format!(
            "compiling workspace for {target} inside {} ({}{})",
            spec.image,
            self.container_engine.program(),
            spec.platform
                .as_deref()
                .map(|platform| format!(", {platform}"))
                .unwrap_or_default(),
        ));
        runner.run(&spec.invocation())?;

        // The container wrote the target binaries into the snapshot's target
        // directory; stage from there. Officials still come from the host's
        // per-target vendored `.phoxal/artifacts`.
        let staging = StagingBuild::Prebuilt {
            target: Some(target.to_string()),
            target_dir: snapshot.path().join("target"),
        };
        self.stage_validate_archive(app, project_root, target, &staging)
    }

    /// The shared tail every backend runs: refresh staging for the target,
    /// validate the staged layout through the loader against the declared target
    /// architecture, and archive it deterministically. Holds the project lock
    /// for the whole operation, as the stager's contract requires.
    fn stage_validate_archive(
        &self,
        app: &AppContext,
        project_root: &Path,
        target: &str,
        staging: &StagingBuild,
    ) -> Result<()> {
        let _lock = ProjectLock::acquire(ProjectLockIdentity::resolve(
            project_root,
            ProjectOperation::Build,
        ))
        .context("failed to acquire the project lock for build")?;

        let options = RunOptions {
            drivers: crate::run::DriversMode::On,
            drivers_subset: Vec::new(),
            suite_source: app.suite_source.clone(),
            watch: false,
        };
        // A shippable bundle contains everything, so staging validates against
        // the full driver set (DriverSelection::All), never a `--drivers`
        // subset. `build` skips the host-native source check (`false`): the
        // loader's target-aware validation over the staged binaries is
        // authoritative, and a cross target's Linux-only crates need not compile
        // on the build host.
        let staged = crate::run::refresh_staging(project_root, &options, staging, false, &app.ui)?;

        // Validate against the *declared* target architecture: a correct
        // cross-built binary passes, a wrong-arch one for that target fails
        // precisely. Native bundles exclude simulator-only binaries (the Native
        // profile / LaunchMode::Run already enforces this).
        crate::loader::validate_layout_plan(
            &staged.staged_root,
            &LaunchMode::Run,
            &staged.plan_options(),
            LayoutInspection::Target(architecture_for_triple(target)),
        )
        .context("failed to validate the staged runtime layout for the target")?;

        let output = self
            .output
            .clone()
            .unwrap_or_else(|| default_output(project_root, target));
        let digest = crate::archive::write_build_archive(&staged.staged_root, &output)
            .context("failed to write the deterministic build.phoxal")?;

        app.ui.info(format!("staged runtime layout for {target}"));
        println!("{}", output.display());
        println!("sha256:{digest}");
        Ok(())
    }
}

/// The default bundle path: a sibling of the staged directory,
/// `<project>/.phoxal/build/<triple>.build.phoxal`, never inside the staged
/// `.phoxal/build/<triple>/` tree it archives.
fn default_output(project_root: &Path, target: &str) -> PathBuf {
    let staged = runtime_layout_dir(project_root, target);
    let parent = staged
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| project_root.to_path_buf());
    parent.join(format!(
        "{target}.{}",
        crate::archive::BUILD_ARCHIVE_EXTENSION
    ))
}

/// Copy a deterministic, git-clean source snapshot of `project_root` into
/// `dest`, honoring `.gitignore` and always excluding `.phoxal` and `target/`.
/// Uses `git ls-files` (tracked + untracked-but-not-ignored), so the snapshot is
/// exactly the working tree minus ignored/build state - which requires the
/// project to be a git working tree.
fn snapshot_source(project_root: &Path, dest: &Path) -> Result<()> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .output()
        .context("failed to run `git ls-files` for the source snapshot; is git installed?")?;
    if !output.status.success() {
        bail!(
            "`--builder container` needs a git working tree to snapshot the source; \
             `git ls-files` failed in {} (initialize a repository or commit the project first)",
            project_root.display()
        );
    }
    for rel in output.stdout.split(|byte| *byte == 0) {
        if rel.is_empty() {
            continue;
        }
        let rel = std::str::from_utf8(rel).context("git reported a non-UTF-8 path")?;
        let relative = Path::new(rel);
        if relative.starts_with(".phoxal") || relative.starts_with("target") {
            continue;
        }
        let source = project_root.join(relative);
        if !source.is_file() {
            continue;
        }
        let target = dest.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        std::fs::copy(&source, &target)
            .with_context(|| format!("failed to copy {} into the snapshot", source.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_three_builder_kinds() {
        assert_eq!(parse_builder("local").unwrap(), BuilderKind::Local);
        assert_eq!(parse_builder("container").unwrap(), BuilderKind::Container);
        assert_eq!(
            parse_builder("ssh://dev@jetson-nano-orin").unwrap(),
            BuilderKind::Ssh("dev@jetson-nano-orin".to_string())
        );
    }

    #[test]
    fn rejects_unknown_and_empty_builders() {
        assert!(parse_builder("docker").is_err());
        assert!(parse_builder("ssh://").is_err());
    }

    #[test]
    fn rejects_oci_platform_target_vocabulary() {
        let error = resolve_target("linux/arm64").expect_err("platform vocabulary is rejected");
        assert!(error.to_string().contains("Rust target triple"), "{error}");
    }

    #[test]
    fn resolves_arch_aliases_and_full_triples() {
        assert_eq!(
            resolve_target("aarch64").unwrap(),
            "aarch64-unknown-linux-gnu"
        );
        assert_eq!(
            resolve_target("x86_64-unknown-linux-gnu").unwrap(),
            "x86_64-unknown-linux-gnu"
        );
    }

    #[test]
    fn local_backend_defaults_target_to_the_host() {
        let backend = select_backend(BuilderKind::Local, None).unwrap();
        assert_eq!(
            backend,
            Backend::Local {
                target: crate::resolver::host_target_triple()
            }
        );
    }

    #[test]
    fn local_backend_honors_an_explicit_target() {
        let backend = select_backend(
            BuilderKind::Local,
            Some("aarch64-unknown-linux-gnu".to_string()),
        )
        .unwrap();
        assert_eq!(
            backend,
            Backend::Local {
                target: "aarch64-unknown-linux-gnu".to_string()
            }
        );
    }

    #[test]
    fn container_backend_defaults_target_to_the_host_linux_gnu_triple() {
        // No `--target`: the container's native triple is the host architecture
        // under Linux (2026-07-24 decision), never the macOS/Windows host triple.
        let backend = select_backend(BuilderKind::Container, None).unwrap();
        assert_eq!(
            backend,
            Backend::Container {
                target: host_linux_gnu_triple()
            }
        );
        assert!(host_linux_gnu_triple().ends_with("-unknown-linux-gnu"));

        let ok = select_backend(
            BuilderKind::Container,
            Some("aarch64-unknown-linux-gnu".to_string()),
        )
        .unwrap();
        assert_eq!(
            ok,
            Backend::Container {
                target: "aarch64-unknown-linux-gnu".to_string()
            }
        );
    }

    #[test]
    fn ssh_backend_is_rejected_as_phase_11() {
        let error = select_backend(
            BuilderKind::Ssh("dev@jetson-nano-orin".to_string()),
            Some("aarch64-unknown-linux-gnu".to_string()),
        )
        .expect_err("ssh builder must be rejected");
        assert!(error.to_string().contains("phase 11 (#930)"), "{error}");
        assert!(
            error.to_string().contains("dev@jetson-nano-orin"),
            "{error}"
        );
    }

    #[test]
    fn default_output_is_a_sibling_of_the_staged_directory() {
        let output = default_output(Path::new("/proj"), "aarch64-unknown-linux-gnu");
        assert_eq!(
            output,
            Path::new("/proj/.phoxal/build/aarch64-unknown-linux-gnu.build.phoxal")
        );
        // The sibling file is never inside the staged directory it archives.
        let staged = runtime_layout_dir(Path::new("/proj"), "aarch64-unknown-linux-gnu");
        assert!(!output.starts_with(&staged));
    }
}
