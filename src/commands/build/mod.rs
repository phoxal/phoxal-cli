//! `phoxal build` - stage a runtime layout for a target and archive it as a
//! deterministic `build.phoxal` (#936).
//!
//! `build` refreshes staging exactly as `run` would - through the one shared
//! `refresh_staging` entry - but for the selected
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
//! - `ssh://user@host` snapshots source, compiles in a remote temporary
//!   directory, and pulls back the identical archive.
//!
//! Every backend produces the identical deterministic `build.phoxal`.

pub(crate) mod container;

use phoxal_cli_core::project::launch_plan::RunIdentity;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;

use crate::AppContext;
use crate::run::{RunOptions, StagingBuild};
use crate::supervisor::{ProjectLock, ProjectLockIdentity, ProjectOperation};
use container::{
    ContainerBuildSpec, ContainerEngine, ContainerOfficial, EngineRunner, ProcessEngineRunner,
    default_builder_image, host_cargo_caches, platform_for_triple, require_platform_for_triple,
};
use phoxal_cli_core::check::participant_metadata::expected_target_for_triple;
use phoxal_cli_core::project::launch_plan::runtime_layout_dir;
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
        help = "Where compilation happens: `local` (host), `container` (toolchain image), or `ssh://user@host` (remote source snapshot)."
    )]
    pub builder: String,
    #[arg(
        long,
        value_name = "PATH",
        help = "Write the bundle here. Defaults to <project>/.phoxal/<triple>.build.phoxal."
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
/// interplay is unit-testable without an `AppContext`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Backend {
    Local {
        target: String,
    },
    Container {
        target: String,
    },
    Ssh {
        host: String,
        target: Option<String>,
    },
}

/// The host-architecture linux-gnu triple - the container builder's "native
/// triple" default when no `--target` is given. The
/// container compiles natively for the host's CPU architecture under Linux; the
/// artifacts target Linux devices, never the macOS/Windows host itself.
fn host_linux_gnu_triple() -> String {
    let arch = std::env::consts::ARCH;
    format!("{arch}-unknown-linux-gnu")
}

/// Decide the backend from a parsed `--builder` and an already-resolved explicit
/// `--target`. `local` defaults its target to the host triple; `container`
/// defaults to the host-architecture linux-gnu triple (native compilation inside
/// the image); `ssh://` discovers the remote host triple when one is not given.
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
        BuilderKind::Ssh(host) => Ok(Backend::Ssh {
            host,
            target: explicit_target,
        }),
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
        // Scope every project-rooted path (the project lock, staging) to the
        // RESOLVED project, exactly as `run`/`start` do (#936, finding 3):
        // without this, `phoxal build /path/to/B` run from project A would
        // compile B while writing A's staged state.
        unsafe {
            std::env::set_var(crate::host_paths::PROJECT_ROOT_ENV, &project_root);
        }

        // Acquire the Build lock for the WHOLE operation - before any snapshot or
        // compile - and hold it through archiving (#936, finding B). The
        // container path snapshots and compiles the source; the lock must already
        // be held then so no concurrent phoxal operation can edit the tree between
        // the snapshot the container compiles and the manifests/assets staging
        // reads, which would pair old binaries with new manifests.
        let _lock = ProjectLock::acquire(ProjectLockIdentity::resolve(
            &project_root,
            ProjectOperation::Build,
        ))
        .context("failed to acquire the project lock for build")?;

        match backend {
            Backend::Local { target } => self.build_local(app, &project_root, &target),
            Backend::Container { target } => {
                let runner = ProcessEngineRunner { ui: &app.ui };
                self.build_container(app, &project_root, &target, &runner)
            }
            Backend::Ssh { host, target } => {
                self.build_ssh(app, &project_root, &host, target.as_deref())
            }
        }
    }

    fn build_ssh(
        &self,
        app: &AppContext,
        project_root: &Path,
        host: &str,
        explicit_target: Option<&str>,
    ) -> Result<()> {
        crate::commands::deploy::validate_ssh_target(host)?;
        crate::commands::deploy::require_remote_phoxal(host)?;
        let target = explicit_target
            .map(str::to_string)
            .map(Ok)
            .unwrap_or_else(|| crate::commands::deploy::remote_toolchain_target(host))?;
        let remote_dir = crate::commands::deploy::create_remote_temp(host)?;
        let result = (|| -> Result<()> {
            crate::commands::deploy::upload_source_payload(host, project_root, &remote_dir)?;
            let source_dir = format!("{remote_dir}/source");
            let remote_archive = format!("{remote_dir}/build.phoxal");
            crate::commands::deploy::run_remote(
                host,
                &format!(
                    "{}; {}; {} build {} --builder local --target {} --output {}",
                    crate::commands::deploy::REMOTE_TOOLCHAIN_PATH,
                    crate::commands::deploy::remote_unpack_source_command(&remote_dir),
                    crate::commands::deploy::REMOTE_PHOXAL,
                    crate::commands::deploy::shell_quote(&source_dir),
                    crate::commands::deploy::shell_quote(&target),
                    crate::commands::deploy::shell_quote(&remote_archive),
                ),
            )?;
            let pulled = tempfile::Builder::new()
                .prefix("phoxal-ssh-build-")
                .suffix(".build.phoxal")
                .tempfile()?;
            crate::commands::deploy::run_local(
                "scp",
                &[
                    "-q",
                    &format!("{host}:{remote_archive}"),
                    pulled.path().to_string_lossy().as_ref(),
                ],
            )?;
            let extracted = tempfile::Builder::new()
                .prefix("phoxal-ssh-layout-")
                .tempdir()?;
            crate::archive::extract_build_archive(pulled.path(), extracted.path())?;
            crate::runtime_header::RuntimeHeader::read_and_validate(extracted.path())?;
            crate::loader::validate_layout_plan(
                extracted.path(),
                &phoxal_cli_core::project::layout::PlanOptions::default(),
                LayoutInspection::Target(expected_target_for_triple(&target)?),
                RunIdentity::default(),
            )?;
            let staged_root = publish_staged_root(project_root, &target, extracted.path())?;
            let output = self
                .output
                .clone()
                .unwrap_or_else(|| default_output(project_root, &target));
            if let Some(parent) = output.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(pulled.path(), &output)?;
            let digest = sha256_file(&output)?;
            app.ui.info(format!(
                "staged runtime layout at {}",
                staged_root.display()
            ));
            println!("{}", output.display());
            println!("sha256:{digest}");
            Ok(())
        })();
        let cleanup = crate::commands::deploy::cleanup_remote_temp(host, &remote_dir);
        result?;
        cleanup
    }

    /// Build on this host with `cargo build --target`, then stage, validate, and
    /// archive. Local staging reads the live tree, but the Build lock (held by
    /// [`Self::run`]) already serializes it against other phoxal operations.
    fn build_local(&self, app: &AppContext, project_root: &Path, target: &str) -> Result<()> {
        let staging = StagingBuild::native_bundle(target.to_string());
        // Local builds and stages the live project tree under the lock.
        self.stage_validate_archive(app, project_root, project_root, target, &staging)
    }

    /// Build inside a toolchain image, then reuse the identical host-side
    /// staging, validation, and archive with the container-built binaries. The
    /// Build lock is already held by [`Self::run`], so the snapshot, the compile,
    /// and the staging that reads that same frozen snapshot all happen under one
    /// lock (#936, finding B).
    fn build_container(
        &self,
        app: &AppContext,
        project_root: &Path,
        target: &str,
        runner: &dyn EngineRunner,
    ) -> Result<()> {
        let (snapshot, officials_root) =
            self.compile_in_container(app, project_root, target, runner, &app.ui)?;

        // Stage manifests, assets, AND binaries from the SAME frozen snapshot the
        // container compiled, never the live tree (#936, finding B): the container
        // wrote the target binaries into `<snapshot>/target`, and staging reads
        // robot.yaml and assets from `<snapshot>` too, so a bundle can never pair
        // old binaries with newer live manifests. The COMPLETE official set -
        // every catalog service/tool/the router, AND every registry-sourced
        // component driver this robot declares - was materialized natively
        // INSIDE the container too, into `officials_root`; host-side staging
        // reads that and never cross-compiles anything itself (organization
        // #951 WS4; blocker 2 of the review). The default output stays a
        // sibling of the real project's staged directory.
        let staging = StagingBuild::prebuilt_native_bundle(
            target.to_string(),
            snapshot.path().join("target"),
            Some(officials_root.path().to_path_buf()),
        );
        self.stage_validate_archive(app, project_root, snapshot.path(), target, &staging)
    }

    /// Snapshot the source, validate its Cargo.lock, resolve the robot graph
    /// to learn its component driver packages, and compile the workspace for
    /// `target` inside the toolchain image - which also materializes the
    /// COMPLETE official set (every catalog service/tool/router plus every
    /// registry-sourced component driver this robot declares) via `cargo
    /// install`, natively on the container's own platform (organization#951
    /// WS4; blocker 2 of the review closes the "component drivers install
    /// host-side, cross-compiled" gap) - returning the snapshot directory
    /// (with `<snapshot>/target` populated by the container) and the
    /// officials root (with its `bin/` populated the identical way, so
    /// host-side staging never needs to cross-compile anything). Assumes the
    /// Build lock is already held (see [`Self::run`]); split out so the
    /// snapshot+compile ordering under the lock is unit-testable with a fake
    /// [`EngineRunner`].
    fn compile_in_container(
        &self,
        app: &AppContext,
        project_root: &Path,
        target: &str,
        runner: &dyn EngineRunner,
        ui: &crate::Ui,
    ) -> Result<(tempfile::TempDir, tempfile::TempDir)> {
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

        let officials_root = tempfile::Builder::new()
            .prefix("phoxal-build-officials-")
            .tempdir()
            .context("failed to create an officials materialization directory")?;

        // The default image is the pinned official rust image with the container
        // platform derived from the target arch; a custom `--builder-image` owns
        // its own toolchain, so we only pass `--platform` when the arch is one we
        // can map.
        let (image, platform) = match &self.builder_image {
            Some(custom) => (
                custom.clone(),
                platform_for_triple(target).map(str::to_string),
            ),
            None => (
                default_builder_image().to_string(),
                Some(require_platform_for_triple(target)?.to_string()),
            ),
        };
        let (cargo_registry, cargo_git) = host_cargo_caches();
        // The deterministic, robot-independent official set - every catalog
        // service, tool, and the router ("every official always runs" per
        // #945) - is known from the catalog alone; no `resolve()` (and no
        // robot.yaml) is needed to compute it.
        let train = phoxal_cli_core::project::train::resolve_locked_train(project_root)
            .context("failed to resolve the locked framework train for the container build")?
            .version;
        let mut officials = phoxal_cli_core::project::catalog::for_webots(false)
            .map(|official| ContainerOfficial {
                package: phoxal_cli_core::project::catalog::cargo_package_name(official.package),
                train: train.clone(),
            })
            .collect::<Vec<_>>();

        // Robot-specific component driver packages are NOT in the static
        // catalog - the container cannot know them without resolving the
        // robot graph, exactly as `refresh_staging` later will (organization
        // #951 WS4 review, blocker 2). Resolve it here, from the SAME frozen
        // snapshot the container compiles, with the same target/driver
        // options `phoxal build` uses (`DriverSelection::All`, no
        // simulators), and add every REGISTRY-sourced driver it finds so the
        // container materializes them too; a workspace/path-overridden
        // driver never reaches `cargo install` and is correctly left out
        // (see `stager::materialize_component_driver`).
        let robot_path = phoxal_cli_core::project::resolver::discover_robot_yaml(snapshot.path())
            .with_context(|| {
            format!(
                "failed to find robot.yaml in the source snapshot {}",
                snapshot.path().display()
            )
        })?;
        let snapshot_root = robot_path
            .parent()
            .context("robot.yaml did not have a parent directory")?;
        let robot = phoxal_cli_core::project::resolver::load_robot(&robot_path)?;
        let driver_policy = crate::run::DriverPolicy::from_options(
            &RunOptions {
                drivers: crate::run::DriversMode::On,
                drivers_subset: Vec::new(),
                offline: app.offline,
            },
            &crate::run::driven_instances(&robot),
        )?;
        let resolved = crate::resolver::resolve(
            &robot,
            snapshot_root,
            phoxal_cli_core::project::resolver::ResolveOptions {
                official_target_triple: Some(target.to_string()),
                tool_target_triple: Some(target.to_string()),
                drivers: driver_policy.selection(),
                include_simulators: false,
            },
        )
        .context("failed to resolve the robot graph to learn its component driver packages")?;
        officials.extend(component_driver_officials(&resolved));
        let spec = ContainerBuildSpec {
            engine: self.container_engine,
            image,
            platform,
            target: target.to_string(),
            snapshot: snapshot.path().to_path_buf(),
            cargo_registry,
            cargo_git,
            officials_root: officials_root.path().to_path_buf(),
            officials,
            offline: app.offline,
        };
        ui.info(format!(
            "compiling workspace and materializing officials for {target} inside {} ({}{})",
            spec.image,
            self.container_engine.program(),
            spec.platform
                .as_deref()
                .map(|platform| format!(", {platform}"))
                .unwrap_or_default(),
        ));
        runner.run(&spec.invocation())?;
        Ok((snapshot, officials_root))
    }

    /// The shared tail every backend runs: refresh staging for the target from
    /// `staging_source` (the live project for `local`, the frozen snapshot for
    /// `container`), validate the staged layout through the loader against the
    /// declared target signature, and archive it deterministically as a sibling
    /// of `project_root`'s staged directory. The Build lock is held by the caller
    /// ([`Self::run`]) for the whole operation.
    fn stage_validate_archive(
        &self,
        app: &AppContext,
        project_root: &Path,
        staging_source: &Path,
        target: &str,
        staging: &StagingBuild,
    ) -> Result<()> {
        let options = RunOptions {
            drivers: crate::run::DriversMode::On,
            drivers_subset: Vec::new(),
            offline: app.offline,
        };
        // A shippable bundle contains everything, so staging validates against
        // the full driver set (DriverSelection::All), never a `--drivers`
        // subset. `build` skips the host-native source check (`false`): the
        // loader's target-aware validation over the staged binaries is
        // authoritative, and a cross target's Linux-only crates need not compile
        // on the build host. `refresh_staging` already validates the compiled
        // layout against the DECLARED target signature (via `staging.target()`)
        // and publishes only after that succeeds (organization#951 WS4
        // review) - there is no separate validation left to do here.
        let staged = crate::run::refresh_staging(
            staging_source,
            &options,
            staging,
            false,
            RunIdentity::default(),
            &app.ui,
        )?;

        // The container path staged under the frozen snapshot, which is
        // deleted when the snapshot guard drops. Publish the validated staged
        // root into the real project's `.phoxal/bundle/` so every
        // backend leaves the same persistent staged runtime layout the command
        // reports and the docs promise (#936); the archive is then written
        // from that published root.
        let staged_root = if staged.staged_root.starts_with(project_root) {
            staged.staged_root.clone()
        } else {
            publish_staged_root(project_root, target, &staged.staged_root)?
        };

        let output = self
            .output
            .clone()
            .unwrap_or_else(|| default_output(project_root, target));
        let digest = crate::archive::write_build_archive(&staged_root, &output)
            .context("failed to write the deterministic build.phoxal")?;

        app.ui.info(format!(
            "staged runtime layout at {}",
            staged_root.display()
        ));
        println!("{}", output.display());
        println!("sha256:{digest}");
        Ok(())
    }
}

/// Every distinct registry-sourced component driver package a resolved robot
/// declares, as [`ContainerOfficial`] entries at that driver's own resolved
/// train - the pure decision half of blocker 2's fix (organization#951 WS4
/// review): the container cannot know component drivers from the catalog
/// alone, so `compile_in_container` resolves the robot graph first and hands
/// the result here to learn which drivers to install alongside the static
/// catalog set. Deduplicates by package (multiple component instances can
/// share one driver package) and skips a workspace/path-overridden driver
/// entirely - that one is staged from its own build output, in the container
/// or on the host, and never reaches `cargo install` either way.
fn component_driver_officials(
    resolved: &phoxal_cli_core::project::resolver::ResolvedRobot,
) -> Vec<ContainerOfficial> {
    let mut seen_packages = std::collections::BTreeSet::new();
    crate::check::component_driver_runtimes_by_ref(resolved)
        .into_values()
        .filter(|runtime| seen_packages.insert(runtime.package.clone()))
        .map(|runtime| ContainerOfficial {
            package: phoxal_cli_core::project::catalog::cargo_package_name(&runtime.package),
            train: runtime.train.clone(),
        })
        .collect()
}

/// Copy a validated staged runtime layout produced outside the project (the
/// container snapshot) into the project's real `.phoxal/bundle/`,
/// replacing any previous layout via the same candidate-then-rename swap the
/// stager uses, so a crashed publish never leaves a half-written layout.
fn publish_staged_root(project_root: &Path, target: &str, source: &Path) -> Result<PathBuf> {
    let destination = runtime_layout_dir(project_root);
    let parent = destination
        .parent()
        .context("staged runtime layout has no parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create {}", parent.display()))?;
    let candidate = tempfile::Builder::new()
        .prefix(&format!(".{target}-candidate-"))
        .tempdir_in(parent)
        .with_context(|| {
            format!(
                "failed to create a layout candidate in {}",
                parent.display()
            )
        })?;
    crate::stager::copy_tree_into(source, candidate.path()).with_context(|| {
        format!(
            "failed to copy the container-staged layout {} into the project",
            source.display()
        )
    })?;
    let candidate = candidate.keep();
    let previous = parent.join(format!(".{target}-previous"));
    let _ = std::fs::remove_dir_all(&previous);
    let had_previous = std::fs::symlink_metadata(&destination).is_ok();
    if had_previous {
        std::fs::rename(&destination, &previous).with_context(|| {
            format!(
                "failed to move previous layout {} aside",
                destination.display()
            )
        })?;
    }
    if let Err(error) = std::fs::rename(&candidate, &destination) {
        if had_previous {
            let _ = std::fs::rename(&previous, &destination);
        }
        let _ = std::fs::remove_dir_all(&candidate);
        return Err(error).with_context(|| {
            format!(
                "failed to publish the staged layout {}",
                destination.display()
            )
        });
    }
    let _ = std::fs::remove_dir_all(&previous);
    Ok(destination)
}

/// The default bundle path: a sibling of the staged directory,
/// `<project>/.phoxal/<triple>.build.phoxal`, never inside the staged
/// `.phoxal/bundle/` tree it archives.
fn default_output(project_root: &Path, target: &str) -> PathBuf {
    let staged = runtime_layout_dir(project_root);
    let parent = staged
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| project_root.to_path_buf());
    parent.join(format!(
        "{target}.{}",
        crate::archive::BUILD_ARCHIVE_EXTENSION
    ))
}

fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// How a `git ls-files` entry materializes on disk, deciding how the snapshot
/// treats it (#936, finding B).
enum SnapshotEntry {
    /// A regular file - copied byte for byte.
    File,
    /// A symlink - copied AS a symlink (never dereferenced), so a symlinked
    /// source file keeps compiling exactly as in the working tree.
    Symlink,
    /// A directory materializing a `git ls-files` entry is a submodule gitlink
    /// (`git ls-files` never lists a plain directory), which the v0 container
    /// snapshot does not support.
    Submodule,
    /// Vanished between `git ls-files` and the stat (e.g. a just-deleted
    /// untracked file) - skipped.
    Missing,
}

/// Classify how a tracked/untracked working-tree path materializes on disk,
/// using `symlink_metadata` so a symlink is seen as a symlink (not
/// dereferenced) and a submodule gitlink is seen as its checked-out directory.
fn classify_snapshot_entry(source: &Path) -> Result<SnapshotEntry> {
    match std::fs::symlink_metadata(source) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                Ok(SnapshotEntry::Symlink)
            } else if file_type.is_file() {
                Ok(SnapshotEntry::File)
            } else if file_type.is_dir() {
                Ok(SnapshotEntry::Submodule)
            } else {
                // A socket/fifo/device the snapshot has no business copying.
                Ok(SnapshotEntry::Missing)
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(SnapshotEntry::Missing),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", source.display())),
    }
}

/// Copy a deterministic, git-clean source snapshot of `project_root` into
/// `dest`, honoring `.gitignore` and always excluding `.phoxal` and `target/`.
/// Uses `git ls-files` (tracked + untracked-but-not-ignored), so the snapshot is
/// exactly the working tree minus ignored/build state - which requires the
/// project to be a git working tree.
///
/// Symlinks are preserved as symlinks (never dereferenced), so the container
/// compiles against a faithful copy of the tree (#936, finding B). A submodule
/// is rejected with a precise error: v0 does not include submodule working trees
/// in the snapshot, so a build that would silently drop submodule sources fails
/// loudly instead.
pub(crate) fn snapshot_source(project_root: &Path, dest: &Path) -> Result<()> {
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
            "source snapshotting needs a git working tree; \
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
        let target = dest.join(relative);
        match classify_snapshot_entry(&source)? {
            SnapshotEntry::Missing => continue,
            SnapshotEntry::Submodule => bail!(
                "source snapshotting does not support git submodules yet (v0): `{rel}` is a \
                 submodule, and its working tree would be silently dropped. Vendor the \
                 submodule's sources into the workspace before building or deploying.",
            ),
            SnapshotEntry::Symlink => {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("failed to create {}", parent.display()))?;
                }
                let link = std::fs::read_link(&source)
                    .with_context(|| format!("failed to read symlink {}", source.display()))?;
                // A symlink escaping the project root would let post-compile
                // staging read LIVE or external content through the "frozen"
                // snapshot (host-side `fs::copy` follows links), so the frozen
                // guarantee would be a fiction for that asset (#936). Only
                // project-internal links are preserved.
                ensure_snapshot_link_contained(project_root, relative, &link)?;
                symlink_verbatim(&link, &target)
                    .with_context(|| format!("failed to snapshot symlink {}", source.display()))?;
            }
            SnapshotEntry::File => {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("failed to create {}", parent.display()))?;
                }
                std::fs::copy(&source, &target).with_context(|| {
                    format!("failed to copy {} into the snapshot", source.display())
                })?;
            }
        }
    }
    Ok(())
}

/// Reject a tracked symlink whose target escapes the project root. `rel` is the
/// symlink's project-relative path; `link` is its literal target. An absolute
/// target is always an escape; a relative one is resolved lexically against the
/// link's parent directory and must stay inside the project.
fn ensure_snapshot_link_contained(project_root: &Path, rel: &Path, link: &Path) -> Result<()> {
    let escapes = if link.is_absolute() {
        true
    } else {
        let mut depth: i64 = rel.components().count() as i64 - 1;
        let mut escaped = false;
        for component in link.components() {
            match component {
                std::path::Component::ParentDir => {
                    depth -= 1;
                    if depth < 0 {
                        escaped = true;
                        break;
                    }
                }
                std::path::Component::CurDir => {}
                _ => depth += 1,
            }
        }
        escaped
    };
    if escapes {
        bail!(
            "source snapshotting cannot preserve `{}`: it is a symlink to `{}`, which escapes \
             the project root {}. Move the linked content into the project, or replace the link \
             with the real file.",
            rel.display(),
            link.display(),
            project_root.display()
        );
    }
    Ok(())
}

/// Recreate a symlink `link_target` at `at`, verbatim, without following it.
#[cfg(unix)]
fn symlink_verbatim(link_target: &Path, at: &Path) -> Result<()> {
    std::os::unix::fs::symlink(link_target, at).map_err(Into::into)
}

/// The CLI and framework publish no non-unix complete-runtime target, so there
/// is no real producer for a non-unix snapshot symlink; refusing is honest,
/// where the old "best-effort copy" was both speculative and incorrect (it
/// resolved relative targets against the process cwd) (#936).
#[cfg(not(unix))]
fn symlink_verbatim(_link_target: &Path, at: &Path) -> Result<()> {
    bail!(
        "cannot snapshot the symlink at {}: symlink-preserving snapshots are not supported on          this platform",
        at.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_cli_core::project::catalog::ArtifactKind;
    use phoxal_cli_core::project::resolver::{
        ResolvedComponent, ResolvedComponentPackage, ResolvedComponentSource,
        ResolvedPlatformRuntime, ResolvedRobot,
    };

    fn minimal_resolved_robot() -> ResolvedRobot {
        let yaml = r#"schema: robot/v0
robot:
  id: testbot
  namespace: dev
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: omnidirectional
    actuators: []
    encoders: []
  components: {}
"#;
        ResolvedRobot {
            robot: phoxal::model::robot::v0::Robot::parse_from_string(yaml)
                .expect("minimal fixture robot.yaml parses"),
            train: "0.36.0".to_string(),
            target: "aarch64-unknown-linux-gnu".to_string(),
            platform_runtimes: Vec::new(),
            simulators: Vec::new(),
            user_runtimes: Vec::new(),
            user_tools: Vec::new(),
            undeclared_runtimes: Vec::new(),
            components: Vec::new(),
            tools: Vec::new(),
            path_overrides: Vec::new(),
        }
    }

    fn registry_driver_component(instance: &str, package: &str) -> ResolvedComponent {
        let runtime = ResolvedPlatformRuntime {
            name: package
                .strip_prefix("phoxal/component-")
                .unwrap_or(package)
                .to_string(),
            package: package.to_string(),
            kind: ArtifactKind::ComponentDriver,
            path_override: None,
            train: "0.36.0".to_string(),
            target: Some("aarch64-unknown-linux-gnu".to_string()),
        };
        ResolvedComponent {
            instance: instance.to_string(),
            source_name: instance.to_string(),
            assets: ResolvedComponentPackage {
                package: package.to_string(),
                kind: ArtifactKind::ComponentAssets,
                source: ResolvedComponentSource::Registry,
                resolved_dir: Some(PathBuf::from("/nonexistent")),
                registry_runtime: None,
            },
            driver: Some(ResolvedComponentPackage {
                package: package.to_string(),
                kind: ArtifactKind::ComponentDriver,
                source: ResolvedComponentSource::Registry,
                resolved_dir: None,
                registry_runtime: Some(runtime),
            }),
            has_driver: true,
        }
    }

    fn path_overridden_driver_component(instance: &str) -> ResolvedComponent {
        ResolvedComponent {
            instance: instance.to_string(),
            source_name: instance.to_string(),
            assets: ResolvedComponentPackage {
                package: format!("workspace/{instance}"),
                kind: ArtifactKind::ComponentAssets,
                source: ResolvedComponentSource::Path {
                    path: PathBuf::from("components").join(instance),
                },
                resolved_dir: Some(PathBuf::from("components").join(instance)),
                registry_runtime: None,
            },
            driver: Some(ResolvedComponentPackage {
                package: format!("workspace/{instance}"),
                kind: ArtifactKind::ComponentDriver,
                source: ResolvedComponentSource::Path {
                    path: PathBuf::from("components").join(instance),
                },
                resolved_dir: Some(PathBuf::from("components").join(instance)),
                registry_runtime: None,
            }),
            has_driver: true,
        }
    }

    /// Blocker 2 of the organization#951 WS4 review: the container used to
    /// materialize only the static catalog set, silently skipping
    /// robot-specific component driver packages and leaving them to
    /// cross-compile host-side. `component_driver_officials` is the pure
    /// decision that closes the gap - proven here without touching a
    /// container, `cargo install`, or the network.
    #[test]
    fn component_driver_officials_covers_registry_drivers_and_dedupes_and_skips_path_overrides() {
        let mut resolved = minimal_resolved_robot();
        resolved.components = vec![
            registry_driver_component("left_wheel", "phoxal/component-ddsm115"),
            // A second instance sharing the SAME driver package must not
            // produce a second `cargo install` for it.
            registry_driver_component("right_wheel", "phoxal/component-ddsm115"),
            // A workspace/path-overridden driver never reaches `cargo
            // install`, in the container or on the host - it must be absent.
            path_overridden_driver_component("custom_gripper"),
        ];

        let officials = component_driver_officials(&resolved);

        assert_eq!(
            officials,
            vec![ContainerOfficial {
                package: "phoxal-component-ddsm115".to_string(),
                train: "0.36.0".to_string(),
            }],
            "exactly one entry for the shared registry driver, deduped across both instances, \
             and no entry at all for the path-overridden driver"
        );
    }

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
    fn ssh_backend_retains_the_host_and_optional_target() {
        let backend = select_backend(
            BuilderKind::Ssh("dev@jetson-nano-orin".to_string()),
            Some("aarch64-unknown-linux-gnu".to_string()),
        )
        .unwrap();
        assert_eq!(
            backend,
            Backend::Ssh {
                host: "dev@jetson-nano-orin".to_string(),
                target: Some("aarch64-unknown-linux-gnu".to_string()),
            }
        );
    }

    #[test]
    fn default_output_is_a_sibling_of_the_staged_directory() {
        let output = default_output(Path::new("/proj"), "aarch64-unknown-linux-gnu");
        assert_eq!(
            output,
            Path::new("/proj/.phoxal/aarch64-unknown-linux-gnu.build.phoxal")
        );
        // The sibling file is never inside the staged directory it archives.
        let staged = runtime_layout_dir(Path::new("/proj"));
        assert!(!output.starts_with(&staged));
    }

    #[test]
    fn snapshot_link_containment_is_lexical_and_depth_aware() -> Result<()> {
        let root = Path::new("/proj");
        // Internal relative links are fine, including into a sibling directory.
        ensure_snapshot_link_contained(
            root,
            Path::new("worlds/a.wbt"),
            Path::new("../meshes/a.stl"),
        )?;
        // Climbing past the project root is rejected.
        assert!(
            ensure_snapshot_link_contained(root, Path::new("a.txt"), Path::new("../out.txt"))
                .is_err()
        );
        // Absolute targets are always rejected.
        assert!(
            ensure_snapshot_link_contained(root, Path::new("a.txt"), Path::new("/etc/hosts"))
                .is_err()
        );
        Ok(())
    }
}
