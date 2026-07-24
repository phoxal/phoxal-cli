//! The container builder engine seam (#936).
//!
//! `phoxal build --builder container` compiles the workspace user/driver crates
//! inside a per-target toolchain image, then hands the built binaries back to the
//! same host-side staging + validation + deterministic archiving every other
//! builder uses. The container is only a compilation environment: it mounts a
//! deterministic source snapshot, the host's Cargo registry/git caches, and the
//! vendored `.phoxal/artifacts`, runs one `cargo build --workspace --target`
//! inside the image, and never produces a Docker/OCI image.
//!
//! The engine invocation is behind a small seam - [`EngineInvocation`] captures
//! exactly the argv, and [`EngineRunner`] executes it - so unit tests assert the
//! command construction (mounts, image ref, env, cargo args) without a container
//! engine installed. The real runner shells out to `docker`/`podman`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

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

/// The default toolchain image for `target`: the cross-rs maintained per-target
/// image. We reuse cross's images without adopting the `cross` tool itself; any
/// custom image overrides this via `--builder-image`.
#[must_use]
pub fn default_builder_image(target: &str) -> String {
    format!("ghcr.io/cross-rs/{target}:latest")
}

/// Everything one container cross-build needs: the engine, image, target triple,
/// the host source snapshot mounted as the workdir, and the read-through caches.
/// All paths are host paths; [`Self::invocation`] renders them into engine mount
/// arguments.
#[derive(Debug, Clone)]
pub struct ContainerBuildSpec {
    pub engine: ContainerEngine,
    pub image: String,
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
    /// `cargo build --workspace --target <triple>` against the mounted snapshot.
    #[must_use]
    pub fn invocation(&self) -> EngineInvocation {
        let mut args = vec![
            "run".to_string(),
            "--rm".to_string(),
            "-w".to_string(),
            CONTAINER_WORKDIR.to_string(),
            "-v".to_string(),
            format!("{}:{}", self.snapshot.display(), CONTAINER_WORKDIR),
        ];
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
            image: default_builder_image("aarch64-unknown-linux-gnu"),
            target: "aarch64-unknown-linux-gnu".to_string(),
            snapshot: PathBuf::from("/tmp/snapshot"),
            cargo_registry: Some(PathBuf::from("/home/dev/.cargo/registry")),
            cargo_git: Some(PathBuf::from("/home/dev/.cargo/git")),
            artifacts: Some(PathBuf::from("/proj/.phoxal/artifacts")),
        }
    }

    #[test]
    fn default_image_is_the_cross_rs_per_target_image() {
        assert_eq!(
            default_builder_image("aarch64-unknown-linux-gnu"),
            "ghcr.io/cross-rs/aarch64-unknown-linux-gnu:latest"
        );
    }

    #[test]
    fn engine_program_names() {
        assert_eq!(ContainerEngine::Docker.program(), "docker");
        assert_eq!(ContainerEngine::Podman.program(), "podman");
        assert_eq!(ContainerEngine::default(), ContainerEngine::Docker);
    }

    #[test]
    fn invocation_mounts_snapshot_caches_and_artifacts_then_builds_the_target() {
        let invocation = spec().invocation();
        assert_eq!(invocation.program, "docker");
        let joined = invocation.args.join(" ");

        // The container runs, is removed after, and works in the snapshot.
        assert!(joined.contains("run --rm"), "{joined}");
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
        // The image ref precedes the cargo command.
        assert!(
            joined.contains("ghcr.io/cross-rs/aarch64-unknown-linux-gnu:latest"),
            "{joined}"
        );
        assert!(
            joined.contains("cargo build --workspace --target aarch64-unknown-linux-gnu"),
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
        assert!(joined.contains("cargo build --workspace"), "{joined}");
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

    /// Real-engine smoke: run a trivial command in the default cross-rs image to
    /// prove the [`ProcessEngineRunner`] wiring drives docker end to end.
    /// Ignored by default - it needs a working docker with network to pull the
    /// image, so it is opt-in (`cargo test -- --ignored real_docker`).
    #[test]
    #[ignore = "requires a working docker engine and network to pull the cross-rs image"]
    fn real_docker_runs_a_container_command() {
        let ui = crate::Ui::from_env();
        let runner = ProcessEngineRunner { ui: &ui };
        let invocation = EngineInvocation {
            program: "docker".to_string(),
            args: vec![
                "run".to_string(),
                "--rm".to_string(),
                default_builder_image("aarch64-unknown-linux-gnu"),
                "true".to_string(),
            ],
        };
        runner
            .run(&invocation)
            .expect("docker should run the toolchain image");
    }
}
