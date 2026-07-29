use anyhow::Result;
use clap::{Args, Subcommand};
use phoxal_cli_core::identity::ExecutionId;
use phoxal_cli_core::project::launch_plan::DEFAULT_ROUTER_CONNECT;

use crate::application::attachment::StatusQuery;
use crate::cli::AppContext;

#[derive(Debug, Clone, Args)]
pub struct BusTargetArgs {
    #[arg(long, value_name = "ENDPOINT", default_value = DEFAULT_ROUTER_CONNECT)]
    pub connect: String,
    #[arg(long, value_name = "NAMESPACE")]
    pub namespace: Option<String>,
    #[arg(long = "robot-id", value_name = "ID")]
    pub robot_id: Option<String>,
    #[arg(long, value_name = "EXECUTION", value_parser = parse_execution)]
    pub execution: Option<ExecutionId>,
}

fn parse_execution(value: &str) -> Result<ExecutionId, String> {
    ExecutionId::parse(value).map_err(|error| error.to_string())
}

impl BusTargetArgs {
    pub(crate) fn request(&self) -> crate::application::attachment::BusTargetRequest {
        crate::application::attachment::BusTargetRequest {
            connect: self.connect.clone(),
            namespace: self.namespace.clone(),
            robot_id: self.robot_id.clone(),
            execution: self.execution,
        }
    }
}

#[derive(Debug, Args)]
pub struct Status {
    #[command(subcommand)]
    pub command: StatusSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum StatusSubcommand {
    Safety(BusTargetArgs),
    Motion(BusTargetArgs),
    Localization(BusTargetArgs),
}

impl Status {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let (target, query) = match &self.command {
            StatusSubcommand::Safety(target) => (target, StatusQuery::Safety),
            StatusSubcommand::Motion(target) => (target, StatusQuery::Motion),
            StatusSubcommand::Localization(target) => (target, StatusQuery::Localization),
        };
        crate::application::attachment::status_command(app, &target.request(), query).await
    }
}
