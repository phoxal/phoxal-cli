//! The container builder engine seam.
//!
//! `phoxal build --builder container` compiles the workspace user/driver crates
//! inside a pinned official Docker `rust` image, then hands the built binaries
//! back to the same host-side staging + validation + deterministic archiving
//! every other builder uses. The container is only a compilation environment: it
//! mounts a deterministic source snapshot plus Cargo's persistent target,
//! registry, and git caches, builds the selected packages, and installs every official
//! package in one container invocation. It never produces a Docker/OCI image.
//!
//! ## Officials materialize inside the container too
//!
//! The robot-independent set of official services is installed in the same
//! container invocation, not host-side: the container already runs natively on the target
//! architecture ([`platform_for_triple`]), which is exactly the property that
//! makes cross-compiling from the host risky for a package with any native
//! dependency - the very thing the container exists to avoid for the
//! workspace build. Doing the same for officials keeps that guarantee for
//! the *whole* deterministic set, not just the user's own crates, and
//! reuses the identical package-isolated `cargo install` shape
//! [`crate::build::materialise`] uses everywhere else.
//!
//! On Linux with rootful Docker, container-written entries in the persistent
//! Cargo target directory can be owned by root. Rootless Podman and Docker
//! Desktop map those writes to the invoking user. Operators using rootful
//! Docker should therefore keep the target directory container-owned or
//! correct its ownership before switching back to host-side Cargo commands.
//!
//! Component driver packages are robot-specific (only the components a robot
//! declares), so they are not known from the catalog alone: `phoxal build
//! --builder container` resolves the robot graph, from the same frozen
//! source snapshot the container compiles, BEFORE invoking the container -
//! specifically to learn them - and adds every registry-sourced driver it
//! finds to the same `cargo install` batch the container runs. A container
//! build therefore leaves no official package, including a component driver,
//! to cross-compile host-side. A missing expected official
//! in the container's own output is a hard error, never a silent fallback to
//! a host-side cross-compiled install - see
//! `stager::link_from_officials_source`. A workspace/path-overridden driver
//! is the one thing that still never reaches `cargo install` at all (in the
//! container or on the host): it is staged from its own build output
//! instead, exactly like any other source-overridden participant.
//!
//! ## Offline and proxy plumbing
//!
//! `--offline` threads through EVERY Cargo invocation this module makes: the
//! workspace `cargo build` line as well as the officials' `cargo install`
//! ([`ContainerBuildSpec::container_script`]) - a warm-cache "offline" build
//! must not silently reach the network for one of them and not the other.
//! The container also has no route to the host's own network configuration,
//! so [`ContainerBuildSpec::invocation`] forwards every proxy environment
//! variable ([`PROXY_ENV_VARS`]) the host process actually has set, via
//! `-e VAR=value`; a build behind a corporate/lab proxy would otherwise fail
//! network-side inside the container even though the host has connectivity
//! through it.
//!
//! ## Default image strategy (human-decided 2026-07-24)
//!
//! The default image is the pinned official Docker Hub `rust` image
//! ([`DEFAULT_BUILDER_IMAGE`]), NOT a `cross-rs` per-target image. The cross-rs
//! images ship no Rust toolchain of their own - the `cross` tool bind-mounts the
//! host's Linux toolchain into them, which is structurally impossible from a
//! macOS host. The official `rust` image carries a complete toolchain, so
//! compilation is **native inside the container**: we select the container's
//! CPU architecture with the engine's `--platform` flag (derived from the target
//! triple's arch) and run `cargo build --target <triple>` where the triple
//! matches that platform. No cross toolchain is ever involved - on Apple Silicon
//! an `aarch64` target builds at native speed inside a `linux/arm64` container.
//!
//! **glibc note.** The image's glibc must be **<=** the target device's glibc, or
//! binaries built against a newer glibc fail to load on the device. The default
//! `rust:1.88-bookworm` ships glibc 2.36; a jetson L4T r36 device has glibc 2.35,
//! so for that device pin an older-glibc variant with `--builder-image`
//! (`rust:1.88-bullseye`, glibc 2.31). The pin also satisfies the workspace MSRV
//! (Rust 1.85). `--builder-image` overrides the default entirely - a user-provided
//! image owns its own toolchain and glibc.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::build::shell::shell_quote;

/// The default toolchain image: a pinned official Docker Hub `rust` image. The
/// `bookworm` variant ships glibc 2.36; for older devices (e.g. jetson L4T r36,
/// glibc 2.35) override with `--builder-image rust:1.88-bullseye` (glibc 2.31).
/// The `1.88` pin satisfies the workspace MSRV (1.85). See the module docs.
pub const DEFAULT_BUILDER_IMAGE: &str = "rust:1.88-bookworm";

/// The container engine that runs the toolchain image. The engine name is not
/// domain vocabulary - it only selects which CLI drives the image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContainerEngine {
    #[default]
    Docker,
    Podman,
}

impl ContainerEngine {
    /// The engine binary name on PATH.
    #[must_use]
    pub fn program(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Podman => "podman",
        }
    }
}

/// The default toolchain image: the pinned official Docker `rust` image. Unlike a
/// cross-rs per-target image this carries a full toolchain, so the container
/// compiles natively for the platform [`platform_for_triple`] selects.
#[must_use]
pub fn default_builder_image() -> &'static str {
    DEFAULT_BUILDER_IMAGE
}

/// The OCI `--platform` value for a target triple's architecture: the container
/// runs on that CPU architecture so compilation is native. Only the two
/// architectures the official `rust` image publishes are supported; any other
/// arch yields `None`, and the default-image path rejects it with a precise
/// error (a custom `--builder-image` may still own an exotic toolchain).
#[must_use]
pub fn platform_for_triple(triple: &str) -> Option<&'static str> {
    match triple.split('-').next().unwrap_or("") {
        "aarch64" | "arm64" => Some("linux/arm64"),
        "x86_64" | "amd64" => Some("linux/amd64"),
        _ => None,
    }
}

/// Resolve the engine `--platform` for a default-image container build, rejecting
/// an architecture the official `rust` image does not publish. Used only for the
/// default image; a user-supplied `--builder-image` skips this (it owns its
/// toolchain), passing `--platform` only when the arch is one we can map.
pub fn require_platform_for_triple(triple: &str) -> Result<&'static str> {
    platform_for_triple(triple).with_context(|| {
        format!(
            "`--builder container` cannot select a container platform for target `{triple}`: the \
             default `{DEFAULT_BUILDER_IMAGE}` image only publishes linux/amd64 (x86_64) and \
             linux/arm64 (aarch64). Cross-build to one of those triples, or pass a custom \
             `--builder-image` that carries the toolchain for `{triple}`."
        )
    })
}

/// One explicit Cargo package [`ContainerBuildSpec::invocation`] installs
/// inside the container at an exact train. This includes participant packages
/// and the non-catalog framework supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerPackage {
    pub package: String,
    pub train: String,
}

/// Everything one container build needs: the engine, image, the container
/// platform (arch), target triple, the host source snapshot mounted as the
/// workdir, the read-through caches, and the deterministic official set to
/// materialize alongside the workspace build. All paths are host paths;
/// [`Self::invocation`] renders them into engine arguments.
#[derive(Debug, Clone)]
pub struct ContainerBuildSpec {
    pub engine: ContainerEngine,
    pub image: String,
    /// The OCI `--platform` the container runs on (e.g. `linux/arm64`), so
    /// compilation is native. `None` only for a custom image whose architecture
    /// we cannot map; the default image always resolves one.
    pub platform: Option<String>,
    pub target: String,
    /// Host directory holding the deterministic source snapshot; mounted at
    /// [`CONTAINER_WORKDIR`] and used as the cargo working directory.
    pub snapshot: PathBuf,
    /// The real project's persistent Cargo target root, mounted over the
    /// snapshot's `target/` path. Workspace output stays directly under it;
    /// official installs use distinct `phoxal-install/<package>` children.
    pub cargo_target: PathBuf,
    /// Host Cargo registry cache (`$CARGO_HOME/registry`), mounted read-write so
    /// the container reuses already-fetched crates AND official packages -
    /// `cargo install --registry phoxal` writes into the identical
    /// `$CARGO_HOME/registry` tree as the workspace's own dependencies.
    pub cargo_registry: Option<PathBuf>,
    /// Host Cargo git cache (`$CARGO_HOME/git`), mounted read-write.
    pub cargo_git: Option<PathBuf>,
    /// Host directory `cargo install --root` writes each official's `bin/`
    /// entry into, mounted read-write at [`CONTAINER_OFFICIALS`]. Must exist
    /// and be empty before the container runs; its `bin/` subtree becomes
    /// the container build's `officials_source` for host-side staging.
    pub officials_root: PathBuf,
    /// The complete explicit Cargo-package set installed as one release-profile
    /// batch of package-isolated commands: catalog participants, selected
    /// registry component drivers, and the exact-train framework supervisor.
    /// Empty skips every registry install; the caller then has no
    /// `officials_source` to hand to host-side staging, which materializes
    /// the whole set itself instead.
    pub packages: Vec<ContainerPackage>,
    /// Sorted, deduplicated package names selected from the frozen workspace.
    /// An unselected crate never enters the build command.
    pub workspace_packages: Vec<String>,
    pub offline: bool,
}

/// Where the source snapshot is mounted inside the container.
pub const CONTAINER_WORKDIR: &str = "/phoxal/src";
const CONTAINER_CARGO_REGISTRY: &str = "/usr/local/cargo/registry";
const CONTAINER_CARGO_GIT: &str = "/usr/local/cargo/git";
/// Where official packages are installed inside the container -
/// [`ContainerBuildSpec::officials_root`]'s mount point.
const CONTAINER_OFFICIALS: &str = "/phoxal/officials";

/// Host proxy environment variable names forwarded into the container
/// including both the upper- and lower-case
/// spellings different tools read, so whichever one a host's proxy setup
/// actually exports is carried through.
pub(super) const PROXY_ENV_VARS: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "all_proxy",
];

/// The `-e VAR=value` engine arguments for every [`PROXY_ENV_VARS`] entry
/// present in `env`. Takes an injectable iterator (rather than reading
/// `std::env::vars()` itself) so it is unit-testable without mutating the
/// real, test-shared process environment.
pub(super) fn proxy_args(
    flag: &str,
    env: impl IntoIterator<Item = (String, String)>,
) -> Vec<String> {
    let present = env
        .into_iter()
        .collect::<std::collections::BTreeMap<_, _>>();
    PROXY_ENV_VARS
        .iter()
        .filter_map(|name| present.get(*name).map(|value| (*name, value.as_str())))
        .flat_map(|(name, value)| vec![flag.to_string(), format!("{name}={value}")])
        .collect()
}

fn proxy_engine_args(env: impl IntoIterator<Item = (String, String)>) -> Vec<String> {
    proxy_args("-e", env)
}

/// A fully rendered engine command: the program (`docker`/`podman`) and its
/// argv. Kept as data so tests can assert it without executing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineInvocation {
    pub program: String,
    pub args: Vec<String>,
}

impl ContainerBuildSpec {
    /// Render the engine invocation that compiles selected workspace packages and
    /// materializes the deterministic official set inside the image. The
    /// container runs, as a compilation environment only,
    /// `cargo build --package <selected> ... --locked --release --target <triple>`
    /// against the mounted snapshot, then one `cargo install` per exact
    /// `<package>@<train>`. Registry sources stay shared, while each package's
    /// compiler output is isolated because its published lock graph is
    /// independent. The whole build runs on [`Self::platform`], so it is native
    /// rather than cross-compiled.
    #[must_use]
    pub fn invocation(&self) -> EngineInvocation {
        let mut args = vec!["run".to_string(), "--rm".to_string()];
        if self.offline {
            args.push("--network=none".to_string());
        }
        // Select the container CPU architecture so the toolchain builds natively
        // for the target; the emitted `--target` triple matches this platform.
        if let Some(platform) = &self.platform {
            args.push("--platform".to_string());
            args.push(platform.clone());
        }
        args.extend([
            "-w".to_string(),
            CONTAINER_WORKDIR.to_string(),
            "-v".to_string(),
            format!("{}:{}", self.snapshot.display(), CONTAINER_WORKDIR),
            "-v".to_string(),
            format!("{}:{CONTAINER_WORKDIR}/target", self.cargo_target.display()),
        ]);
        if let Some(registry) = &self.cargo_registry {
            args.push("-v".to_string());
            args.push(format!(
                "{}:{}",
                registry.display(),
                CONTAINER_CARGO_REGISTRY
            ));
        }
        if let Some(git) = &self.cargo_git {
            args.push("-v".to_string());
            args.push(format!("{}:{}", git.display(), CONTAINER_CARGO_GIT));
        }
        if !self.packages.is_empty() {
            args.push("-v".to_string());
            args.push(format!(
                "{}:{}",
                self.officials_root.display(),
                CONTAINER_OFFICIALS
            ));
        }
        // The container has no route to the host's own network configuration
        // otherwise: a build behind
        // a corporate/lab proxy would silently fail network-side inside the
        // container even though the host itself has connectivity through it.
        args.extend(proxy_engine_args(std::env::vars()));
        // The image already carries the target toolchain; the build never needs
        // to reach the network for a toolchain, only for crate dependencies (and
        // official packages) the mounted caches usually already hold.
        args.push(self.image.clone());
        args.extend(["sh".to_string(), "-c".to_string(), self.container_script()]);
        EngineInvocation {
            program: self.engine.program().to_string(),
            args,
        }
    }

    /// The shell script run inside the prepared image: selected workspace
    /// packages, then one package-isolated `cargo install` for each registry
    /// runtime.
    fn container_script(&self) -> String {
        let mut commands = Vec::new();
        if !self.workspace_packages.is_empty() {
            let operands = self
                .workspace_packages
                .iter()
                .map(|package| format!("--package {}", shell_quote(package)))
                .collect::<Vec<_>>()
                .join(" ");
            let mut workspace_build = format!(
                "cargo build {operands} --locked --release --target {} --target-dir {CONTAINER_WORKDIR}/target",
                shell_quote(&self.target),
            );
            if self.offline {
                workspace_build.push_str(" --offline");
            }
            commands.push(workspace_build);
        }
        if !self.packages.is_empty() {
            let shared_target = Path::new(CONTAINER_WORKDIR).join("target");
            for package in &self.packages {
                let install_args = crate::build::materialise::build_install_args(
                    &package.package,
                    &package.train,
                    Some(&self.target),
                    crate::build::materialise::MaterializeProfile::Release,
                    self.offline,
                    // A container builds the published train: a host checkout
                    // is not mounted inside it, so the development overlay
                    // deliberately does not reach here.
                    &crate::build::materialise::MaterializeSource::Registry,
                );
                let package_target =
                    crate::build::materialise::package_target_dir(&shared_target, &package.package);
                let mut command = vec!["cargo".to_string(), "install".to_string()];
                command.extend(install_args);
                command.extend([
                    "--target-dir".to_string(),
                    package_target.display().to_string(),
                ]);
                command.push("--root".to_string());
                command.push(CONTAINER_OFFICIALS.to_string());
                commands.push(
                    command
                        .iter()
                        .map(|arg| shell_quote(arg))
                        .collect::<Vec<_>>()
                        .join(" "),
                );
            }
        }
        if commands.is_empty() {
            "set -e".to_string()
        } else {
            format!("set -e; {}", commands.join("; "))
        }
    }
}

/// Execute a rendered invocation, streaming output through the session UI.
/// Rendering stays separate so every engine argument remains unit-testable
/// without starting a container.
pub fn run_engine(ui: &dyn crate::Reporter, invocation: &EngineInvocation) -> Result<()> {
    let mut command = Command::new(&invocation.program);
    command.args(&invocation.args);
    let status =
        crate::build::shell::command_status_captured(&mut command, ui).with_context(|| {
            format!(
                "failed to start `{}` for the container build; is the engine installed?",
                invocation.program
            )
        })?;
    if !status.success() {
        bail!(
            "container build failed: `{} {}` exited with {status}",
            invocation.program,
            invocation.args.join(" ")
        );
    }
    Ok(())
}

/// Execute an engine query without replaying its machine-readable stdout.
/// Process-group isolation and missing-engine diagnostics match [`run_engine`].
pub fn run_engine_output(invocation: &EngineInvocation) -> Result<std::process::Output> {
    let output = crate::build::shell::run_output(&invocation.program, &invocation.args, None, None)
        .with_context(|| {
            format!(
                "failed to start `{}` for the container build; is the engine installed?",
                invocation.program
            )
        })?;
    if !output.status.success() {
        bail!(
            "container query failed: `{} {}` exited with {}: {}",
            invocation.program,
            invocation.args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output)
}

/// The host Cargo registry and git cache directories, from `$CARGO_HOME` (or the
/// default `~/.cargo`), when they exist. Missing caches are simply not mounted -
/// the container fetches fresh.
#[must_use]
pub fn host_cargo_caches() -> (Option<PathBuf>, Option<PathBuf>) {
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".cargo")));
    let Some(cargo_home) = cargo_home else {
        return (None, None);
    };
    let registry = cargo_home.join("registry");
    let git = cargo_home.join("git");
    (
        registry.is_dir().then_some(registry),
        git.is_dir().then_some(git),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ContainerBuildSpec {
        ContainerBuildSpec {
            engine: ContainerEngine::Docker,
            image: default_builder_image().to_string(),
            platform: Some("linux/arm64".to_string()),
            target: "aarch64-unknown-linux-gnu".to_string(),
            snapshot: PathBuf::from("/tmp/snapshot"),
            cargo_target: PathBuf::from("/workspace/target"),
            cargo_registry: Some(PathBuf::from("/home/dev/.cargo/registry")),
            cargo_git: Some(PathBuf::from("/home/dev/.cargo/git")),
            officials_root: PathBuf::from("/tmp/officials"),
            packages: vec![
                ContainerPackage {
                    package: "phoxal-service-drive".to_string(),
                    train: "0.42.0".to_string(),
                },
                ContainerPackage {
                    package: "phoxal-tool-bus".to_string(),
                    train: "0.41.7".to_string(),
                },
            ],
            workspace_packages: vec!["robot-service".to_string(), "robot-tool".to_string()],
            offline: false,
        }
    }

    #[test]
    fn the_script_contains_no_apt_get() {
        let script = spec().container_script();
        assert!(!script.contains("apt-get"), "{script}");
    }

    #[test]
    fn an_empty_workspace_set_omits_the_build_command() {
        let mut empty = spec();
        empty.workspace_packages.clear();
        assert!(!empty.container_script().contains("cargo build"));
    }

    #[test]
    fn default_image_is_the_pinned_official_rust_image() {
        assert_eq!(default_builder_image(), "rust:1.88-bookworm");
    }

    #[test]
    fn platform_maps_the_two_published_arches_and_rejects_others() {
        assert_eq!(
            platform_for_triple("aarch64-unknown-linux-gnu"),
            Some("linux/arm64")
        );
        assert_eq!(
            platform_for_triple("x86_64-unknown-linux-gnu"),
            Some("linux/amd64")
        );
        // A triple the official rust image does not publish is unmappable.
        assert_eq!(platform_for_triple("riscv64gc-unknown-linux-gnu"), None);
        let error = require_platform_for_triple("riscv64gc-unknown-linux-gnu")
            .expect_err("an unmappable arch must be rejected for the default image");
        assert!(
            error
                .to_string()
                .contains("cannot select a container platform"),
            "{error}"
        );
    }

    #[test]
    fn engine_program_names() {
        assert_eq!(ContainerEngine::Docker.program(), "docker");
        assert_eq!(ContainerEngine::Podman.program(), "podman");
        assert_eq!(ContainerEngine::default(), ContainerEngine::Docker);
    }

    #[test]
    fn invocation_selects_platform_mounts_and_builds_the_target_locked() {
        let invocation = spec().invocation();
        assert_eq!(invocation.program, "docker");
        let joined = invocation.args.join(" ");

        // The container runs, is removed after, on the target's native platform,
        // and works in the snapshot.
        assert!(joined.contains("run --rm"), "{joined}");
        assert!(joined.contains("--platform linux/arm64"), "{joined}");
        assert!(
            joined.contains(&format!("-w {CONTAINER_WORKDIR}")),
            "{joined}"
        );
        // Snapshot mount as the workdir.
        assert!(
            joined.contains(&format!("/tmp/snapshot:{CONTAINER_WORKDIR}")),
            "{joined}"
        );
        assert!(
            joined.contains(&format!("/workspace/target:{CONTAINER_WORKDIR}/target")),
            "{joined}"
        );
        // Cargo caches mounted read-write; the officials root too, since
        // officials materialize inside the container.
        assert!(
            joined.contains(&format!(
                "/home/dev/.cargo/registry:{CONTAINER_CARGO_REGISTRY}"
            )),
            "{joined}"
        );
        assert!(
            joined.contains(&format!("/home/dev/.cargo/git:{CONTAINER_CARGO_GIT}")),
            "{joined}"
        );
        assert!(
            joined.contains(&format!("/tmp/officials:{CONTAINER_OFFICIALS}")),
            "{joined}"
        );
        // The pinned official rust image precedes the shell script.
        assert!(joined.contains("rust:1.88-bookworm"), "{joined}");
        // Native compilation for the target, with the locked lockfile enforced.
        assert!(
            joined.contains("cargo build --package 'robot-service' --package 'robot-tool' --locked --release --target 'aarch64-unknown-linux-gnu' --target-dir /phoxal/src/target"),
            "{joined}"
        );
        // Every official installs, pinned to its exact train, into the
        // mounted officials root, cross-target-explicit like the workspace
        // build.
        assert!(joined.contains("phoxal-service-drive@0.42.0"), "{joined}");
        assert!(joined.contains("phoxal-tool-bus@0.41.7"), "{joined}");
        assert!(joined.contains(CONTAINER_OFFICIALS), "{joined}");
        assert!(joined.contains("--registry"), "{joined}");
        assert!(joined.contains("phoxal"), "{joined}");
        assert!(joined.contains("--locked"), "{joined}");
        assert!(
            joined.contains(
                "'--target-dir' '/phoxal/src/target/phoxal-install/phoxal-service-drive'"
            ),
            "{joined}"
        );
        assert!(
            joined.contains("'--target-dir' '/phoxal/src/target/phoxal-install/phoxal-tool-bus'"),
            "{joined}"
        );
        assert_eq!(
            joined.matches("'cargo' 'install'").count(),
            2,
            "each independently locked official needs its own Cargo invocation: {joined}"
        );
        assert_eq!(joined.matches("--registry").count(), 2, "{joined}");
        assert_eq!(joined.matches("--locked").count(), 3, "{joined}");
        // `set -e` so a failed official install stops the whole script rather
        // than silently continuing with a partial officials root.
        assert!(joined.contains("set -e"), "{joined}");
    }

    #[test]
    fn podman_engine_selects_podman() {
        let mut spec = spec();
        spec.engine = ContainerEngine::Podman;
        assert_eq!(spec.invocation().program, "podman");
    }

    #[test]
    fn offline_runs_the_container_with_no_network() {
        let mut offline = spec();
        offline.offline = true;
        let invocation = offline.invocation();
        assert!(invocation.args.starts_with(&[
            "run".to_string(),
            "--rm".to_string(),
            "--network=none".to_string()
        ]));
        assert!(offline.container_script().contains("--offline"));
    }

    #[test]
    fn missing_caches_are_not_mounted() {
        let mut spec = spec();
        spec.cargo_registry = None;
        spec.cargo_git = None;
        spec.packages = Vec::new();
        let joined = spec.invocation().args.join(" ");
        assert!(!joined.contains(".cargo/registry"), "{joined}");
        assert!(!joined.contains(".cargo/git"), "{joined}");
        assert!(!joined.contains(CONTAINER_OFFICIALS), "{joined}");
        // The snapshot mount and cargo build survive.
        assert!(joined.contains(CONTAINER_WORKDIR), "{joined}");
        assert!(
            joined
                .contains("cargo build --package 'robot-service' --package 'robot-tool' --locked"),
            "{joined}"
        );
    }

    #[test]
    fn no_officials_omits_both_the_mount_and_any_install_command() {
        let mut spec = spec();
        spec.packages = Vec::new();
        let joined = spec.invocation().args.join(" ");
        assert!(!joined.contains("'cargo' 'install'"), "{joined}");
        assert!(!joined.contains(CONTAINER_OFFICIALS), "{joined}");
    }

    #[test]
    fn offline_appends_the_offline_flag_to_every_official_install() {
        let mut spec = spec();
        spec.offline = true;
        let joined = spec.invocation().args.join(" ");
        assert!(joined.contains("--offline"), "{joined}");
    }

    /// The official installs already
    /// threaded `--offline` through; the workspace build line itself did
    /// not, so a warm-cache "offline" container build could still reach the
    /// network for the workspace's own crate dependencies. Asserts the
    /// WORKSPACE BUILD command specifically, not just that `--offline`
    /// appears anywhere in the rendered script (which the officials installs
    /// alone would already satisfy).
    #[test]
    fn offline_appends_the_offline_flag_to_the_workspace_build_itself() {
        let mut spec = spec();
        spec.offline = true;
        let script = spec.container_script();
        let build_line = script
            .split("; ")
            .find(|line| line.starts_with("cargo build"))
            .expect("the selected workspace build is the script's first command");
        assert!(
            build_line.contains("--offline"),
            "workspace build line must itself carry --offline: {build_line}"
        );
    }

    #[test]
    fn online_omits_the_offline_flag_from_the_workspace_build() {
        let spec = spec();
        let script = spec.container_script();
        let build_line = script
            .split("; ")
            .find(|line| line.starts_with("cargo build"))
            .expect("the selected workspace build is the script's first command");
        assert!(!build_line.contains("--offline"), "{build_line}");
    }

    #[test]
    fn proxy_engine_args_forwards_only_the_variables_actually_set() {
        let args = proxy_engine_args(vec![
            (
                "HTTPS_PROXY".to_string(),
                "http://proxy.example:3128".to_string(),
            ),
            ("no_proxy".to_string(), "localhost,127.0.0.1".to_string()),
            // Not a proxy variable - must be ignored entirely.
            ("UNRELATED_VAR".to_string(), "value".to_string()),
        ]);
        assert_eq!(
            args,
            vec![
                "-e".to_string(),
                "HTTPS_PROXY=http://proxy.example:3128".to_string(),
                "-e".to_string(),
                "no_proxy=localhost,127.0.0.1".to_string(),
            ]
        );
    }

    #[test]
    fn proxy_engine_args_is_empty_with_no_proxy_variables_set() {
        assert!(proxy_engine_args(Vec::new()).is_empty());
    }

    #[test]
    fn a_custom_image_without_a_mappable_platform_omits_the_flag() {
        let mut spec = spec();
        spec.image = "my-registry/exotic-toolchain:latest".to_string();
        spec.platform = None;
        let joined = spec.invocation().args.join(" ");
        assert!(!joined.contains("--platform"), "{joined}");
        assert!(
            joined.contains("my-registry/exotic-toolchain:latest"),
            "{joined}"
        );
        assert!(
            joined
                .contains("cargo build --package 'robot-service' --package 'robot-tool' --locked"),
            "{joined}"
        );
    }
}
