//! System service command adapters.

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::cli::AppContext;

#[derive(Debug, Args)]
pub struct Service {
    #[command(subcommand)]
    pub command: ServiceSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum ServiceSubcommand {
    #[command(about = "Install the one systemd phoxal.service and generic runtime paths.")]
    Install(ServiceInstall),
    #[command(about = "Disable and remove phoxal.service without deleting releases.")]
    Uninstall(ServiceUninstall),
    #[command(about = "Show the live systemd state for phoxal.service.")]
    Status(ServiceStatus),
    #[command(about = "Print official services from the catalog at the project's locked train.")]
    Suite(Suite),
}

#[derive(Debug, Args)]
pub struct ServiceInstall {}

#[derive(Debug, Args)]
pub struct ServiceUninstall {}

#[derive(Debug, Args)]
pub struct ServiceStatus {}

#[derive(Debug, Args)]
pub struct Suite {}

impl Service {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        match &self.command {
            ServiceSubcommand::Install(_) => {
                crate::application::run::service_install_command(app).await
            }
            ServiceSubcommand::Uninstall(_) => {
                crate::application::run::service_uninstall_command(app).await
            }
            ServiceSubcommand::Status(_) => {
                crate::application::run::service_status_command(app).await
            }
            ServiceSubcommand::Suite(_) => {
                crate::application::run::service_suite_command(app).await
            }
        }
    }
}
