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

use phoxal_cli_core::project::launch_plan::RunIdentity;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::build::container::{
    ContainerBuildSpec, ContainerEngine, ContainerOfficial, default_builder_image,
    host_cargo_caches, platform_for_triple, require_platform_for_triple,
};
use crate::build::profile::StagingBuild;
use crate::run::RunOptions;
use crate::{BuildBackend, BuildBundleRequest, BuiltBundle};
use anyhow::{Context, Result, bail};
use phoxal_cli_core::check::participant_metadata::expected_target_for_triple;
use phoxal_cli_core::project::launch_plan::runtime_layout_dir;
use phoxal_cli_core::project::layout::LayoutInspection;

const REMOTE_TOOLCHAIN_PATH: &str = r#"export PATH="$HOME/.cargo/bin:$PATH""#;
const REMOTE_PHOXAL: &str = "/usr/local/bin/phoxal";

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
    crate::resolve::project::resolve_target_triple(selector)
}

/// The resolved build backend and its target triple: the pure decision every
/// invocation makes before touching the project, so the `--builder`/`--target`
/// interplay is unit-testable without an `AppContext`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ResolvedBackend {
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
fn select_backend(builder: BuildBackend) -> Result<ResolvedBackend> {
    match builder {
        BuildBackend::Local { target } => Ok(ResolvedBackend::Local {
            target: target
                .as_deref()
                .map(resolve_target)
                .transpose()?
                .unwrap_or_else(crate::resolve::project::host_target_triple),
        }),
        BuildBackend::Container { target, .. } => Ok(ResolvedBackend::Container {
            // No `--target`: build for the host architecture under Linux inside
            // the container - the builder's native triple.
            target: target
                .as_deref()
                .map(resolve_target)
                .transpose()?
                .unwrap_or_else(host_linux_gnu_triple),
        }),
        BuildBackend::Ssh { host, target } => Ok(ResolvedBackend::Ssh {
            host,
            target: target.as_deref().map(resolve_target).transpose()?,
        }),
    }
}

struct Worker {
    request: BuildBundleRequest,
    engine: ContainerEngine,
    image: Option<String>,
}

pub(crate) fn build_bundle(request: BuildBundleRequest) -> Result<BuiltBundle> {
    let backend = select_backend(request.backend.clone())?;
    let (engine, image) = match &request.backend {
        BuildBackend::Container { engine, image, .. } => (*engine, image.clone()),
        _ => (ContainerEngine::Docker, None),
    };
    let worker = Worker {
        request,
        engine,
        image,
    };
    let project_root = worker.request.target.logical_root.clone();
    let (archive, sha256, staged_root) = match backend {
        ResolvedBackend::Local { target } => worker.build_local(&project_root, &target),
        ResolvedBackend::Container { target } => worker.build_container(&project_root, &target),
        ResolvedBackend::Ssh { host, target } => {
            worker.build_ssh(&project_root, &host, target.as_deref())
        }
    }?;
    Ok(BuiltBundle {
        archive,
        sha256,
        staged_root,
    })
}

impl Worker {
    fn build_ssh(
        &self,
        project_root: &Path,
        host: &str,
        explicit_target: Option<&str>,
    ) -> Result<(PathBuf, String, Option<PathBuf>)> {
        validate_ssh_target(host)?;
        let reporter = self.request.reporter.as_ref();
        require_remote_phoxal(host, reporter)?;
        let target = explicit_target
            .map(str::to_string)
            .map(Ok)
            .unwrap_or_else(|| remote_toolchain_target(host, reporter))?;
        let remote_dir = create_remote_temp(host, reporter)?;
        let result = (|| -> Result<(PathBuf, String, Option<PathBuf>)> {
            upload_source_payload(host, project_root, &remote_dir, reporter)?;
            let source_dir = format!("{remote_dir}/source");
            let remote_archive = format!("{remote_dir}/build.phoxal");
            run_remote(
                host,
                &format!(
                    "{}; {}; {} build {} --builder local --target {} --output {}{}",
                    REMOTE_TOOLCHAIN_PATH,
                    remote_unpack_source_command(&remote_dir),
                    REMOTE_PHOXAL,
                    shell_quote(&source_dir),
                    shell_quote(&target),
                    shell_quote(&remote_archive),
                    // Forward --offline to the nested remote `phoxal build`
                    // invocation (organization#951 WS4 review, round 2): the
                    // remote process has no way to know the local invocation
                    // requested it otherwise.
                    if self.request.offline {
                        " --offline"
                    } else {
                        ""
                    },
                ),
                reporter,
            )?;
            let pulled = tempfile::Builder::new()
                .prefix("phoxal-ssh-build-")
                .suffix(".build.phoxal")
                .tempfile()?;
            run_local(
                "scp",
                &[
                    "-q",
                    &format!("{host}:{remote_archive}"),
                    pulled.path().to_string_lossy().as_ref(),
                ],
                reporter,
            )?;
            let extracted = tempfile::Builder::new()
                .prefix("phoxal-ssh-layout-")
                .tempdir()?;
            crate::bundle::archive::extract_build_archive(pulled.path(), extracted.path())?;
            crate::load::header::RuntimeHeader::read_and_validate(extracted.path())?;
            crate::load::layout::validate_layout_plan(
                extracted.path(),
                &phoxal_cli_core::project::layout::PlanOptions::default(),
                LayoutInspection::Target(expected_target_for_triple(&target)?),
                RunIdentity::default(),
            )?;
            let staged_root = self
                .request
                .publish
                .then(|| publish_staged_root(project_root, &target, extracted.path()))
                .transpose()?;
            let output = self
                .request
                .output
                .clone()
                .unwrap_or_else(|| default_output(project_root, &target));
            if let Some(parent) = output.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(pulled.path(), &output)?;
            let digest = sha256_file(&output)?;
            Ok((output, digest, staged_root))
        })();
        let cleanup = cleanup_remote_temp(host, &remote_dir, reporter);
        let built = result?;
        cleanup?;
        Ok(built)
    }

    /// Build on this host with `cargo build --target`, then stage, validate, and
    /// archive. Local staging reads the live tree, but the command adapter
    /// already holds the Build lock and serializes it against other phoxal operations.
    fn build_local(
        &self,
        project_root: &Path,
        target: &str,
    ) -> Result<(PathBuf, String, Option<PathBuf>)> {
        let staging = StagingBuild::native_bundle(target.to_string());
        // Local builds and stages the live project tree under the lock.
        self.stage_validate_archive(project_root, project_root, target, &staging)
    }

    /// Build inside a toolchain image, then reuse the identical host-side
    /// staging, validation, and archive with the container-built binaries. The
    /// The command adapter already holds the Build lock, so the snapshot, the
    /// compile, and the staging that reads that same frozen snapshot all happen
    /// under one lock (#936, finding B).
    fn build_container(
        &self,
        project_root: &Path,
        target: &str,
    ) -> Result<(PathBuf, String, Option<PathBuf>)> {
        let (snapshot, officials_root, cargo_target) =
            self.compile_in_container(project_root, target, self.request.reporter.as_ref())?;

        // Stage manifests, assets, AND binaries from the SAME frozen snapshot the
        // container compiled, never the live tree (#936, finding B): the container
        // wrote the target binaries into the project's Cargo target directory,
        // mounted at `<snapshot>/target`, and staging reads
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
            cargo_target,
            Some(officials_root.path().to_path_buf()),
        );
        self.stage_validate_archive(project_root, snapshot.path(), target, &staging)
    }

    /// Snapshot the source, validate its Cargo.lock, resolve the robot graph
    /// to learn its component driver packages, and compile the workspace for
    /// `target` inside the toolchain image - which also materializes the
    /// COMPLETE official set (every catalog service/tool/router plus every
    /// registry-sourced component driver this robot declares) via `cargo
    /// install`, natively on the container's own platform (organization#951
    /// WS4; blocker 2 of the review closes the "component drivers install
    /// host-side, cross-compiled" gap) - returning the snapshot directory,
    /// persistent project Cargo target directory, and the officials root (with
    /// its `bin/` populated the identical way, so
    /// host-side staging never needs to cross-compile anything). Assumes the
    /// The command adapter already holds the Build lock; the rendered engine
    /// invocation remains unit-testable without starting a container.
    fn compile_in_container(
        &self,
        project_root: &Path,
        target: &str,
        ui: &dyn crate::Reporter,
    ) -> Result<(tempfile::TempDir, tempfile::TempDir, PathBuf)> {
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
        let cargo_target =
            crate::build::cargo::cargo_target_dir(project_root, self.request.offline)?;
        std::fs::create_dir_all(&cargo_target).with_context(|| {
            format!(
                "failed to create persistent Cargo target directory {}",
                cargo_target.display()
            )
        })?;

        // The default image is the pinned official rust image with the container
        // platform derived from the target arch; a custom `--builder-image` owns
        // its own toolchain, so we only pass `--platform` when the arch is one we
        // can map.
        let (image, platform) = match &self.image {
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
        let train = phoxal_cli_core::project::train::resolve_locked_train(
            project_root,
            self.request.offline,
        )
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
        let driver_policy = crate::run::report::DriverPolicy::from_options(
            &RunOptions {
                drivers: crate::run::DriversMode::On,
                drivers_subset: Vec::new(),
                offline: self.request.offline,
            },
            &crate::run::report::driven_instances(&robot),
        )?;
        let resolved = crate::progress::run_phase(
            ui,
            crate::PhaseId::new("validate"),
            "Validating robot.yaml",
            || {
                crate::resolve::project::resolve(
                    &robot,
                    snapshot_root,
                    phoxal_cli_core::project::resolver::ResolveOptions {
                        official_target_triple: Some(target.to_string()),
                        tool_target_triple: Some(target.to_string()),
                        drivers: driver_policy.selection(),
                        include_simulators: false,
                        offline: self.request.offline,
                    },
                )
            },
        )
        .context("failed to resolve the robot graph to learn its component driver packages")?;
        officials.extend(component_driver_officials(&resolved));
        let spec = ContainerBuildSpec {
            engine: self.engine,
            image,
            platform,
            target: target.to_string(),
            snapshot: snapshot.path().to_path_buf(),
            cargo_target: cargo_target.clone(),
            cargo_registry,
            cargo_git,
            officials_root: officials_root.path().to_path_buf(),
            officials,
            offline: self.request.offline,
        };
        ui.info(format!(
            "compiling workspace and materializing officials for {target} inside {} ({}{})",
            spec.image,
            self.engine.program(),
            spec.platform
                .as_deref()
                .map(|platform| format!(", {platform}"))
                .unwrap_or_default(),
        ));
        crate::build::container::run_engine(ui, &spec.invocation())?;
        Ok((snapshot, officials_root, cargo_target))
    }

    /// The shared tail every backend runs: refresh staging for the target from
    /// `staging_source` (the live project for `local`, the frozen snapshot for
    /// `container`), validate the staged layout through the loader against the
    /// declared target signature, and archive it deterministically as a sibling
    /// of `project_root`'s staged directory. The command adapter holds the Build
    /// lock for the whole operation.
    fn stage_validate_archive(
        &self,
        project_root: &Path,
        staging_source: &Path,
        target: &str,
        staging: &StagingBuild,
    ) -> Result<(PathBuf, String, Option<PathBuf>)> {
        let options = RunOptions {
            drivers: crate::run::DriversMode::On,
            drivers_subset: Vec::new(),
            offline: self.request.offline,
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
        let staged = crate::run::prepare::refresh_staging(
            staging_source,
            &options,
            staging,
            false,
            RunIdentity::default(),
            self.request.reporter.as_ref(),
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
            .request
            .output
            .clone()
            .unwrap_or_else(|| default_output(project_root, target));
        let digest = crate::bundle::archive::write_build_archive(&staged_root, &output)
            .context("failed to write the deterministic build.phoxal")?;

        Ok((output, digest, Some(staged_root)))
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
    resolved: &phoxal_cli_core::project::resolver::BundlePlan,
) -> Vec<ContainerOfficial> {
    let mut seen_packages = std::collections::BTreeSet::new();
    crate::validation::component_driver_runtimes_by_ref(resolved)
        .into_values()
        .filter(|runtime| seen_packages.insert(runtime.package.clone()))
        .map(|runtime| ContainerOfficial {
            package: phoxal_cli_core::project::catalog::cargo_package_name(&runtime.package),
            train: runtime.train.clone(),
        })
        .collect()
}

fn validate_ssh_target(target: &str) -> Result<()> {
    let Some((user, host)) = target.split_once('@') else {
        bail!("build target must be `user@host`");
    };
    anyhow::ensure!(
        !user.is_empty()
            && !host.is_empty()
            && !host.contains('@')
            && target
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._@:-".contains(&byte)),
        "invalid build target `{target}`; expected `user@host`"
    );
    Ok(())
}

fn require_remote_phoxal(target: &str, reporter: &dyn crate::Reporter) -> Result<()> {
    let output = remote_output(
        target,
        &format!("test -x {REMOTE_PHOXAL} && sudo -n test -x {REMOTE_PHOXAL}"),
        reporter,
    )?;
    anyhow::ensure!(
        output.status.success(),
        "{target} does not have the privileged phoxal binary installed"
    );
    Ok(())
}

fn remote_toolchain_target(target: &str, reporter: &dyn crate::Reporter) -> Result<String> {
    let output = remote_output(
        target,
        &format!(
            "{REMOTE_TOOLCHAIN_PATH}; command -v cargo >/dev/null && command -v rustc >/dev/null && rustc -vV"
        ),
        reporter,
    )?;
    if !output.status.success() {
        let arch = remote_output(target, "uname -m", reporter)
            .ok()
            .filter(|probe| probe.status.success())
            .and_then(|probe| String::from_utf8(probe.stdout).ok())
            .unwrap_or_default();
        let triple = match arch.trim() {
            "aarch64" | "arm64" => "aarch64-unknown-linux-gnu",
            "x86_64" | "amd64" => "x86_64-unknown-linux-gnu",
            _ => "<target-triple>",
        };
        anyhow::bail!(
            "{target} is missing Cargo or rustc; run `phoxal build --target {triple}`, then \
             `phoxal deploy {target} --build <archive>`"
        );
    }
    String::from_utf8(output.stdout)?
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::to_string)
        .context("remote `rustc -vV` did not report a host target triple")
}

fn create_remote_temp(target: &str, reporter: &dyn crate::Reporter) -> Result<String> {
    let output = remote_output(target, "mktemp -d /tmp/phoxal-build.XXXXXX", reporter)?;
    anyhow::ensure!(
        output.status.success(),
        "failed to create remote build directory"
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

fn cleanup_remote_temp(target: &str, path: &str, reporter: &dyn crate::Reporter) -> Result<()> {
    anyhow::ensure!(
        path.starts_with("/tmp/phoxal-build.") && !path.contains(char::is_whitespace),
        "refusing to clean unexpected remote path `{path}`"
    );
    run_remote(
        target,
        &format!("rm -rf -- {}", shell_quote(path)),
        reporter,
    )
}

fn upload_source_payload(
    target: &str,
    project: &Path,
    remote_dir: &str,
    reporter: &dyn crate::Reporter,
) -> Result<()> {
    let snapshot = tempfile::Builder::new()
        .prefix("phoxal-build-source-")
        .tempdir()?;
    snapshot_source(project, snapshot.path())?;
    let archive = tempfile::Builder::new()
        .prefix("phoxal-source-")
        .suffix(".tar.gz")
        .tempfile()?;
    let mut tar = Command::new("tar");
    tar.env("COPYFILE_DISABLE", "1")
        .args(["--no-xattrs", "-czf"])
        .arg(archive.path())
        .arg("-C")
        .arg(snapshot.path())
        .arg(".");
    let status = crate::build::shell::command_status_captured(&mut tar, reporter)?;
    anyhow::ensure!(
        status.success(),
        "source archive creation failed with {status}"
    );
    run_local(
        "scp",
        &[
            "-q",
            archive.path().to_string_lossy().as_ref(),
            &format!("{target}:{remote_dir}/source.tar.gz"),
        ],
        reporter,
    )
}

fn remote_unpack_source_command(remote_dir: &str) -> String {
    let source = format!("{remote_dir}/source");
    format!(
        "set -eu; mkdir {}; tar -xzf {} -C {}",
        shell_quote(&source),
        shell_quote(&format!("{remote_dir}/source.tar.gz")),
        shell_quote(&source)
    )
}

fn run_remote(target: &str, command: &str, reporter: &dyn crate::Reporter) -> Result<()> {
    let output = remote_output(target, command, reporter)?;
    anyhow::ensure!(
        output.status.success(),
        "remote command failed with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn remote_output(
    target: &str,
    command: &str,
    reporter: &dyn crate::Reporter,
) -> Result<std::process::Output> {
    crate::build::shell::run_output(
        "ssh",
        ["-o", "BatchMode=yes", target, command],
        None,
        Some(reporter),
    )
    .with_context(|| format!("failed to run remote command on {target}"))
}

fn run_local(program: &str, args: &[&str], reporter: &dyn crate::Reporter) -> Result<()> {
    let mut command = Command::new(program);
    command.args(args);
    let status = crate::build::shell::command_status_captured(&mut command, reporter)?;
    anyhow::ensure!(status.success(), "{program} failed with {status}");
    Ok(())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
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
    crate::stage::copy_tree_into(source, candidate.path()).with_context(|| {
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
        crate::bundle::archive::BUILD_ARCHIVE_EXTENSION
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
        BundlePlan, ResolvedComponent, ResolvedComponentPackage, ResolvedComponentSource,
        ResolvedPlatformRuntime,
    };

    fn minimal_bundle_plan() -> BundlePlan {
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
        BundlePlan {
            source_manifest: phoxal_manifest::source::robot::parse_from_string(yaml)
                .expect("minimal fixture robot.yaml parses"),
            compiled: Default::default(),
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
        let mut resolved = minimal_bundle_plan();
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
        let backend = select_backend(BuildBackend::Local { target: None }).unwrap();
        assert_eq!(
            backend,
            ResolvedBackend::Local {
                target: crate::resolve::project::host_target_triple()
            }
        );
    }

    #[test]
    fn local_backend_honors_an_explicit_target() {
        let backend = select_backend(BuildBackend::Local {
            target: Some("aarch64-unknown-linux-gnu".to_string()),
        })
        .unwrap();
        assert_eq!(
            backend,
            ResolvedBackend::Local {
                target: "aarch64-unknown-linux-gnu".to_string()
            }
        );
    }

    #[test]
    fn container_backend_defaults_target_to_the_host_linux_gnu_triple() {
        // No `--target`: the container's native triple is the host architecture
        // under Linux (2026-07-24 decision), never the macOS/Windows host triple.
        let backend = select_backend(BuildBackend::Container {
            target: None,
            engine: ContainerEngine::Docker,
            image: None,
        })
        .unwrap();
        assert_eq!(
            backend,
            ResolvedBackend::Container {
                target: host_linux_gnu_triple()
            }
        );
        assert!(host_linux_gnu_triple().ends_with("-unknown-linux-gnu"));

        let ok = select_backend(BuildBackend::Container {
            target: Some("aarch64-unknown-linux-gnu".to_string()),
            engine: ContainerEngine::Docker,
            image: None,
        })
        .unwrap();
        assert_eq!(
            ok,
            ResolvedBackend::Container {
                target: "aarch64-unknown-linux-gnu".to_string()
            }
        );
    }

    #[test]
    fn ssh_backend_retains_the_host_and_optional_target() {
        let backend = select_backend(BuildBackend::Ssh {
            host: "dev@jetson-nano-orin".to_string(),
            target: Some("aarch64-unknown-linux-gnu".to_string()),
        })
        .unwrap();
        assert_eq!(
            backend,
            ResolvedBackend::Ssh {
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
