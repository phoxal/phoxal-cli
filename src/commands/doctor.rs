use anyhow::Result;
use async_trait::async_trait;
use clap::{Args, Subcommand};

use phoxal_cli_core::AppContext;
use phoxal_cli_core::Command;
use phoxal_cli_core::unit::Unit;
use phoxal_cli_core::unit::container::BuildPlatform;
use phoxal_cli_core::unit::doctor::{CheckDoctor, DoctorReport, DoctorStatus, FixDoctor};

#[derive(Debug, Args)]
pub(crate) struct Doctor {
    #[arg(
        long,
        help = "Rust target triple checked for image-build prerequisites. Defaults to the local xtask image target."
    )]
    pub(crate) target_triple: Option<String>,

    #[command(subcommand)]
    pub(crate) action: Option<DoctorAction>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum DoctorAction {
    #[command(about = "Install missing doctor prerequisites when xtask knows how to do it.")]
    Fix,
}

#[async_trait(?Send)]
impl Command for Doctor {
    async fn execute(&self, app: &AppContext) -> Result<()> {
        let default_platform = BuildPlatform::local_default();
        let platform = BuildPlatform::new(
            self.target_triple
                .clone()
                .unwrap_or_else(|| default_platform.target_triple.clone()),
            default_platform.docker_platform,
        );

        let report = match self.action.as_ref() {
            Some(DoctorAction::Fix) => FixDoctor {
                platform: platform.clone(),
            }
            .run(app)?,
            None => CheckDoctor { platform }.run(app)?,
        };
        render_report(app, &report);
        Ok(())
    }
}

fn render_report(app: &AppContext, report: &DoctorReport) {
    for item in &report.items {
        match item.status {
            DoctorStatus::Ok => app.ui.success(format!("{}: {}", item.label, item.detail)),
            DoctorStatus::Missing => app.ui.warn(format!("{}: {}", item.label, item.detail)),
        }
    }

    if report.has_missing() {
        app.ui.warn("doctor found missing prerequisites");
    } else {
        app.ui.success("doctor passed");
    }
}
