//! The container builder engine seam (#936).
//!
//! `phoxal build --builder container` compiles the workspace user/driver crates
//! inside a pinned official Docker `rust` image, then hands the built binaries
//! back to the same host-side staging + validation + deterministic archiving
//! every other builder uses. The container is only a compilation environment: it
//! mounts a deterministic source snapshot, the host's Cargo registry/git caches,
//! and the vendored `.phoxal/artifacts`, runs one
//! `cargo build --workspace --locked --target` inside the image, and never
//! produces a Docker/OCI image.
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

/// The default toolchain image: a pinned official Docker Hub `rust` image. The
/// `bookworm` variant ships glibc 2.36; for older devices (e.g. jetson L4T r36,
/// glibc 2.35) override with `--builder-image rust:1.88-bullseye` (glibc 2.31).
/// The `1.88` pin satisfies the workspace MSRV (1.85). See the module docs.
pub const DEFAULT_BUILDER_IMAGE: &str = "rust:1.88-bookworm";

/// The container engine that runs the toolchain image. The engine name is not
/// domain vocabulary - it only selects which CLI drives the image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
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

/// Everything one container build needs: the engine, image, the container
/// platform (arch), target triple, the host source snapshot mounted as the
/// workdir, and the read-through caches. All paths are host paths;
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
    /// Host Cargo registry cache (`$CARGO_HOME/registry`), mounted read-write so
    /// the container reuses already-fetched crates.
    pub cargo_registry: Option<PathBuf>,
    /// Host Cargo git cache (`$CARGO_HOME/git`), mounted read-write.
    pub cargo_git: Option<PathBuf>,
    /// The vendored per-train `.phoxal/artifacts` store, mounted read-only. The
    /// container never fetches officials; staging links them host-side.
    pub artifacts: Option<PathBuf>,
}

/// Where the source snapshot is mounted inside the container.
pub const CONTAINER_WORKDIR: &str = "/phoxal/src";
const CONTAINER_CARGO_REGISTRY: &str = "/usr/local/cargo/registry";
const CONTAINER_CARGO_GIT: &str = "/usr/local/cargo/git";
const CONTAINER_ARTIFACTS: &str = "/phoxal/src/.phoxal/artifacts";

/// A fully rendered engine command: the program (`docker`/`podman`) and its
/// argv. Kept as data so tests can assert it without executing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineInvocation {
    pub program: String,
    pub args: Vec<String>,
}

impl ContainerBuildSpec {
    /// Render the engine invocation that compiles the workspace for the target
    /// inside the image. The container runs, as a compilation environment only,
    /// `cargo build --workspace --locked --target <triple>` against the mounted
    /// snapshot, on the [`Self::platform`] architecture so compilation is native.
    #[must_use]
    pub fn invocation(&self) -> EngineInvocation {
        let mut args = vec!["run".to_string(), "--rm".to_string()];
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
        if let Some(artifacts) = &self.artifacts {
            args.push("-v".to_string());
            args.push(format!(
                "{}:{}:ro",
                artifacts.display(),
                CONTAINER_ARTIFACTS
            ));
        }
        // The image already carries the target toolchain; the build never needs
        // to reach the network for a toolchain, only for crate dependencies the
        // mounted caches usually already hold.
        args.push(self.image.clone());
        args.extend([
            "cargo".to_string(),
            "build".to_string(),
            "--workspace".to_string(),
            "--locked".to_string(),
            "--target".to_string(),
            self.target.clone(),
        ]);
        EngineInvocation {
            program: self.engine.program().to_string(),
            args,
        }
    }
}

/// Runs a rendered [`EngineInvocation`]. Behind a trait so the orchestration is
/// unit-testable with a fake runner that records the invocation instead of
/// spawning a container engine.
pub trait EngineRunner {
    fn run(&self, invocation: &EngineInvocation) -> Result<()>;
}

/// The production runner: shells out to the engine, streaming its output through
/// the session UI exactly like a host `cargo build`.
pub struct ProcessEngineRunner<'a> {
    pub ui: &'a crate::Ui,
}

impl EngineRunner for ProcessEngineRunner<'_> {
    fn run(&self, invocation: &EngineInvocation) -> Result<()> {
        let mut command = Command::new(&invocation.program);
        command.args(&invocation.args);
        let status = self
            .ui
            .command_status_captured(&mut command)
            .with_context(|| {
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

/// The vendored `.phoxal/artifacts` store under `project_root`, when present.
#[must_use]
pub fn vendored_artifacts(project_root: &Path) -> Option<PathBuf> {
    let path = project_root.join(".phoxal").join("artifacts");
    path.is_dir().then_some(path)
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
            cargo_registry: Some(PathBuf::from("/home/dev/.cargo/registry")),
            cargo_git: Some(PathBuf::from("/home/dev/.cargo/git")),
            artifacts: Some(PathBuf::from("/proj/.phoxal/artifacts")),
        }
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
        // Cargo caches mounted read-write; artifacts read-only.
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
            joined.contains(&format!("/proj/.phoxal/artifacts:{CONTAINER_ARTIFACTS}:ro")),
            "{joined}"
        );
        // The pinned official rust image precedes the cargo command.
        assert!(joined.contains("rust:1.88-bookworm"), "{joined}");
        // Native compilation for the target, with the locked lockfile enforced.
        assert!(
            joined.contains("cargo build --workspace --locked --target aarch64-unknown-linux-gnu"),
            "{joined}"
        );
    }

    #[test]
    fn podman_engine_selects_podman() {
        let mut spec = spec();
        spec.engine = ContainerEngine::Podman;
        assert_eq!(spec.invocation().program, "podman");
    }

    #[test]
    fn missing_caches_are_not_mounted() {
        let mut spec = spec();
        spec.cargo_registry = None;
        spec.cargo_git = None;
        spec.artifacts = None;
        let joined = spec.invocation().args.join(" ");
        assert!(!joined.contains(".cargo/registry"), "{joined}");
        assert!(!joined.contains(".cargo/git"), "{joined}");
        assert!(!joined.contains("artifacts"), "{joined}");
        // The snapshot mount and cargo build survive.
        assert!(joined.contains(CONTAINER_WORKDIR), "{joined}");
        assert!(
            joined.contains("cargo build --workspace --locked"),
            "{joined}"
        );
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
            joined.contains("cargo build --workspace --locked"),
            "{joined}"
        );
    }

    /// A fake runner proves the orchestration seam: command construction is
    /// exercised end to end without a container engine present.
    struct RecordingRunner {
        recorded: std::cell::RefCell<Option<EngineInvocation>>,
    }
    impl EngineRunner for RecordingRunner {
        fn run(&self, invocation: &EngineInvocation) -> Result<()> {
            *self.recorded.borrow_mut() = Some(invocation.clone());
            Ok(())
        }
    }

    #[test]
    fn a_recording_runner_captures_the_invocation() {
        let runner = RecordingRunner {
            recorded: std::cell::RefCell::new(None),
        };
        let invocation = spec().invocation();
        runner.run(&invocation).unwrap();
        assert_eq!(runner.recorded.into_inner(), Some(invocation));
    }
}
