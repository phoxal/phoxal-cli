//! Thin `phoxal build` command adapter.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};

use crate::AppContext;
use crate::supervisor::{ProjectLock, ProjectLockIdentity, ProjectOperation};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum ContainerEngine {
    #[default]
    Docker,
    Podman,
}

impl From<ContainerEngine> for phoxal_cli_project::ContainerEngine {
    fn from(value: ContainerEngine) -> Self {
        match value {
            ContainerEngine::Docker => Self::Docker,
            ContainerEngine::Podman => Self::Podman,
        }
    }
}

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
        help = "Rust target triple to build for (e.g. aarch64-unknown-linux-gnu). Defaults to the builder's native triple."
    )]
    pub target: Option<String>,
    #[arg(
        long,
        default_value = "local",
        value_name = "local|container|ssh://user@host",
        help = "Where compilation happens: local, container, or ssh://user@host."
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
        help = "Container engine for --builder container."
    )]
    pub container_engine: ContainerEngine,
    #[arg(
        long,
        value_name = "REF",
        help = "Override the pinned rust:1.88-bookworm toolchain image. For older-glibc devices use rust:1.88-bullseye."
    )]
    pub builder_image: Option<String>,
}

impl Build {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let target =
            phoxal_cli_project::resolve_target(self.project.as_deref(), app.project.root())?;
        // SAFETY: command dispatch has not started worker threads for this verb.
        unsafe {
            std::env::set_var(phoxal_cli_project::PROJECT_ROOT_ENV, &target.logical_root);
        }
        let _lock = ProjectLock::acquire(ProjectLockIdentity::resolve(
            &target.logical_root,
            ProjectOperation::Build,
        ))
        .context("failed to acquire the project lock for build")?;
        let backend = match self.builder.as_str() {
            "local" => phoxal_cli_project::BuildBackend::Local {
                target: self.target.clone(),
            },
            "container" => phoxal_cli_project::BuildBackend::Container {
                target: self.target.clone(),
                engine: self.container_engine.into(),
                image: self.builder_image.clone(),
            },
            value if value.starts_with("ssh://") => {
                let host = value.trim_start_matches("ssh://");
                if host.is_empty() {
                    bail!("`--builder ssh://` needs a user@host");
                }
                phoxal_cli_project::BuildBackend::Ssh {
                    host: host.to_string(),
                    target: self.target.clone(),
                }
            }
            other => bail!(
                "unknown --builder `{other}`; expected `local`, `container`, or `ssh://user@host`"
            ),
        };
        let (reporter, signal_task) = crate::ui::cancellable_preparation_reporter(app.ui);
        let built = phoxal_cli_project::build_bundle(phoxal_cli_project::BuildBundleRequest {
            target,
            backend,
            output: self.output.clone(),
            publish: true,
            offline: app.offline,
            reporter,
        })
        .await;
        signal_task.abort();
        let built = built?;
        app.ui.info(format!(
            "staged runtime layout at {}",
            built
                .staged_root
                .as_ref()
                .context("build did not publish its staged runtime layout")?
                .display()
        ));
        println!("{}", built.archive.display());
        println!("sha256:{}", built.sha256);
        Ok(())
    }
}
